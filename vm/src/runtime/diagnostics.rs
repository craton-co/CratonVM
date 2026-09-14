// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Runtime diagnostics support.
//!
//! Provides VM diagnostic counters, health checks, and runtime metrics
//! for monitoring and troubleshooting the JVM.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// VM Diagnostic Counters
// ---------------------------------------------------------------------------

/// Global diagnostic counters for the VM runtime.
pub struct DiagnosticCounters {
    /// Total number of bytecodes executed.
    pub bytecodes_executed: AtomicU64,
    /// Total number of method invocations.
    pub method_invocations: AtomicU64,
    /// Total number of exceptions thrown.
    pub exceptions_thrown: AtomicU64,
    /// Total number of classes loaded.
    pub classes_loaded: AtomicU64,
    /// Total number of classes unloaded.
    pub classes_unloaded: AtomicU64,
    /// Total number of GC cycles completed.
    pub gc_cycles: AtomicU64,
    /// Total GC pause time in microseconds.
    pub gc_pause_us: AtomicU64,
    /// Total bytes allocated on the heap.
    pub heap_bytes_allocated: AtomicU64,
    /// Total number of monitor contentions (threads that had to wait).
    pub monitor_contentions: AtomicU64,
    /// Total number of threads created.
    pub threads_created: AtomicU64,
    /// Total number of JIT compilations.
    pub jit_compilations: AtomicU64,
    /// Total number of JIT deoptimizations.
    pub jit_deoptimizations: AtomicU64,
    /// Total number of safepoint pauses.
    pub safepoints: AtomicU64,
    /// Total safepoint pause time in microseconds.
    pub safepoint_pause_us: AtomicU64,
    /// VM start time.
    start_time: Instant,
}

impl DiagnosticCounters {
    /// Create a new set of diagnostic counters, initialized to zero.
    pub fn new() -> Self {
        Self {
            bytecodes_executed: AtomicU64::new(0),
            method_invocations: AtomicU64::new(0),
            exceptions_thrown: AtomicU64::new(0),
            classes_loaded: AtomicU64::new(0),
            classes_unloaded: AtomicU64::new(0),
            gc_cycles: AtomicU64::new(0),
            gc_pause_us: AtomicU64::new(0),
            heap_bytes_allocated: AtomicU64::new(0),
            monitor_contentions: AtomicU64::new(0),
            threads_created: AtomicU64::new(0),
            jit_compilations: AtomicU64::new(0),
            jit_deoptimizations: AtomicU64::new(0),
            safepoints: AtomicU64::new(0),
            safepoint_pause_us: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    /// Get VM uptime in seconds.
    pub fn uptime_secs(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }

    /// Increment a counter by 1.
    #[inline]
    pub fn inc(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Add a value to a counter.
    #[inline]
    pub fn add(&self, counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::Relaxed);
    }

    /// Read a counter's current value.
    #[inline]
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    /// Generate a summary report of all counters.
    pub fn summary(&self) -> DiagnosticSummary {
        DiagnosticSummary {
            uptime_secs: self.uptime_secs(),
            bytecodes_executed: Self::get(&self.bytecodes_executed),
            method_invocations: Self::get(&self.method_invocations),
            exceptions_thrown: Self::get(&self.exceptions_thrown),
            // Served by the class loader's own counter, not by this struct's
            // `classes_loaded` cell. That cell was incremented by nothing and
            // therefore reported `Classes loaded: 0` on every run — which reads
            // as a measurement rather than as a missing instrument, and is why
            // "what is still defining classes in steady state?" went unasked
            // long enough to become a line item on the H2 throughput page.
            // `define_census::total` counts DEFINITIONS at the single choke
            // point every `define_class*` entry funnels through.
            classes_loaded: cratonvm_classloading::define_census::total(),
            classes_unloaded: Self::get(&self.classes_unloaded),
            gc_cycles: Self::get(&self.gc_cycles),
            gc_pause_us: Self::get(&self.gc_pause_us),
            heap_bytes_allocated: Self::get(&self.heap_bytes_allocated),
            monitor_contentions: Self::get(&self.monitor_contentions),
            threads_created: Self::get(&self.threads_created),
            jit_compilations: Self::get(&self.jit_compilations),
            jit_deoptimizations: Self::get(&self.jit_deoptimizations),
            safepoints: Self::get(&self.safepoints),
            safepoint_pause_us: Self::get(&self.safepoint_pause_us),
        }
    }

    /// Reset all counters to zero (does not reset uptime).
    pub fn reset(&self) {
        self.bytecodes_executed.store(0, Ordering::Relaxed);
        self.method_invocations.store(0, Ordering::Relaxed);
        self.exceptions_thrown.store(0, Ordering::Relaxed);
        self.classes_loaded.store(0, Ordering::Relaxed);
        self.classes_unloaded.store(0, Ordering::Relaxed);
        self.gc_cycles.store(0, Ordering::Relaxed);
        self.gc_pause_us.store(0, Ordering::Relaxed);
        self.heap_bytes_allocated.store(0, Ordering::Relaxed);
        self.monitor_contentions.store(0, Ordering::Relaxed);
        self.threads_created.store(0, Ordering::Relaxed);
        self.jit_compilations.store(0, Ordering::Relaxed);
        self.jit_deoptimizations.store(0, Ordering::Relaxed);
        self.safepoints.store(0, Ordering::Relaxed);
        self.safepoint_pause_us.store(0, Ordering::Relaxed);
    }
}

impl Default for DiagnosticCounters {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Diagnostic Summary (snapshot)
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of all diagnostic counters.
#[derive(Debug, Clone)]
pub struct DiagnosticSummary {
    pub uptime_secs: f64,
    pub bytecodes_executed: u64,
    pub method_invocations: u64,
    pub exceptions_thrown: u64,
    pub classes_loaded: u64,
    pub classes_unloaded: u64,
    pub gc_cycles: u64,
    pub gc_pause_us: u64,
    pub heap_bytes_allocated: u64,
    pub monitor_contentions: u64,
    pub threads_created: u64,
    pub jit_compilations: u64,
    pub jit_deoptimizations: u64,
    pub safepoints: u64,
    pub safepoint_pause_us: u64,
}

impl std::fmt::Display for DiagnosticSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "VM Diagnostics Summary")?;
        writeln!(f, "  Uptime: {:.3}s", self.uptime_secs)?;
        writeln!(f, "  Bytecodes executed: {}", self.bytecodes_executed)?;
        writeln!(f, "  Method invocations: {}", self.method_invocations)?;
        writeln!(f, "  Exceptions thrown: {}", self.exceptions_thrown)?;
        writeln!(f, "  Classes loaded: {}", self.classes_loaded)?;
        writeln!(f, "  Classes unloaded: {}", self.classes_unloaded)?;
        writeln!(f, "  GC cycles: {}", self.gc_cycles)?;
        writeln!(f, "  GC pause time: {}us", self.gc_pause_us)?;
        writeln!(f, "  Heap allocated: {} bytes", self.heap_bytes_allocated)?;
        writeln!(f, "  Monitor contentions: {}", self.monitor_contentions)?;
        writeln!(f, "  Threads created: {}", self.threads_created)?;
        writeln!(f, "  JIT compilations: {}", self.jit_compilations)?;
        writeln!(f, "  JIT deoptimizations: {}", self.jit_deoptimizations)?;
        writeln!(f, "  Safepoints: {}", self.safepoints)?;
        writeln!(f, "  Safepoint pause time: {}us", self.safepoint_pause_us)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Health Check
// ---------------------------------------------------------------------------

/// Result of a VM health check.
#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub heap_healthy: bool,
    pub threads_healthy: bool,
    pub gc_healthy: bool,
    pub overall_healthy: bool,
    pub warnings: Vec<String>,
}

/// Perform a basic health check on VM state.
pub fn check_health(
    summary: &DiagnosticSummary,
    heap_usage_percent: f64,
    live_threads: u64,
) -> HealthCheck {
    let mut warnings = Vec::new();

    let heap_healthy = if heap_usage_percent > 95.0 {
        warnings.push(format!(
            "Heap usage critically high: {:.1}%",
            heap_usage_percent
        ));
        false
    } else if heap_usage_percent > 80.0 {
        warnings.push(format!("Heap usage elevated: {:.1}%", heap_usage_percent));
        true
    } else {
        true
    };

    let threads_healthy = if live_threads > 10000 {
        warnings.push(format!("Very high thread count: {}", live_threads));
        false
    } else {
        true
    };

    let gc_healthy = if summary.gc_cycles > 0 {
        let avg_pause = summary.gc_pause_us / summary.gc_cycles;
        if avg_pause > 1_000_000 {
            warnings.push(format!("Average GC pause very high: {}us", avg_pause));
            false
        } else {
            true
        }
    } else {
        true
    };

    let overall_healthy = heap_healthy && threads_healthy && gc_healthy;

    HealthCheck {
        heap_healthy,
        threads_healthy,
        gc_healthy,
        overall_healthy,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// B6: Silent-swallow instrumentation
// ---------------------------------------------------------------------------

/// Record a silent swallow of an error during VM bootstrap / class-init /
/// invokedynamic / native-call paths.
///
/// This helper exists so every swallow site in the VM behaves consistently:
///   - Increments `shared.debug.swallow_counter` so the CLI can detect a silent
///     exit caused by accumulated swallows.
///   - Emits a `tracing::warn!` with the site, category, and detail so users
///     setting `RUST_LOG=warn` or above see why something failed.
///   - Honors `CRATONVM_STRICT_SWALLOWS=1` — when set, escalates the swallow
///     to a panic so the culprit is impossible to miss during debugging.
pub fn record_swallow(shared: &crate::vm::SharedVm, site: &str, category: &str, detail: &str) {
    shared
        .debug
        .swallow_counter
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::warn!(
        site = %site,
        category = %category,
        detail = %detail,
        "B6: silent-swallow — error suppressed to keep VM running"
    );
    if crate::runtime::env_cache::strict_swallows() {
        panic!("CRATONVM_STRICT_SWALLOWS=1: swallow at {site} [{category}]: {detail}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counters_default_zero() {
        let counters = DiagnosticCounters::new();
        assert_eq!(DiagnosticCounters::get(&counters.bytecodes_executed), 0);
        assert_eq!(DiagnosticCounters::get(&counters.gc_cycles), 0);
    }

    #[test]
    fn test_counters_increment() {
        let counters = DiagnosticCounters::new();
        counters.inc(&counters.bytecodes_executed);
        counters.inc(&counters.bytecodes_executed);
        counters.inc(&counters.bytecodes_executed);
        assert_eq!(DiagnosticCounters::get(&counters.bytecodes_executed), 3);
    }

    #[test]
    fn test_counters_add() {
        let counters = DiagnosticCounters::new();
        counters.add(&counters.gc_pause_us, 5000);
        counters.add(&counters.gc_pause_us, 3000);
        assert_eq!(DiagnosticCounters::get(&counters.gc_pause_us), 8000);
    }

    #[test]
    fn test_counters_reset() {
        let counters = DiagnosticCounters::new();
        counters.inc(&counters.exceptions_thrown);
        counters.add(&counters.heap_bytes_allocated, 1024);
        counters.reset();
        assert_eq!(DiagnosticCounters::get(&counters.exceptions_thrown), 0);
        assert_eq!(DiagnosticCounters::get(&counters.heap_bytes_allocated), 0);
    }

    #[test]
    fn test_summary_snapshot() {
        let counters = DiagnosticCounters::new();
        counters.inc(&counters.gc_cycles);
        let summary = counters.summary();
        assert_eq!(summary.gc_cycles, 1);
        assert!(summary.uptime_secs >= 0.0);
    }

    /// `classes_loaded` reports the class loader's own definition count, not
    /// this struct's cell.
    ///
    /// The previous version of this test wrote 42 into the cell and read 42
    /// back, which passed for as long as the field existed and proved only that
    /// an `AtomicU64` stores what you put in it. Nothing in the VM ever wrote to
    /// it, so the report said `Classes loaded: 0` forever and the test was
    /// green throughout. A summary field must be tested against the thing that
    /// PRODUCES it.
    #[test]
    fn summary_classes_loaded_tracks_real_class_definitions() {
        let counters = DiagnosticCounters::new();
        let before = counters.summary().classes_loaded;
        cratonvm_classloading::define_census::note("com/example/DiagnosticsProbe");
        assert_eq!(
            counters.summary().classes_loaded,
            before + 1,
            "summary().classes_loaded did not follow a real class definition"
        );
        // And the struct's own cell is NOT what feeds it — writing to the cell
        // must not move the reported number, or the old defect can come back
        // silently by someone re-pointing the field.
        let held = counters.summary().classes_loaded;
        counters.add(&counters.classes_loaded, 42);
        assert_eq!(counters.summary().classes_loaded, held);
    }

    #[test]
    fn test_summary_display() {
        let counters = DiagnosticCounters::new();
        counters.add(&counters.method_invocations, 100);
        let summary = counters.summary();
        let output = format!("{}", summary);
        assert!(output.contains("Method invocations: 100"));
        assert!(output.contains("VM Diagnostics Summary"));
    }

    #[test]
    fn test_health_check_healthy() {
        let summary = DiagnosticSummary {
            uptime_secs: 100.0,
            bytecodes_executed: 1000,
            method_invocations: 500,
            exceptions_thrown: 0,
            classes_loaded: 50,
            classes_unloaded: 0,
            gc_cycles: 5,
            gc_pause_us: 5000,
            heap_bytes_allocated: 1024 * 1024,
            monitor_contentions: 0,
            threads_created: 10,
            jit_compilations: 3,
            jit_deoptimizations: 0,
            safepoints: 5,
            safepoint_pause_us: 100,
        };
        let health = check_health(&summary, 50.0, 20);
        assert!(health.overall_healthy);
        assert!(health.warnings.is_empty());
    }

    #[test]
    fn test_health_check_high_heap() {
        let summary = DiagnosticSummary {
            uptime_secs: 100.0,
            bytecodes_executed: 0,
            method_invocations: 0,
            exceptions_thrown: 0,
            classes_loaded: 0,
            classes_unloaded: 0,
            gc_cycles: 0,
            gc_pause_us: 0,
            heap_bytes_allocated: 0,
            monitor_contentions: 0,
            threads_created: 0,
            jit_compilations: 0,
            jit_deoptimizations: 0,
            safepoints: 0,
            safepoint_pause_us: 0,
        };
        let health = check_health(&summary, 96.0, 5);
        assert!(!health.heap_healthy);
        assert!(!health.overall_healthy);
        assert!(!health.warnings.is_empty());
    }

    #[test]
    fn test_health_check_high_threads() {
        let summary = DiagnosticSummary {
            uptime_secs: 100.0,
            bytecodes_executed: 0,
            method_invocations: 0,
            exceptions_thrown: 0,
            classes_loaded: 0,
            classes_unloaded: 0,
            gc_cycles: 0,
            gc_pause_us: 0,
            heap_bytes_allocated: 0,
            monitor_contentions: 0,
            threads_created: 0,
            jit_compilations: 0,
            jit_deoptimizations: 0,
            safepoints: 0,
            safepoint_pause_us: 0,
        };
        let health = check_health(&summary, 50.0, 20000);
        assert!(!health.threads_healthy);
        assert!(!health.overall_healthy);
    }

    #[test]
    fn test_counters_thread_safety() {
        use std::sync::Arc;
        let counters = Arc::new(DiagnosticCounters::new());
        let mut handles = vec![];
        for _ in 0..4 {
            let c = counters.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    c.inc(&c.bytecodes_executed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(DiagnosticCounters::get(&counters.bytecodes_executed), 4000);
    }

    #[test]
    fn test_uptime_positive() {
        let counters = DiagnosticCounters::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(counters.uptime_secs() > 0.0);
    }
}
