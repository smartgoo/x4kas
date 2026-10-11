//! The app's own health over time, for the charts on the Health page's Live view: the
//! process's CPU, memory and disk use (from `sysinfo`), and figures the app already
//! keeps (the index's size, ingest rate and backlog, how far the index trails the node,
//! the node poll's round trip), sampled every [`SAMPLE_EVERY`] by [`start_sampler`]
//! into `App.metrics`. Kept in memory for this run only. Samples are stamped with
//! [`clock_ms`], so a change of the system clock never reorders them.

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::app::{App, ConnectionStatus};
use crate::format::now_ms;

/// How often [`start_sampler`] takes a sample.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// How far back every sample is kept; older ones survive only as [`Bucket`]s.
pub const FINE_SPAN_MS: u64 = 60 * 60 * 1000;
/// The width of a [`Bucket`]: the resolution of the whole run's history.
pub const BUCKET_MS: u64 = 60 * 1000;
/// A sampler tick this late means the app was suspended or stalled: every line breaks.
const STALL: Duration = Duration::from_secs(3);

/// Milliseconds since the epoch by the wall clock at the first call, advanced by a
/// monotonic clock since: it never steps back when the system clock is set.
pub fn clock_ms() -> u64 {
    static ANCHOR: OnceLock<(Instant, u64)> = OnceLock::new();
    let (at, ms) = ANCHOR.get_or_init(|| (Instant::now(), now_ms()));
    ms + at.elapsed().as_millis() as u64
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at_ms: u64,
    pub value: f64,
    /// The line breaks before this sample: none were taken for a while.
    pub new_run: bool,
}

/// What [`Series::points`] returns: the samples, and whether they are per-minute means.
#[derive(Debug, Clone, Default)]
pub struct Points {
    pub samples: Vec<Sample>,
    pub bucketed: bool,
}

/// The samples of one [`BUCKET_MS`] stretch, folded into their mean and peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub start_ms: u64,
    sum: f64,
    count: u32,
    pub max: f64,
}

impl Bucket {
    fn new(sample: Sample) -> Self {
        Self {
            start_ms: sample.at_ms - sample.at_ms % BUCKET_MS,
            sum: sample.value,
            count: 1,
            max: sample.value,
        }
    }

    pub fn mean(&self) -> f64 {
        self.sum / self.count as f64
    }

    /// The bucket as a point at its middle; the line breaks before it when the minute
    /// before it has no bucket.
    fn sample(&self, previous: Option<&Bucket>) -> Sample {
        Sample {
            at_ms: self.start_ms + BUCKET_MS / 2,
            value: self.mean(),
            new_run: previous.is_some_and(|p| p.start_ms + BUCKET_MS < self.start_ms),
        }
    }
}

/// One metric's history: every sample of the last [`FINE_SPAN_MS`], and a [`Bucket`]
/// per [`BUCKET_MS`] since the first sample, so a long run costs a point a minute.
#[derive(Debug, Clone, Default)]
pub struct Series {
    fine: VecDeque<Sample>,
    buckets: Vec<Bucket>,
    /// Whether samples have aged out of `fine`.
    dropped: bool,
    /// The next sample starts a new run (`break_line`).
    broken: bool,
}

impl Series {
    /// Add a sample. One stamped before the latest is moved up to it, so the history
    /// stays in time order.
    pub fn push(&mut self, at_ms: u64, value: f64) {
        if !value.is_finite() {
            return;
        }
        let at_ms = self.fine.back().map_or(at_ms, |s| at_ms.max(s.at_ms));
        let sample = Sample {
            at_ms,
            value,
            new_run: std::mem::take(&mut self.broken),
        };
        match self.buckets.last_mut() {
            Some(b) if at_ms < b.start_ms + BUCKET_MS => {
                b.sum += value;
                b.count += 1;
                b.max = b.max.max(value);
            }
            _ => self.buckets.push(Bucket::new(sample)),
        }
        self.fine.push_back(sample);
        while self
            .fine
            .front()
            .is_some_and(|s| s.at_ms + FINE_SPAN_MS < at_ms)
        {
            self.fine.pop_front();
            self.dropped = true;
        }
    }

    /// Break the line here: there is no sample for this moment.
    pub fn break_line(&mut self) {
        if !self.fine.is_empty() {
            self.broken = true;
        }
    }

    /// Push `value`, or break the line without one.
    pub fn record(&mut self, at_ms: u64, value: Option<f64>) {
        match value {
            Some(v) => self.push(at_ms, v),
            None => self.break_line(),
        }
    }

    pub fn latest(&self) -> Option<Sample> {
        self.fine.back().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.fine.is_empty() && self.buckets.is_empty()
    }

    /// Whether the fine history holds every sample since `from_ms`: nothing has been
    /// dropped from it yet, or it reaches back that far.
    fn fine_covers(&self, from_ms: u64) -> bool {
        !self.dropped || self.fine.front().is_some_and(|s| s.at_ms <= from_ms)
    }

    /// The points to draw since `from_ms`: every sample while the fine history reaches
    /// back that far, else a bucket's mean per minute.
    pub fn points(&self, from_ms: u64) -> Points {
        if self.fine_covers(from_ms) {
            let first = self.fine.partition_point(|s| s.at_ms < from_ms);
            Points {
                samples: self.fine.range(first..).copied().collect(),
                bucketed: false,
            }
        } else {
            let first = self
                .buckets
                .partition_point(|b| b.start_ms + BUCKET_MS <= from_ms);
            let samples = (first..self.buckets.len())
                .map(|i| self.buckets[i].sample(i.checked_sub(1).map(|p| &self.buckets[p])))
                .collect();
            Points {
                samples,
                bucketed: true,
            }
        }
    }

    /// The value nearest `at_ms`, from the history [`Self::points`] draws since
    /// `from_ms`: the closer of the samples on either side, unless the line breaks
    /// between them, where only one within a sample or two (a minute for buckets) of
    /// `at_ms` counts.
    pub fn value_at(&self, from_ms: u64, at_ms: u64) -> Option<f64> {
        if self.fine_covers(from_ms) {
            let after = self.fine.partition_point(|s| s.at_ms <= at_ms);
            let get = |i: usize| self.fine[i];
            nearest(
                after,
                self.fine.len(),
                get,
                at_ms,
                2 * SAMPLE_EVERY.as_millis() as u64,
            )
        } else {
            let after = self
                .buckets
                .partition_point(|b| b.start_ms + BUCKET_MS / 2 <= at_ms);
            let get = |i: usize| self.buckets[i].sample(i.checked_sub(1).map(|p| &self.buckets[p]));
            nearest(after, self.buckets.len(), get, at_ms, BUCKET_MS)
        }
    }

    /// The highest sample since `from_ms`, from the same history as [`Self::points`].
    pub fn max_since(&self, from_ms: u64) -> Option<f64> {
        if self.fine_covers(from_ms) {
            self.fine
                .iter()
                .filter(|s| s.at_ms >= from_ms)
                .map(|s| s.value)
                .reduce(f64::max)
        } else {
            self.buckets
                .iter()
                .filter(|b| b.start_ms + BUCKET_MS > from_ms)
                .map(|b| b.max)
                .reduce(f64::max)
        }
    }
}

/// The value of the sample nearest `at_ms` among `len` sorted samples, the first
/// after it being `after` (see [`Series::value_at`]).
fn nearest(
    after: usize,
    len: usize,
    get: impl Fn(usize) -> Sample,
    at_ms: u64,
    tolerance: u64,
) -> Option<f64> {
    let before = after.checked_sub(1).map(&get);
    let next = (after < len).then(|| get(after));
    match (before, next) {
        (Some(b), Some(n)) if !n.new_run => Some(if at_ms - b.at_ms <= n.at_ms - at_ms {
            b.value
        } else {
            n.value
        }),
        (b, n) => [b, n]
            .into_iter()
            .flatten()
            .filter(|s| s.at_ms.abs_diff(at_ms) <= tolerance)
            .min_by_key(|s| s.at_ms.abs_diff(at_ms))
            .map(|s| s.value),
    }
}

/// Every charted metric. The GUI's frame time is the GUI's own (`GuiApp`), not here.
#[derive(Debug, Clone, Default)]
pub struct HealthMetrics {
    /// When the run started (the sampler's first sample), for the "Run" window.
    pub started_ms: Option<u64>,
    /// The process's CPU use, as a percentage of every core together.
    pub cpu_percent: Series,
    /// The process's resident memory, in bytes.
    pub memory_bytes: Series,
    /// Bytes the process read from and wrote to disk per second.
    pub disk_read_bps: Series,
    pub disk_write_bps: Series,
    /// The index's size on disk (`ChainStatus::disk_bytes`), while one is open.
    pub index_bytes: Series,
    /// Transactions indexed per second, from `ChainStatus::txs_indexed`.
    pub ingest_tps: Series,
    /// Responses waiting for the index writer (`ChainStatus::backlog`).
    pub backlog: Series,
    /// Seconds between the node's sink and the index's position, both chain times.
    pub index_lag_secs: Series,
    /// The node poll's round trip (`NodeState::last_poll_duration_ms`), one per poll.
    pub poll_ms: Series,
    /// The machine's logical cores and memory, for the charts' context.
    pub cores: usize,
    pub total_memory_bytes: u64,
}

impl HealthMetrics {
    /// Break every line: the sampler didn't run for a while.
    fn break_all(&mut self) {
        for series in [
            &mut self.cpu_percent,
            &mut self.memory_bytes,
            &mut self.disk_read_bps,
            &mut self.disk_write_bps,
            &mut self.index_bytes,
            &mut self.ingest_tps,
            &mut self.backlog,
            &mut self.index_lag_secs,
            &mut self.poll_ms,
        ] {
            series.break_line();
        }
    }
}

/// What the sampler carries from one sample to the next to turn totals into rates and
/// take each poll once.
#[derive(Default)]
struct Previous {
    txs_indexed: Option<u64>,
    at: Option<Instant>,
    poll: Option<Instant>,
}

/// The figures `App` already has, sampled at `at_ms` into `metrics`.
fn sample_app(app: &mut App, at_ms: u64, elapsed_secs: Option<f64>, prev: &mut Previous) {
    let chain = &app.chain;
    let indexing = chain.position.is_some() || chain.disk_bytes > 0;
    let metrics = &mut app.metrics;
    // Every series without a value now breaks its line, so a pause isn't drawn across.
    let (bytes, backlog, rate, lag) = if indexing {
        // A total that fell was reset (a resync, a new connection): start over from it.
        let rate = match (prev.txs_indexed, elapsed_secs) {
            (Some(before), Some(secs)) if chain.txs_indexed >= before && secs > 0.0 => {
                Some((chain.txs_indexed - before) as f64 / secs)
            }
            _ => None,
        };
        prev.txs_indexed = Some(chain.txs_indexed);
        let lag = app
            .node
            .sink_timestamp_ms
            .zip(chain.position.as_ref())
            .map(|(sink, position)| sink.saturating_sub(position.time_ms) as f64 / 1000.0);
        (
            Some(chain.disk_bytes as f64),
            Some(chain.backlog as f64),
            rate,
            lag,
        )
    } else {
        prev.txs_indexed = None;
        (None, None, None, None)
    };
    metrics.index_bytes.record(at_ms, bytes);
    metrics.backlog.record(at_ms, backlog);
    metrics.ingest_tps.record(at_ms, rate);
    metrics.index_lag_secs.record(at_ms, lag);
    // A poll is taken once, whenever it lands; the line breaks only while disconnected.
    if !matches!(app.node.connection_status, ConnectionStatus::Connected) {
        metrics.poll_ms.break_line();
    } else if let (Some(at), Some(ms)) = (app.node.last_refresh, app.node.last_poll_duration_ms)
        && prev.poll != Some(at)
    {
        prev.poll = Some(at);
        metrics.poll_ms.push(at_ms, ms);
    }
}

/// Sample the process and `App` every [`SAMPLE_EVERY`] until the returned task is
/// aborted (the controller's shutdown). It doesn't ask for a repaint: the GUI's own
/// once-a-second repaint shows the new samples.
pub fn start_sampler(app: Arc<RwLock<App>>) -> JoinHandle<()> {
    let span = tracing::info_span!("metrics");
    tokio::spawn(
        async move {
            let pid = match sysinfo::get_current_pid() {
                Ok(pid) => Some(pid),
                Err(e) => {
                    tracing::warn!("process metrics unavailable: {e}");
                    None
                }
            };
            let mut system = System::new();
            system.refresh_memory();
            {
                let mut app = app.write().await;
                app.metrics.started_ms = Some(clock_ms());
                app.metrics.cores = std::thread::available_parallelism().map_or(1, |n| n.get());
                app.metrics.total_memory_bytes = system.total_memory();
            }
            let mut prev = Previous::default();
            let mut ticker = tokio::time::interval(SAMPLE_EVERY);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let now = Instant::now();
                let elapsed = prev.at.map(|at| now.duration_since(at).as_secs_f64());
                prev.at = Some(now);
                // Outside the lock: a refresh reads from the OS.
                let process = pid.and_then(|pid| sample_process(&mut system, pid));
                let at_ms = clock_ms();
                let mut app = app.write().await;
                if elapsed.is_some_and(|s| s > STALL.as_secs_f64()) {
                    app.metrics.break_all();
                }
                let m = &mut app.metrics;
                let cores = m.cores.max(1) as f64;
                // The first refresh has nothing to measure CPU against (sysinfo reads 0)
                // and counts the disk since the process started: rates start from the
                // second.
                let rates = process.as_ref().zip(elapsed.filter(|s| *s > 0.0));
                m.cpu_percent
                    .record(at_ms, rates.map(|(p, _)| p.cpu / cores));
                m.memory_bytes
                    .record(at_ms, process.as_ref().map(|p| p.memory as f64));
                m.disk_read_bps
                    .record(at_ms, rates.map(|(p, secs)| p.read as f64 / secs));
                m.disk_write_bps
                    .record(at_ms, rates.map(|(p, secs)| p.written as f64 / secs));
                sample_app(&mut app, at_ms, elapsed, &mut prev);
            }
        }
        .instrument(span),
    )
}

struct ProcessSample {
    /// Percent of one core (above 100 with several busy).
    cpu: f64,
    memory: u64,
    /// Bytes since the previous refresh.
    read: u64,
    written: u64,
}

fn sample_process(system: &mut System, pid: Pid) -> Option<ProcessSample> {
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage()
            .without_tasks(),
    );
    let process = system.process(pid)?;
    let disk = process.disk_usage();
    Some(ProcessSample {
        cpu: process.cpu_usage() as f64,
        memory: process.memory(),
        read: disk.read_bytes,
        written: disk.written_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    #[test]
    fn series_keeps_recent_samples_and_buckets_older_ones() {
        let mut s = Series::default();
        let start = 10 * MIN;
        // Two hours at one sample a minute, valued by the minute.
        for m in 0..120 {
            s.push(start + m * MIN, m as f64);
        }
        assert_eq!(s.latest().unwrap().value, 119.0);
        // The last hour is every sample.
        let recent = s.points(start + 100 * MIN);
        assert!(!recent.bucketed);
        let recent = recent.samples;
        assert_eq!(recent.len(), 20);
        assert_eq!(recent[0].value, 100.0);
        // The whole run reaches past the fine history, so it comes as buckets.
        let all = s.points(start);
        assert!(all.bucketed);
        let all = all.samples;
        assert_eq!(all.len(), 120);
        assert_eq!(all[0].at_ms, start + MIN / 2);
        assert_eq!(s.max_since(start), Some(119.0));
        assert_eq!(s.max_since(start + 100 * MIN), Some(119.0));
    }

    #[test]
    fn bucket_folds_its_minute() {
        let mut s = Series::default();
        for (i, v) in [1.0, 5.0, 3.0].into_iter().enumerate() {
            s.push(MIN + i as u64 * 1000, v);
        }
        s.push(2 * MIN, 10.0);
        assert_eq!(s.buckets.len(), 2);
        assert_eq!(s.buckets[0].mean(), 3.0);
        assert_eq!(s.buckets[0].max, 5.0);
        // Not a number is dropped.
        s.push(2 * MIN + 1000, f64::NAN);
        assert_eq!(s.latest().unwrap().value, 10.0);
    }

    #[test]
    fn app_samples_rates_and_each_poll_once() {
        let mut app = App::default();
        let mut prev = Previous::default();
        app.chain.disk_bytes = 1_000;
        app.chain.txs_indexed = 100;
        sample_app(&mut app, 1_000, None, &mut prev);
        assert!(app.metrics.ingest_tps.is_empty());
        app.chain.txs_indexed = 300;
        sample_app(&mut app, 3_000, Some(2.0), &mut prev);
        assert_eq!(app.metrics.ingest_tps.latest().unwrap().value, 100.0);
        // A reset total gives no rate.
        app.chain.txs_indexed = 5;
        sample_app(&mut app, 4_000, Some(1.0), &mut prev);
        assert_eq!(app.metrics.ingest_tps.points(0).samples.len(), 1);
        assert_eq!(app.metrics.index_bytes.points(0).samples.len(), 3);

        let at = Instant::now();
        app.node.connection_status = ConnectionStatus::Connected;
        app.node.last_refresh = Some(at);
        app.node.last_poll_duration_ms = Some(42.0);
        sample_app(&mut app, 5_000, Some(1.0), &mut prev);
        sample_app(&mut app, 6_000, Some(1.0), &mut prev);
        assert_eq!(app.metrics.poll_ms.points(0).samples.len(), 1);

        // No index: its lines break, and the next sample starts a new run.
        app.chain.disk_bytes = 0;
        sample_app(&mut app, 7_000, Some(1.0), &mut prev);
        app.chain.disk_bytes = 2_000;
        sample_app(&mut app, 8_000, Some(1.0), &mut prev);
        let bytes = app.metrics.index_bytes.points(0).samples;
        assert_eq!(bytes.len(), 6);
        assert!(bytes[5].new_run && !bytes[4].new_run);
    }

    #[test]
    fn earlier_sample_keeps_time_order() {
        let mut s = Series::default();
        s.push(10_000, 1.0);
        s.push(5_000, 2.0);
        let p = s.points(0).samples;
        assert_eq!(p[1].at_ms, 10_000);
    }

    #[test]
    fn value_at_spans_slow_samples_but_not_breaks() {
        let mut s = Series::default();
        // A poll every 5 s: far apart but one line.
        for (i, v) in [10.0, 20.0, 30.0].into_iter().enumerate() {
            s.push(i as u64 * 5_000, v);
        }
        assert_eq!(s.value_at(0, 6_000), Some(20.0));
        assert_eq!(s.value_at(0, 9_000), Some(30.0));
        // After a break, a moment in the gap has no value, unless a sample is close.
        s.break_line();
        s.push(60_000, 40.0);
        assert_eq!(s.value_at(0, 35_000), None);
        assert_eq!(s.value_at(0, 11_000), Some(30.0));
        assert_eq!(s.value_at(0, 59_000), Some(40.0));
        // Past the newest sample by more than a couple of seconds: nothing.
        assert_eq!(s.value_at(0, 70_000), None);
        assert_eq!(s.value_at(0, 61_000), Some(40.0));
    }

    #[test]
    fn missing_minutes_break_the_buckets() {
        let mut s = Series::default();
        // Over an hour of samples, a minute apart, with ten minutes missing.
        for m in (0..30).chain(40..100) {
            s.push(m * MIN, m as f64);
        }
        let p = s.points(0);
        assert!(p.bucketed);
        let gap = p.samples.iter().position(|x| x.new_run).unwrap();
        assert_eq!(p.samples[gap].value, 40.0);
        assert_eq!(p.samples.iter().filter(|x| x.new_run).count(), 1);
        assert_eq!(s.value_at(0, 35 * MIN), None);
        assert_eq!(s.value_at(0, 20 * MIN + 10_000), Some(20.0));
    }
}
