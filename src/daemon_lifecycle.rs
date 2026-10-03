use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;

use crate::app::{App, DaemonStatus};
use crate::config::DaemonConfig;
use crate::daemon;
use crate::rpc::client::RpcManager;

/// Tracks cancellable background polling tasks.
pub struct PollingHandles {
    pub mining: Option<tokio::task::JoinHandle<()>>,
    pub analytics: Option<tokio::task::JoinHandle<()>>,
}

impl PollingHandles {
    pub fn new() -> Self {
        Self {
            mining: None,
            analytics: None,
        }
    }

    pub fn abort_all(&mut self) {
        if let Some(h) = self.mining.take() {
            h.abort();
        }
        if let Some(h) = self.analytics.take() {
            h.abort();
        }
    }
}

pub fn start_mining_polling(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) {
    let rpc_for_mining = rpc.clone();
    let app_for_mining = app.clone();
    handles.mining = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let app_guard = app_for_mining.read().await;
            let is_synced = app_guard.node.server_info.as_ref().is_some_and(|s| s.is_synced);
            let is_paused = app_guard.paused;
            drop(app_guard);
            if !is_paused
                && is_synced
                && let Ok(info) = rpc_for_mining.fetch_mining_info().await
            {
                let mut app = app_for_mining.write().await;
                app.node.mining_info = Some(info);
                app.mark_dirty();
            }
        }
    }));

    // Analytics streaming will be set up separately via start_analytics_streaming()
    handles.analytics = None;
}

/// Create an RPC manager, connect, and start polling.
pub async fn create_and_start_rpc(
    url: Option<String>,
    network: &str,
    app: &Arc<RwLock<App>>,
    refresh_interval_ms: u64,
    retry: bool,
) -> Result<Arc<RpcManager>> {
    let rpc_manager = RpcManager::new(url, network, app.clone()).await?;
    let rpc = Arc::new(rpc_manager);

    let rpc_for_connect = rpc.clone();
    let interval = refresh_interval_ms;
    let app_clone = app.clone();
    tokio::spawn(async move {
        let max_attempts = if retry { 30 } else { 1 };
        for attempt in 0..max_attempts {
            match rpc_for_connect.connect().await {
                Ok(_) => break,
                Err(_) if attempt < max_attempts - 1 => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(_) => break,
            }
        }
        rpc_for_connect.start_polling_shared(Duration::from_millis(interval), app_clone);
    });

    Ok(rpc)
}

pub fn start_log_tailing(config: &DaemonConfig, app: Arc<RwLock<App>>) -> tokio::task::JoinHandle<()> {
    let log_dir = daemon::log_dir(config);
    tokio::spawn(async move {
        // Brief wait for log file to be created
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut last_pos: u64 = 0;
        let mut first_read = true;
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let known_log_path = log_dir.join("rusty-kaspa.log");
        loop {
            ticker.tick().await;

            // Perform all file I/O in a blocking thread to avoid stalling the async runtime
            let path = known_log_path.clone();
            let pos = last_pos;
            let is_first = first_read;
            let read_result = tokio::task::spawn_blocking(move || {
                use std::io::{Read, Seek, SeekFrom};

                let Ok(metadata) = std::fs::metadata(&path) else {
                    return None;
                };
                let file_len = metadata.len();

                let mut effective_pos = pos;
                if is_first {
                    effective_pos = file_len.saturating_sub(65536);
                }

                if file_len <= effective_pos {
                    return Some((String::new(), effective_pos));
                }

                let Ok(mut file) = std::fs::File::open(&path) else {
                    return None;
                };
                let _ = file.seek(SeekFrom::Start(effective_pos));
                let mut buf = String::new();
                let Ok(bytes_read) = file.read_to_string(&mut buf) else {
                    return None;
                };
                Some((buf, effective_pos + bytes_read as u64))
            })
            .await;

            first_read = false;

            let Some(Some((buf, new_pos))) = read_result.ok() else {
                continue;
            };
            last_pos = new_pos;

            if !buf.is_empty() {
                let mut app_guard = app.write().await;
                for line in buf.lines() {
                    if !line.trim().is_empty() {
                        app_guard.integrated_node.log_lines.push_back(line.to_string());
                    }
                }
                // Cap at 1000 lines (O(1) per pop with VecDeque)
                while app_guard.integrated_node.log_lines.len() > 1000 {
                    app_guard.integrated_node.log_lines.pop_front();
                }
                // Auto-scroll to bottom if enabled
                if app_guard.integrated_node.log_auto_scroll {
                    let total = app_guard.integrated_node.log_lines.len();
                    app_guard.integrated_node.log_scroll = total;
                }
                app_guard.mark_dirty();
            }
        }
    })
}

/// Start the daemon, connect RPC, start all polling tasks, and start log tailing.
/// This is the shared logic used by both auto-start and manual DaemonCommand::Start.
pub async fn start_daemon_and_connect(
    config: &DaemonConfig,
    app: &Arc<RwLock<App>>,
    refresh_interval_ms: u64,
    polling_handles: &mut PollingHandles,
) -> Result<(daemon::DaemonHandle, Arc<RpcManager>, tokio::task::JoinHandle<()>)> {
    let handle = daemon::start_daemon(config)?;

    // Wait for wRPC server readiness
    tokio::time::sleep(Duration::from_secs(2)).await;

    let daemon_url = daemon::DaemonHandle::wrpc_borsh_url(&config.network);
    let rpc = create_and_start_rpc(
        Some(daemon_url),
        &config.network,
        app,
        refresh_interval_ms,
        true,
    )
    .await?;

    start_mining_polling(&rpc, app, polling_handles);
    crate::analytics_streaming::start_analytics_streaming(&rpc, app, polling_handles);

    let log_handle = start_log_tailing(config, app.clone());

    {
        let mut app_guard = app.write().await;
        app_guard.integrated_node.status = DaemonStatus::Running;
        app_guard.integrated_node.started_at = Some(std::time::Instant::now());
        app_guard.has_direct_node = true;
    }

    Ok((handle, rpc, log_handle))
}
