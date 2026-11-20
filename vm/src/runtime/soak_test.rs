// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Soak/stress testing framework for the JVM runtime.
//!
//! Detects memory leaks, thread leaks, file descriptor leaks, and performance
//! regressions by running workloads over an extended period and analyzing
//! metric trends via linear regression.

use std::collections::HashMap;
use std::fmt;
use std::io::Write as IoWrite;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

// ---------------------------------------------------------------------------
// SoakError
// ---------------------------------------------------------------------------

/// Errors that can occur during soak testing.
#[derive(Debug)]
pub enum SoakError {
    /// Error during workload setup.
    Setup(String),
    /// Error during workload execution.
    Runtime(String),
    /// Error during post-run analysis.
    Analysis(String),
    /// I/O error (writing reports, etc.).
    Io(std::io::Error),
}

impl fmt::Display for SoakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SoakError::Setup(msg) => write!(f, "soak setup error: {msg}"),
            SoakError::Runtime(msg) => write!(f, "soak runtime error: {msg}"),
            SoakError::Analysis(msg) => write!(f, "soak analysis error: {msg}"),
            SoakError::Io(e) => write!(f, "soak I/O error: {e}"),
        }
    }
}

impl std::error::Error for SoakError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SoakError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SoakError {
    fn from(e: std::io::Error) -> Self {
        SoakError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// SoakTestConfig
// ---------------------------------------------------------------------------

/// Configuration for a soak test run.
#[derive(Debug, Clone)]
pub struct SoakTestConfig {
    /// Total duration of the soak test (excluding warmup).
    pub duration: Duration,
    /// Warmup period where samples are discarded.
    pub warmup_duration: Duration,
    /// How often to sample metrics.
    pub sample_interval: Duration,
    /// Maximum allowed heap growth in percent before flagging a leak.
    pub max_heap_growth_percent: f64,
    /// Maximum thread count increase before flagging a thread leak.
    pub max_thread_leak_count: u32,
    /// Maximum fd count increase before flagging an fd leak.
    pub max_fd_leak_count: u32,
    /// Optional path to write reports to.
    pub report_path: Option<String>,
    /// Time-scaled projection target, in (fractional) days. When set to
    /// `Some(n)`, the runner will run for the configured `duration` but the
    /// post-run report will extrapolate linear trends (heap, thread, fd)
    /// out to `n * 86_400` seconds using the regression slope. This is how
    /// we approximate a 30-day soak from a short run: the regression fit
    /// drives the projection and the scaled growth is compared against
    /// `max_heap_growth_percent`. `None` disables projection (short runs
    /// are evaluated as-is).
    pub days_scaled: Option<f64>,
}

impl Default for SoakTestConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(30 * 60),       // 30 minutes
            warmup_duration: Duration::from_secs(5 * 60), // 5 minutes
            sample_interval: Duration::from_secs(10),     // 10 seconds
            max_heap_growth_percent: 1.0,
            max_thread_leak_count: 2,
            max_fd_leak_count: 5,
            report_path: None,
            days_scaled: None,
        }
    }
}

impl SoakTestConfig {
    /// Build a config that runs for `run_duration` but projects all linear
    /// trends out to `days` simulated days. Useful for treating a 30-minute
    /// run as an estimator for 30-day soak behavior.
    pub fn scaled_run(run_duration: Duration, days: f64) -> Self {
        Self {
            duration: run_duration,
            warmup_duration: Duration::from_millis(0),
            sample_interval: Duration::from_secs(1),
            days_scaled: Some(days),
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// SoakMetricsSample
// ---------------------------------------------------------------------------

/// A single point-in-time snapshot of runtime metrics.
#[derive(Debug, Clone, Serialize)]
pub struct SoakMetricsSample {
    /// Elapsed seconds since the start of the measurement phase.
    pub elapsed_secs: f64,
    /// Heap memory in use (bytes).
    pub heap_used_bytes: u64,
    /// Heap memory committed (bytes).
    pub heap_committed_bytes: u64,
    /// Number of live threads.
    pub thread_count: u32,
    /// Number of open file descriptors.
    pub fd_count: u32,
    /// Cumulative GC cycle count.
    pub gc_count: u64,
    /// Cumulative GC pause time in milliseconds.
    pub gc_pause_total_ms: f64,
    /// CPU usage percentage (0.0-100.0).
    pub cpu_percent: f64,
}

// ---------------------------------------------------------------------------
// WorkloadResult
// ---------------------------------------------------------------------------

/// Result of a single workload iteration.
#[derive(Debug, Clone)]
pub struct WorkloadResult {
    /// Latency of this iteration in nanoseconds.
    pub latency_ns: u64,
    /// Number of allocations made during this iteration.
    pub allocations: u64,
    /// Number of errors encountered.
    pub errors: u64,
}

// ---------------------------------------------------------------------------
// SoakWorkload trait
// ---------------------------------------------------------------------------

/// Trait for workloads that run inside the soak test loop.
pub trait SoakWorkload: Send {
    /// Human-readable workload name.
    fn name(&self) -> &str;

    /// One-time setup before the soak run begins.
    fn setup(&mut self) -> Result<(), SoakError> {
        Ok(())
    }

    /// Execute a single iteration of the workload.
    fn run_iteration(&mut self) -> Result<WorkloadResult, SoakError>;

    /// Cleanup after the soak run completes.
    fn teardown(&mut self) -> Result<(), SoakError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MetricsCollector (trait for platform abstraction / testing)
// ---------------------------------------------------------------------------

/// Collects runtime metrics. Abstracted for testability.
pub trait MetricsCollector: Send {
    fn collect(&self) -> SoakMetricsSample;
}

/// Default metrics collector that uses process-level counters.
pub struct ProcessMetricsCollector {
    start: Instant,
    heap_used: Arc<AtomicU64>,
    heap_committed: Arc<AtomicU64>,
    thread_count: Arc<AtomicU64>,
    fd_count: Arc<AtomicU64>,
    gc_count: Arc<AtomicU64>,
    gc_pause_ms: Arc<AtomicU64>,
}

impl ProcessMetricsCollector {
    /// Create a new collector with the given shared counters.
    pub fn new(
        heap_used: Arc<AtomicU64>,
        heap_committed: Arc<AtomicU64>,
        thread_count: Arc<AtomicU64>,
        fd_count: Arc<AtomicU64>,
        gc_count: Arc<AtomicU64>,
        gc_pause_ms: Arc<AtomicU64>,
    ) -> Self {
        Self {
            start: Instant::now(),
            heap_used,
            heap_committed,
            thread_count,
            fd_count,
            gc_count,
            gc_pause_ms,
        }
    }
}

impl MetricsCollector for ProcessMetricsCollector {
    fn collect(&self) -> SoakMetricsSample {
        SoakMetricsSample {
            elapsed_secs: self.start.elapsed().as_secs_f64(),
            heap_used_bytes: self.heap_used.load(Ordering::Relaxed),
            heap_committed_bytes: self.heap_committed.load(Ordering::Relaxed),
            thread_count: self.thread_count.load(Ordering::Relaxed) as u32,
            fd_count: self.fd_count.load(Ordering::Relaxed) as u32,
            gc_count: self.gc_count.load(Ordering::Relaxed),
            gc_pause_total_ms: self.gc_pause_ms.load(Ordering::Relaxed) as f64,
            cpu_percent: 0.0, // platform-specific; placeholder for portability
        }
    }
}

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------

/// Overall soak test verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    Pass,
    Fail,
    Warning,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Pass => write!(f, "PASS"),
            Verdict::Fail => write!(f, "FAIL"),
            Verdict::Warning => write!(f, "WARNING"),
        }
    }
}

// ---------------------------------------------------------------------------
// SoakReport
// ---------------------------------------------------------------------------

/// Complete report from a soak test run.
#[derive(Debug, Clone, Serialize)]
pub struct SoakReport {
    /// Total wall-clock time of the soak run (excluding warmup).
    pub total_duration_secs: f64,
    /// Total iterations executed across all workloads.
    pub total_iterations: u64,
    /// All collected metric samples.
    pub samples: Vec<SoakMetricsSample>,
    /// Heap growth rate in bytes per second (from linear regression slope).
    pub heap_growth_rate: f64,
    /// Whether a thread leak was detected.
    pub thread_leak_detected: bool,
    /// Whether an fd leak was detected.
    pub fd_leak_detected: bool,
    /// P50 latency in nanoseconds.
    pub p50_latency_ns: u64,
    /// P99 latency in nanoseconds.
    pub p99_latency_ns: u64,
    /// P99.9 latency in nanoseconds.
    pub p999_latency_ns: u64,
    /// Overall verdict.
    pub verdict: Verdict,
    /// List of detected issues.
    pub issues: Vec<String>,
    /// Per-workload iteration counts.
    pub workload_iterations: HashMap<String, u64>,
    /// Per-workload error counts.
    pub workload_errors: HashMap<String, u64>,
    /// Time-scaled projection: present when `days_scaled` is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<ProjectedMetrics>,
}

/// Linear extrapolation of soak metrics out to a target day count.
///
/// Computed by fitting a least-squares line to the observed samples and
/// evaluating it at `days * 86_400` seconds. All fields are the projected
/// value *at the target horizon*, not the rate.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectedMetrics {
    /// The target horizon in days.
    pub target_days: f64,
    /// The target horizon in seconds.
    pub target_seconds: f64,
    /// Projected heap bytes at the target horizon.
    pub projected_heap_bytes: f64,
    /// Projected heap growth as a percentage of initial heap.
    pub projected_heap_growth_percent: f64,
    /// Projected thread count at the target horizon.
    pub projected_thread_count: f64,
    /// Projected file descriptor count at the target horizon.
    pub projected_fd_count: f64,
    /// Projected GC pause total (ms) at the target horizon.
    pub projected_gc_pause_ms: f64,
    /// Heap growth slope used for projection (bytes/sec).
    pub heap_slope_bytes_per_sec: f64,
    /// Thread growth slope (threads/sec).
    pub thread_slope_per_sec: f64,
    /// FD growth slope (fds/sec).
    pub fd_slope_per_sec: f64,
}

impl SoakReport {
    /// Generate a human-readable text report.
    pub fn generate_text_report(&self) -> String {
        let mut out = String::with_capacity(2048);
        out.push_str("=== Soak Test Report ===\n\n");
        out.push_str(&format!("Verdict:          {}\n", self.verdict));
        out.push_str(&format!(
            "Duration:         {:.1}s\n",
            self.total_duration_secs
        ));
        out.push_str(&format!("Iterations:       {}\n", self.total_iterations));
        out.push_str(&format!("Samples:          {}\n", self.samples.len()));
        out.push_str(&format!(
            "Heap growth rate: {:.2} bytes/sec\n",
            self.heap_growth_rate
        ));
        out.push_str(&format!(
            "Thread leak:      {}\n",
            self.thread_leak_detected
        ));
        out.push_str(&format!("FD leak:          {}\n", self.fd_leak_detected));
        out.push_str(&format!("P50 latency:      {} ns\n", self.p50_latency_ns));
        out.push_str(&format!("P99 latency:      {} ns\n", self.p99_latency_ns));
        out.push_str(&format!("P99.9 latency:    {} ns\n", self.p999_latency_ns));

        if let Some(proj) = &self.projection {
            out.push_str("\n--- Projection ---\n");
            out.push_str(&format!(
                "  Horizon:              {:.2} days ({:.0}s)\n",
                proj.target_days, proj.target_seconds
            ));
            out.push_str(&format!(
                "  Projected heap bytes: {:.0}\n",
                proj.projected_heap_bytes
            ));
            out.push_str(&format!(
                "  Projected heap growth: {:.2}%\n",
                proj.projected_heap_growth_percent
            ));
            out.push_str(&format!(
                "  Projected thread cnt: {:.1}\n",
                proj.projected_thread_count
            ));
            out.push_str(&format!(
                "  Projected fd count:   {:.1}\n",
                proj.projected_fd_count
            ));
            out.push_str(&format!(
                "  Projected GC pause:   {:.1} ms\n",
                proj.projected_gc_pause_ms
            ));
        }

        if !self.workload_iterations.is_empty() {
            out.push_str("\n--- Workloads ---\n");
            for (name, count) in &self.workload_iterations {
                let errors = self.workload_errors.get(name).copied().unwrap_or(0);
                out.push_str(&format!("  {name}: {count} iterations, {errors} errors\n"));
            }
        }

        if !self.issues.is_empty() {
            out.push_str("\n--- Issues ---\n");
            for issue in &self.issues {
                out.push_str(&format!("  - {issue}\n"));
            }
        }

        out
    }

    /// Generate a JSON report.
    pub fn generate_json_report(&self) -> Result<String, SoakError> {
        serde_json::to_string_pretty(self).map_err(|e| SoakError::Analysis(e.to_string()))
    }
}

/// Project all monitored metrics to the target horizon by fitting a
/// least-squares line to the samples and evaluating at `days * 86_400`s.
pub fn project_to_days(samples: &[SoakMetricsSample], days: f64) -> ProjectedMetrics {
    let target_seconds = days * 86_400.0;
    let initial_heap = samples
        .first()
        .map(|s| s.heap_used_bytes)
        .unwrap_or(0)
        .max(1);

    let heap_pts: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| (s.elapsed_secs, s.heap_used_bytes as f64))
        .collect();
    let (heap_slope, heap_intercept) = linear_regression(&heap_pts);
    let projected_heap = heap_intercept + heap_slope * target_seconds;
    let projected_heap_growth_pct =
        ((projected_heap - initial_heap as f64) / initial_heap as f64) * 100.0;

    let thread_pts: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| (s.elapsed_secs, s.thread_count as f64))
        .collect();
    let (thread_slope, thread_intercept) = linear_regression(&thread_pts);
    let projected_threads = thread_intercept + thread_slope * target_seconds;

    let fd_pts: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| (s.elapsed_secs, s.fd_count as f64))
        .collect();
    let (fd_slope, fd_intercept) = linear_regression(&fd_pts);
    let projected_fds = fd_intercept + fd_slope * target_seconds;

    let gc_pts: Vec<(f64, f64)> = samples
        .iter()
        .map(|s| (s.elapsed_secs, s.gc_pause_total_ms))
        .collect();
    let (gc_slope, gc_intercept) = linear_regression(&gc_pts);
    let projected_gc_pause = gc_intercept + gc_slope * target_seconds;

    ProjectedMetrics {
        target_days: days,
        target_seconds,
        projected_heap_bytes: projected_heap.max(0.0),
        projected_heap_growth_percent: projected_heap_growth_pct,
        projected_thread_count: projected_threads.max(0.0),
        projected_fd_count: projected_fds.max(0.0),
        projected_gc_pause_ms: projected_gc_pause.max(0.0),
        heap_slope_bytes_per_sec: heap_slope,
        thread_slope_per_sec: thread_slope,
        fd_slope_per_sec: fd_slope,
    }
}

// ---------------------------------------------------------------------------
// Math helpers
// ---------------------------------------------------------------------------

/// Simple linear regression on a set of (x, y) points.
///
/// Returns `(slope, intercept)`. If fewer than 2 points, returns `(0.0, 0.0)`.
pub fn linear_regression(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len();
    if n < 2 {
        return (0.0, 0.0);
    }
    let nf = n as f64;
    let sum_x: f64 = points.iter().map(|(x, _)| x).sum();
    let sum_y: f64 = points.iter().map(|(_, y)| y).sum();
    let sum_xy: f64 = points.iter().map(|(x, y)| x * y).sum();
    let sum_x2: f64 = points.iter().map(|(x, _)| x * x).sum();

    let denom = nf * sum_x2 - sum_x * sum_x;
    if denom.abs() < f64::EPSILON {
        // All x values are the same; slope is undefined.
        return (0.0, sum_y / nf);
    }
    let slope = (nf * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / nf;
    (slope, intercept)
}

/// Compute the p-th percentile of a mutable slice of u64 values.
///
/// `p` is in the range 0.0..=100.0. The slice is sorted in-place.
/// Returns 0 if the slice is empty.
pub fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let p = p.clamp(0.0, 100.0);
    let idx = ((p / 100.0) * (values.len() as f64 - 1.0)).round() as usize;
    let idx = idx.min(values.len() - 1);
    values[idx]
}

// ---------------------------------------------------------------------------
// SoakTestRunner
// ---------------------------------------------------------------------------

/// Orchestrates soak test execution, metric collection, and analysis.
pub struct SoakTestRunner {
    config: SoakTestConfig,
    workloads: Vec<Box<dyn SoakWorkload>>,
    collector: Box<dyn MetricsCollector>,
    stop_flag: Arc<AtomicBool>,
}

impl SoakTestRunner {
    /// Create a new runner with the given configuration and metrics collector.
    pub fn new(config: SoakTestConfig, collector: Box<dyn MetricsCollector>) -> Self {
        Self {
            config,
            workloads: Vec::new(),
            collector,
            stop_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Add a workload to the runner.
    pub fn add_workload(&mut self, workload: Box<dyn SoakWorkload>) {
        self.workloads.push(workload);
    }

    /// Get a clone of the stop flag so external code can signal early termination.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }

    /// Execute the soak test and produce a report.
    pub fn run(mut self) -> Result<SoakReport, SoakError> {
        if self.workloads.is_empty() {
            return Err(SoakError::Setup("no workloads registered".into()));
        }

        // Setup all workloads.
        for w in &mut self.workloads {
            w.setup().map_err(|e| {
                SoakError::Setup(format!("workload '{}' setup failed: {e}", w.name()))
            })?;
        }

        // --- Warmup phase ---
        let warmup_start = Instant::now();
        while warmup_start.elapsed() < self.config.warmup_duration {
            if self.stop_flag.load(Ordering::Relaxed) {
                break;
            }
            for w in &mut self.workloads {
                let _ = w.run_iteration();
            }
        }

        // --- Measurement phase ---
        let measure_start = Instant::now();
        let mut samples: Vec<SoakMetricsSample> = Vec::new();
        let mut latencies: Vec<u64> = Vec::new();
        let mut workload_iterations: HashMap<String, u64> = HashMap::new();
        let mut workload_errors: HashMap<String, u64> = HashMap::new();
        let mut total_iterations: u64 = 0;
        let mut last_sample = Instant::now();

        // Take initial sample.
        samples.push(self.collector.collect());

        while measure_start.elapsed() < self.config.duration {
            if self.stop_flag.load(Ordering::Relaxed) {
                break;
            }

            // Run one iteration of each workload.
            for w in &mut self.workloads {
                match w.run_iteration() {
                    Ok(result) => {
                        latencies.push(result.latency_ns);
                        *workload_iterations.entry(w.name().to_string()).or_insert(0) += 1;
                        *workload_errors.entry(w.name().to_string()).or_insert(0) += result.errors;
                        total_iterations += 1;
                    }
                    Err(e) => {
                        *workload_errors.entry(w.name().to_string()).or_insert(0) += 1;
                        tracing::warn!("workload '{}' iteration error: {e}", w.name());
                    }
                }
            }

            // Collect metrics at the configured interval.
            if last_sample.elapsed() >= self.config.sample_interval {
                samples.push(self.collector.collect());
                last_sample = Instant::now();
            }
        }

        // Final sample.
        samples.push(self.collector.collect());

        // Teardown all workloads.
        for w in &mut self.workloads {
            if let Err(e) = w.teardown() {
                tracing::warn!("workload '{}' teardown error: {e}", w.name());
            }
        }

        // --- Analysis ---
        let total_duration_secs = measure_start.elapsed().as_secs_f64();

        // Heap growth via linear regression.
        let heap_points: Vec<(f64, f64)> = samples
            .iter()
            .map(|s| (s.elapsed_secs, s.heap_used_bytes as f64))
            .collect();
        let (heap_slope, _) = linear_regression(&heap_points);

        // Thread growth detection.
        let first_threads = samples.first().map(|s| s.thread_count).unwrap_or(0);
        let last_threads = samples.last().map(|s| s.thread_count).unwrap_or(0);
        let thread_growth = last_threads.saturating_sub(first_threads);

        // FD growth detection.
        let first_fds = samples.first().map(|s| s.fd_count).unwrap_or(0);
        let last_fds = samples.last().map(|s| s.fd_count).unwrap_or(0);
        let fd_growth = last_fds.saturating_sub(first_fds);

        // Latency percentiles.
        let p50 = percentile(&mut latencies, 50.0);
        let p99 = percentile(&mut latencies, 99.0);
        let p999 = percentile(&mut latencies, 99.9);

        // Heap growth as a percentage of the initial heap.
        let initial_heap = samples
            .first()
            .map(|s| s.heap_used_bytes)
            .unwrap_or(1)
            .max(1);
        let final_heap = samples.last().map(|s| s.heap_used_bytes).unwrap_or(0);
        let observed_heap_growth_pct = if initial_heap > 0 {
            ((final_heap as f64 - initial_heap as f64) / initial_heap as f64) * 100.0
        } else {
            0.0
        };

        // Optional projection: when `days_scaled` is set, extrapolate the
        // linear trends out to the target horizon. The verdict is then
        // evaluated against the *projected* growth — this is what lets a
        // short run flag a 30-day leak.
        let projection = self
            .config
            .days_scaled
            .map(|days| project_to_days(&samples, days));
        let effective_heap_growth_pct = projection
            .as_ref()
            .map(|p| p.projected_heap_growth_percent)
            .unwrap_or(observed_heap_growth_pct);

        // Build issues list and determine verdict.
        let mut issues: Vec<String> = Vec::new();
        let mut verdict = Verdict::Pass;

        if effective_heap_growth_pct > self.config.max_heap_growth_percent {
            if projection.is_some() {
                issues.push(format!(
                    "Projected heap grew {effective_heap_growth_pct:.2}% over {:.2} days (threshold: {:.2}%), slope: {heap_slope:.1} B/s",
                    self.config.days_scaled.unwrap(),
                    self.config.max_heap_growth_percent,
                ));
            } else {
                issues.push(format!(
                    "Heap grew {effective_heap_growth_pct:.2}% (threshold: {:.2}%), slope: {heap_slope:.1} B/s",
                    self.config.max_heap_growth_percent,
                ));
            }
            verdict = Verdict::Fail;
        }

        // For thread/fd leaks with a projection configured, detect leaks
        // against projected growth rather than observed (this is the whole
        // point of time-scaled mode).
        let (effective_thread_growth, effective_fd_growth) = if let Some(p) = &projection {
            let first = samples.first();
            let projected_thread_delta = (p.projected_thread_count
                - first.map(|s| s.thread_count as f64).unwrap_or(0.0))
            .max(0.0)
            .round() as u32;
            let projected_fd_delta = (p.projected_fd_count
                - first.map(|s| s.fd_count as f64).unwrap_or(0.0))
            .max(0.0)
            .round() as u32;
            (projected_thread_delta, projected_fd_delta)
        } else {
            (thread_growth, fd_growth)
        };

        let thread_leak_detected = effective_thread_growth > self.config.max_thread_leak_count;
        if thread_leak_detected {
            if projection.is_some() {
                issues.push(format!(
                    "Projected thread count grows by {effective_thread_growth} over {:.2} days (threshold: {})",
                    self.config.days_scaled.unwrap(),
                    self.config.max_thread_leak_count,
                ));
            } else {
                issues.push(format!(
                    "Thread count grew by {effective_thread_growth} (threshold: {})",
                    self.config.max_thread_leak_count,
                ));
            }
            verdict = Verdict::Fail;
        }

        let fd_leak_detected = effective_fd_growth > self.config.max_fd_leak_count;
        if fd_leak_detected {
            if projection.is_some() {
                issues.push(format!(
                    "Projected FD count grows by {effective_fd_growth} over {:.2} days (threshold: {})",
                    self.config.days_scaled.unwrap(),
                    self.config.max_fd_leak_count,
                ));
            } else {
                issues.push(format!(
                    "FD count grew by {effective_fd_growth} (threshold: {})",
                    self.config.max_fd_leak_count,
                ));
            }
            verdict = Verdict::Fail;
        }

        // Warn if any workload had errors.
        let total_errors: u64 = workload_errors.values().sum();
        if total_errors > 0 && verdict == Verdict::Pass {
            verdict = Verdict::Warning;
            issues.push(format!("{total_errors} total workload errors"));
        }

        let report = SoakReport {
            total_duration_secs,
            total_iterations,
            samples,
            heap_growth_rate: heap_slope,
            thread_leak_detected,
            fd_leak_detected,
            p50_latency_ns: p50,
            p99_latency_ns: p99,
            p999_latency_ns: p999,
            verdict,
            issues,
            workload_iterations,
            workload_errors,
            projection,
        };

        // Write report if a path was configured.
        if let Some(ref path) = self.config.report_path {
            let text = report.generate_text_report();
            let mut f = std::fs::File::create(path)?;
            f.write_all(text.as_bytes())?;
        }

        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Built-in workloads
// ---------------------------------------------------------------------------

/// Stress-tests the allocator by allocating and dropping vectors of varying sizes.
pub struct AllocatorStressWorkload {
    iteration: u64,
    /// Retained allocations to simulate long-lived objects.
    retained: Vec<Vec<u8>>,
    /// Maximum number of retained allocations.
    max_retained: usize,
}

impl AllocatorStressWorkload {
    pub fn new(max_retained: usize) -> Self {
        Self {
            iteration: 0,
            retained: Vec::new(),
            max_retained,
        }
    }
}

impl SoakWorkload for AllocatorStressWorkload {
    fn name(&self) -> &str {
        "allocator_stress"
    }

    fn run_iteration(&mut self) -> Result<WorkloadResult, SoakError> {
        let start = Instant::now();
        self.iteration += 1;

        // Allocate a buffer whose size varies with the iteration.
        let size = 64 + (self.iteration as usize % 4096);
        let buf = vec![0xABu8; size];

        // Retain some allocations to simulate leaky/long-lived patterns.
        if self.retained.len() < self.max_retained {
            self.retained.push(buf);
        } else {
            // Rotate: drop oldest, push new.
            let idx = self.iteration as usize % self.max_retained;
            self.retained[idx] = buf;
        }

        // Also do some short-lived allocations.
        for i in 0..10 {
            let tmp = vec![i as u8; 128 + i * 32];
            std::hint::black_box(&tmp);
        }

        let latency_ns = start.elapsed().as_nanos() as u64;
        Ok(WorkloadResult {
            latency_ns,
            allocations: 11, // 1 main + 10 short-lived
            errors: 0,
        })
    }

    fn teardown(&mut self) -> Result<(), SoakError> {
        self.retained.clear();
        Ok(())
    }
}

/// Stress-tests thread creation and joining.
pub struct ThreadChurnWorkload {
    threads_per_iteration: usize,
}

impl ThreadChurnWorkload {
    pub fn new(threads_per_iteration: usize) -> Self {
        Self {
            threads_per_iteration,
        }
    }
}

impl SoakWorkload for ThreadChurnWorkload {
    fn name(&self) -> &str {
        "thread_churn"
    }

    fn run_iteration(&mut self) -> Result<WorkloadResult, SoakError> {
        let start = Instant::now();
        let mut handles = Vec::with_capacity(self.threads_per_iteration);
        let mut errors = 0u64;

        for i in 0..self.threads_per_iteration {
            let work = i;
            match std::thread::Builder::new()
                .name(format!("soak-thread-{work}"))
                .stack_size(64 * 1024) // small stack to be lightweight
                .spawn(move || {
                    // Do some trivial computation so the thread isn't empty.
                    let mut acc: u64 = 0;
                    for j in 0..1000 {
                        acc = acc.wrapping_add(j);
                    }
                    std::hint::black_box(acc);
                }) {
                Ok(h) => handles.push(h),
                Err(_) => errors += 1,
            }
        }

        for h in handles {
            if h.join().is_err() {
                errors += 1;
            }
        }

        let latency_ns = start.elapsed().as_nanos() as u64;
        Ok(WorkloadResult {
            latency_ns,
            allocations: 0,
            errors,
        })
    }
}

/// Stress-tests hash map operations (insert, lookup, remove).
pub struct HashMapWorkload {
    map: HashMap<u64, Vec<u8>>,
    iteration: u64,
    map_capacity: usize,
}

impl HashMapWorkload {
    pub fn new(map_capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(map_capacity),
            iteration: 0,
            map_capacity,
        }
    }
}

impl SoakWorkload for HashMapWorkload {
    fn name(&self) -> &str {
        "hashmap_stress"
    }

    fn run_iteration(&mut self) -> Result<WorkloadResult, SoakError> {
        let start = Instant::now();
        self.iteration += 1;

        let base = self.iteration.wrapping_mul(6364136223846793005);

        // Insert entries.
        for i in 0..50 {
            let key = base.wrapping_add(i);
            let val = vec![(key & 0xFF) as u8; 64];
            self.map.insert(key, val);
        }

        // Lookup entries.
        let mut found = 0u64;
        for i in 0..50 {
            let key = base.wrapping_add(i);
            if self.map.get(&key).is_some() {
                found += 1;
            }
        }
        std::hint::black_box(found);

        // Remove entries to keep bounded size.
        if self.map.len() > self.map_capacity {
            let keys_to_remove: Vec<u64> = self.map.keys().take(50).copied().collect();
            for k in keys_to_remove {
                self.map.remove(&k);
            }
        }

        let latency_ns = start.elapsed().as_nanos() as u64;
        Ok(WorkloadResult {
            latency_ns,
            allocations: 50, // 50 vec allocations
            errors: 0,
        })
    }

    fn teardown(&mut self) -> Result<(), SoakError> {
        self.map.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
