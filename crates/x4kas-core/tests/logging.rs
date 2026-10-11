//! The installed logging end to end (its own test binary: `init` sets a process-wide
//! subscriber): a rolling file gets the app's events, the `log` crate's and
//! workflow-log's, and the filter changes while it runs.

use x4kas_core::logging::{self, FileLog, LogConfig, LogLevel, LogSettings};

#[test]
fn file_gets_every_source_and_the_filter_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let config = LogConfig {
        level: LogLevel::Info,
        filter: "workflow=info,bridged=info".to_string(),
        stderr: false,
        file: Some(FileLog {
            dir: dir.path().to_path_buf(),
            prefix: "test",
            keep_files: 2,
        }),
        ..LogConfig::gui(&LogSettings::default())
    };
    let logs = logging::init(&config).unwrap();
    assert!(logs.file().is_some());

    tracing::info!(target: "x4kas::test", "from tracing");
    tracing::info_span!(target: "x4kas::test", "my_span", n = 1)
        .in_scope(|| tracing::info!(target: "x4kas::test", "inside a span"));
    tracing::debug!(target: "x4kas::test", "hidden debug");
    log::info!(target: "bridged", "from the log crate");
    workflow_log::log_info!("from workflow-log");

    logs.handle()
        .set_directives(&logging::directives(LogLevel::Debug, "bridged=debug"))
        .unwrap();
    tracing::debug!(target: "x4kas::test", "shown debug");
    // The `log` macros check `log::max_level` first: a reload raises it too.
    log::debug!(target: "bridged", "log crate debug");
    assert!(logs.handle().set_directives("x4kas=[nonsense").is_err());

    // The buffer has what the filter let through, from every source.
    let buffer = logs
        .buffer()
        .expect("the GUI's configuration keeps a buffer");
    let records = buffer.since(0);
    let find = |text: &str| {
        records
            .iter()
            .find(|r| r.message.contains(text))
            .unwrap_or_else(|| panic!("no {text:?} in {records:#?}"))
    };
    assert_eq!(find("from the log crate").target, "bridged");
    assert_eq!(find("from workflow-log").target, "workflow");
    let in_span = find("inside a span");
    assert_eq!(
        (in_span.spans.as_str(), in_span.level),
        ("my_span", LogLevel::Info)
    );
    assert!(records.iter().all(|r| !r.message.contains("hidden debug")));
    tracing::info!(target: "x4kas::test", count = 3, "with fields");
    assert_eq!(
        buffer.since(0).last().unwrap().message,
        "with fields count=3"
    );

    drop(logs); // flushes the non-blocking writer
    let mut text = String::new();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("test.") && name.ends_with(".log"),
            "{name}"
        );
        text += &std::fs::read_to_string(path).unwrap();
    }
    for line in [
        "from tracing",
        "from the log crate",
        "from workflow-log",
        "shown debug",
        "log crate debug",
        "my_span{n=1}: x4kas::test: inside a span",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert!(!text.contains("hidden debug"), "{text}");
    assert!(!text.contains("\x1b["), "no colors in the file");
}
