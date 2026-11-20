// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    /// zt t15/pl6: classes whose initialization this VM performed (the
    /// `<clinit>` step of `vm_util::initialize_class_shared`, whether or not
    /// the class declares one), HotSpot's `sun.cls.initializedClasses`
    /// (`InstanceKlass::initialize_impl` step 8). Cumulative; never reset
    /// ([`Self::reset`] leaves it: a perf counter is monotonic).
    pub classes_initialized: AtomicU64,
    /// zt t15/pl6: link attempts this VM began (a class's `Loaded ->
    /// Verifying` edge), HotSpot's `sun.cls.linkedClasses` (each
    /// `InstanceKlass::link_class_impl` that finds the class unlinked) and,
    /// with no pre-verified shared classes here, its `sun.cls.verifiedClasses`
    /// (`verify_code` on every unrewritten link). Cumulative; never reset.
    pub classes_linked: AtomicU64,
    /// zt t16/pl7: `JVM_DefineClass`-shaped defines (`defineClass1` /
    /// `defineClass2` / `Unsafe.defineClass0`, counted by their natives with
    /// the bytes in hand; `NativeSystemAccess::note_jvm_define_class`),
    /// HotSpot's `sun.cls.defineAppClasses`. Cumulative; never reset.
    pub app_classes_defined: AtomicU64,
    /// zt t16/pl7: those defines' class-file bytes, HotSpot's
    /// `sun.cls.appClassBytes`. Cumulative; never reset.
    pub app_class_bytes: AtomicU64,
    /// zt t16/pl7: `Unsafe.defineClass0` calls, HotSpot's
    /// `sun.cls.unsafeDefineClassCalls`. Cumulative; never reset.
    pub unsafe_define_class_calls: AtomicU64,
    /// VM start time. Behind a lock only so a boot image's restored run can
    /// rebase it to its own start ([`Self::rebase_start_instant`]); read
    /// rarely (uptime, management, perf-data origin).
    start_time: parking_lot::RwLock<Instant>,
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
            classes_initialized: AtomicU64::new(0),
            classes_linked: AtomicU64::new(0),
            app_classes_defined: AtomicU64::new(0),
            app_class_bytes: AtomicU64::new(0),
            unsafe_define_class_calls: AtomicU64::new(0),
            start_time: parking_lot::RwLock::new(Instant::now()),
        }
    }

    /// Get VM uptime in seconds.
    pub fn uptime_secs(&self) -> f64 {
        self.start_time.read().elapsed().as_secs_f64()
    }

    /// Phase 5 WP6: a restored boot image's VM was created by the capture
    /// process; its start is this process's.
    pub fn rebase_start_instant(&self, start: Instant) {
        *self.start_time.write() = start;
    }

    /// The instant these counters -- and so the owning VM -- were created:
    /// the VM's start, as the management natives report it
    /// (`NativeSystemAccess::vm_start_instant`, gc-common w17-e).
    pub fn start_instant(&self) -> Instant {
        *self.start_time.read()
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
mod tests;
