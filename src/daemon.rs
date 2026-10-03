use std::sync::Arc;

use anyhow::{Result, bail};
use kaspa_core::core::Core;
use kaspa_core::signals::Shutdown;
use kaspa_rpc_service::service::RpcCoreService;
use kaspa_utils::fd_budget;
use kaspa_wrpc_server::address::WrpcNetAddress;
use kaspad_lib::args::Args as KaspadArgs;
use kaspad_lib::daemon::{
    DESIRED_DAEMON_SOFT_FD_LIMIT, MINIMUM_DAEMON_SOFT_FD_LIMIT, Runtime, create_core_with_runtime,
    validate_args,
};
use log::LevelFilter;
use log4rs::Config;
use log4rs::append::rolling_file::RollingFileAppender;
use log4rs::append::rolling_file::policy::compound::CompoundPolicy;
use log4rs::append::rolling_file::policy::compound::roll::fixed_window::FixedWindowRoller;
use log4rs::append::rolling_file::policy::compound::trigger::size::SizeTrigger;
use log4rs::config::{Appender, Root};
use log4rs::encode::pattern::PatternEncoder;

use crate::config::DaemonConfig;

pub struct DaemonHandle {
    core: Arc<Core>,
    rpc_core_service: Option<Arc<RpcCoreService>>,
    workers: Option<Vec<std::thread::JoinHandle<()>>>,
}

impl DaemonHandle {
    /// Returns the wRPC Borsh URL for the given network.
    pub fn wrpc_borsh_url(network: &str) -> String {
        let port = match network {
            "mainnet" => 17110,
            "testnet-10" => 17210,
            "testnet-11" => 17210,
            _ => 17110,
        };
        format!("ws://127.0.0.1:{}", port)
    }

    /// Gracefully shut down the daemon.
    /// IMPORTANT: RpcCoreService must be dropped before Core is joined.
    pub fn shutdown(&mut self) {
        // Drop RPC service first (required by kaspad API contract)
        self.rpc_core_service.take();

        // Signal all services to stop
        self.core.shutdown();

        // Wait for all worker threads to finish
        if let Some(workers) = self.workers.take() {
            self.core.join(workers);
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if self.workers.is_some() {
            self.shutdown();
        }
    }
}

fn parse_network(network: &str) -> (bool, u32, bool, bool) {
    // Returns (testnet, testnet_suffix, devnet, simnet)
    match network {
        "mainnet" => (false, 10, false, false),
        "testnet-10" => (true, 10, false, false),
        "testnet-11" => (true, 11, false, false),
        _ => (false, 10, false, false),
    }
}

fn build_kaspad_args(config: &DaemonConfig) -> KaspadArgs {
    let (testnet, testnet_suffix, devnet, simnet) = parse_network(&config.network);

    // Parse peer lists from comma-separated strings
    let connect_peers = DaemonConfig::parse_peers(&config.connect_peers)
        .into_iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    let add_peers = DaemonConfig::parse_peers(&config.add_peers)
        .into_iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    KaspadArgs {
        appdir: Some(config.app_dir.clone()),
        utxoindex: config.utxo_index,
        archival: config.archival,
        ram_scale: config.ram_scale,
        testnet,
        testnet_suffix,
        devnet,
        simnet,
        // Enable wRPC Borsh server on default port
        rpclisten_borsh: Some(WrpcNetAddress::Default),
        // Disable gRPC and JSON wRPC (the app only uses Borsh wRPC)
        disable_grpc: true,
        rpclisten_json: None,
        // Non-interactive mode: auto-approve DB resets (there is no stdin prompt in the GUI)
        yes: true,
        // Enable log files for the Node tab log viewer
        no_log_files: false,
        log_level: config.log_level.clone(),
        async_threads: config.async_threads,
        // Networking
        listen: config.listen.as_ref().and_then(|s| s.parse().ok()),
        externalip: config.externalip.as_ref().and_then(|s| s.parse().ok()),
        outbound_target: config.outbound_target,
        inbound_limit: config.inbound_limit,
        connect_peers,
        add_peers,
        disable_upnp: config.disable_upnp,
        disable_dns_seeding: config.disable_dns_seed,
        // Storage
        reset_db: config.reset_db,
        rpc_max_clients: config.rpc_max_clients,
        rocksdb_preset: if config.rocksdb_preset.is_empty() || config.rocksdb_preset == "default" {
            None
        } else {
            Some(config.rocksdb_preset.clone())
        },
        rocksdb_wal_dir: config.rocksdb_wal_dir.clone(),
        rocksdb_cache_size: config.rocksdb_cache_size,
        retention_period_days: config.retention_period_days,
        // Performance
        perf_metrics: config.perf_metrics,
        ..KaspadArgs::default()
    }
}

/// Initialize log4rs with file-only appenders (the Node tab tails these files).
/// Uses the same log file format and rolling policy as kaspad's built-in logger.
fn init_file_logger(log_dir_path: &std::path::Path, log_level: &str) {
    std::fs::create_dir_all(log_dir_path).ok();

    let log_pattern = "{d(%Y-%m-%d %H:%M:%S%.3f%:z)} [{({l}):5.5}] {m}{n}";
    let max_size: u64 = 100_000_000; // 100 MB
    let max_rolls: u32 = 8;

    let Some(archive_str) = log_dir_path
        .join("rusty-kaspa.log.{}.gz")
        .to_str()
        .map(|s| s.to_string())
    else {
        return;
    };
    let Some(err_archive_str) = log_dir_path
        .join("rusty-kaspa_err.log.{}.gz")
        .to_str()
        .map(|s| s.to_string())
    else {
        return;
    };

    // Main log file
    let log_file = log_dir_path.join("rusty-kaspa.log");
    let trigger = Box::new(SizeTrigger::new(max_size));
    let Ok(roller) = FixedWindowRoller::builder()
        .base(1)
        .build(&archive_str, max_rolls)
    else {
        return;
    };
    let policy = Box::new(CompoundPolicy::new(trigger, Box::new(roller)));
    let Ok(log_appender) = RollingFileAppender::builder()
        .encoder(Box::new(PatternEncoder::new(log_pattern)))
        .build(log_file, policy)
    else {
        return;
    };

    // Error log file (WARN and above)
    let err_file = log_dir_path.join("rusty-kaspa_err.log");
    let err_trigger = Box::new(SizeTrigger::new(max_size));
    let Ok(err_roller) = FixedWindowRoller::builder()
        .base(1)
        .build(&err_archive_str, max_rolls)
    else {
        return;
    };
    let err_policy = Box::new(CompoundPolicy::new(err_trigger, Box::new(err_roller)));
    let Ok(err_appender) = RollingFileAppender::builder()
        .encoder(Box::new(PatternEncoder::new(log_pattern)))
        .build(err_file, err_policy)
    else {
        return;
    };

    let level = match log_level.to_uppercase().as_str() {
        "TRACE" => LevelFilter::Trace,
        "DEBUG" => LevelFilter::Debug,
        "INFO" => LevelFilter::Info,
        "WARN" => LevelFilter::Warn,
        "ERROR" => LevelFilter::Error,
        _ => LevelFilter::Info,
    };

    let Ok(config) = Config::builder()
        .appender(Appender::builder().build("log_file", Box::new(log_appender)))
        .appender(
            Appender::builder()
                .filter(Box::new(log4rs::filter::threshold::ThresholdFilter::new(
                    LevelFilter::Warn,
                )))
                .build("err_log_file", Box::new(err_appender)),
        )
        .build(
            Root::builder()
                .appender("log_file")
                .appender("err_log_file")
                .build(level),
        )
    else {
        return;
    };

    // Use init_config (not init_config with handle) — if logger was already set, ignore
    let _ = log4rs::init_config(config);
}

/// Start the embedded kaspa daemon.
/// Returns a handle that must be kept alive for the daemon to run.
pub fn start_daemon(config: &DaemonConfig) -> Result<DaemonHandle> {
    let args = build_kaspad_args(config);

    // Validate args before proceeding
    if let Err(e) = validate_args(&args) {
        bail!("Invalid daemon configuration: {}", e);
    }

    // Set file descriptor limits
    match fd_budget::try_set_fd_limit(DESIRED_DAEMON_SOFT_FD_LIMIT) {
        Ok(limit) => {
            if limit < MINIMUM_DAEMON_SOFT_FD_LIMIT {
                eprintln!(
                    "Warning: FD limit ({}) is below recommended minimum ({})",
                    limit, DESIRED_DAEMON_SOFT_FD_LIMIT
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: Could not set FD limit: {}", e);
        }
    }

    let fd_total_budget = fd_budget::limit()
        - args.rpc_max_clients as i32
        - args.inbound_limit as i32
        - args.outbound_target as i32;

    // Initialize file-only logger before creating the core.
    // We skip kaspad's built-in logger (which also writes to stdout)
    // and instead set up log4rs with only file appenders.
    let log_dir_path = log_dir(config);
    init_file_logger(&log_dir_path, &config.log_level);

    // Create runtime manually to avoid kaspad's logger/panic hook initialization
    // which would interfere with the host app
    let runtime = Runtime::default();

    let (core, rpc_core_service) = create_core_with_runtime(&runtime, &args, fd_total_budget);

    // Start all services (non-blocking - returns worker thread handles)
    let workers = core.start();

    Ok(DaemonHandle {
        core,
        rpc_core_service: Some(rpc_core_service),
        workers: Some(workers),
    })
}

/// Returns the log directory path for the given config.
pub fn log_dir(config: &DaemonConfig) -> std::path::PathBuf {
    let network_dir = match config.network.as_str() {
        "testnet-10" => "kaspa-testnet-10",
        "testnet-11" => "kaspa-testnet-11",
        _ => "kaspa-mainnet",
    };
    std::path::PathBuf::from(&config.app_dir)
        .join(network_dir)
        .join("logs")
}
