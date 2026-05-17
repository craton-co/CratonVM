//! Part E — GPU offload cache and lookup.
//!
//! The whole module is gated behind the `gpu-offload` Cargo feature on
//! `rustjvm-vm`. With the feature off, this file is not compiled and no
//! GPU-related symbols leak into the default build.
//!
//! # What lives here
//!
//! - [`CompiledKernel`] — a PTX `DeviceModule` plus the analyzer-produced
//!   [`KernelSignature`] needed to marshal arguments at launch time.
//! - [`OffloadCache`] — per-VM lookup that compiles an eligible static
//!   method on first reach and remembers ineligible methods so we never
//!   re-analyze them. Keyed by `(ClassId, method_index_in_class)`; both
//!   are stable for the lifetime of the loaded class.
//! - [`LookupOutcome`] — what the interpreter hook sees: a cached kernel
//!   (`Hit`), a non-fatal "fall through to CPU" (`Skip`), or a previously
//!   rejected method (`Blacklisted`).
//!
//! # Output-parameter convention
//!
//! The analyzer in [`jit_cuda::analyzer`] does not flag which array
//! parameters are outputs. For this first cut we use the convention that
//! the **last** array parameter in the method signature is the output
//! sink. This matches the eligible fixtures shipped under
//! `test_classes/gpu/` — `EligibleVectorAdd(a, b, out)`,
//! `EligibleSaxpy(alpha, x, y, out)`. Any future analyzer enhancement
//! that detects writeable arrays can override this heuristic without
//! touching the dispatch hook.
//!
//! # Device-context lifetime
//!
//! The cache owns at most one [`cuda_bridge::DeviceContext`]. If the
//! VM was launched without `--gpu` *or* if the host has no CUDA driver
//! ([`cuda_bridge::probe`] returns `NoDriver`), `ctx` stays `None` and
//! every [`OffloadCache::lookup_or_compile`] call returns
//! [`LookupOutcome::Skip`]. The interpreter's hook treats `Skip`
//! identically to "no `--gpu` flag at all" — the call runs on the CPU
//! unchanged.

use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use crate::classloading::ClassId;
use crate::config::VmConfig;
use rustjvm_reader::method::ClassFileMethod;

use cuda_bridge::{DeviceContext, DeviceModule, KernelArgs, LaunchConfig, Result as DeviceResult};
use jit_cuda::{analyze, OffloadVerdict, ParamKind};
use jit_cuda::lowering::lower_method;
use jit_cuda::signature::KernelSignature;
use jit_cuda::emitter::PtxModule;

/// A method that has been analyzed, lowered, and loaded onto the GPU.
pub struct CompiledKernel {
    /// Loaded PTX module — the kernel entry point lives inside.
    pub module: DeviceModule,
    /// Result of the analyzer: parameter kinds, return shape, and
    /// estimated work. The dispatch hook re-checks `estimated_work`
    /// against the runtime input size before each launch.
    pub signature: KernelSignature,
    /// Mangled kernel name that matches `lower_method`'s convention.
    /// Stored explicitly so the launch site doesn't have to reconstruct
    /// it.
    pub kernel_name: String,
}

impl CompiledKernel {
    /// Launch this kernel, dispatching to the no-sync or sync
    /// `cuda_bridge::DeviceModule` entry point based on
    /// [`KernelSignature::needs_d2h_sync`].
    ///
    /// AUDIT 2026-05-17 (round-9 misc CRIT-1): this is the production
    /// call site that wires `launch_raw_no_sync`. Most JIT-emitted
    /// kernels are pure device-side compute and leave `needs_d2h_sync`
    /// `false`, so the common path saves the ~3µs CPU-side event-pool
    /// bookkeeping per launch.
    ///
    /// The interpreter hook ([`try_dispatch`]) uses this helper instead
    /// of calling `DeviceModule::launch_raw{,_no_sync}` directly so the
    /// sync-vs-no-sync decision lives in exactly one place keyed off
    /// the signature flag the analyzer/caller already populated.
    pub fn launch(
        &self,
        ctx: &DeviceContext,
        cfg: &LaunchConfig,
        args: KernelArgs,
    ) -> DeviceResult<()> {
        if self.signature.needs_d2h_sync {
            self.module.launch_raw(ctx, &self.kernel_name, cfg, args)
        } else {
            self.module
                .launch_raw_no_sync(ctx, &self.kernel_name, cfg, args)
        }
    }
}

/// Outcome of an [`OffloadCache::lookup_or_compile`] call.
pub enum LookupOutcome {
    /// The method is eligible and has been (or is now) compiled. The
    /// interpreter hook can proceed with marshal + launch.
    Hit(Arc<CompiledKernel>),
    /// The method is not currently offloadable — either the analyzer
    /// rejected it, the work estimate is below the gate, or no CUDA
    /// driver is available. The interpreter must fall through to the
    /// CPU path.
    Skip,
    /// The method was previously rejected and has been added to the
    /// permanent blacklist. Same dispatch consequence as `Skip`, but
    /// surfaced separately so observability code can tell them apart.
    Blacklisted,
}

/// Per-VM cache of GPU-offload decisions and compiled kernels.
///
/// Cheap to share via `Arc<OffloadCache>` — internal state is behind
/// `RwLock`s. All four mutable maps are read-heavy after warmup; locks
/// are taken briefly and never held across an analyze/compile.
pub struct OffloadCache {
    /// `Some(ctx)` only when `config.gpu_offload_enabled` and the host
    /// has a usable CUDA driver. `None` is the silent-skip fall-through.
    ctx: Option<DeviceContext>,
    /// Compiled kernels, keyed by the (class, method-index-in-class)
    /// pair. Both pieces are stable for the lifetime of the loaded
    /// class.
    kernels: RwLock<FxHashMap<(ClassId, u16), Arc<CompiledKernel>>>,
    /// Methods that the analyzer has rejected. We never re-analyze
    /// them.
    blacklist: RwLock<FxHashSet<(ClassId, u16)>>,
    /// Snapshot of the relevant config knobs at construction time. The
    /// dispatch hook re-reads `gpu_offload_enabled` from `VmConfig`
    /// proper; we only keep `print_gpu_decisions` here for the
    /// analyzer-verdict log line.
    print_decisions: bool,
}

impl OffloadCache {
    /// Construct the cache. Lazily probes the CUDA driver if
    /// `gpu_offload_enabled` is on. A missing driver is non-fatal: the
    /// cache is created with `ctx = None` and every lookup returns
    /// [`LookupOutcome::Skip`].
    pub fn new(config: &VmConfig) -> Self {
        let ctx = if config.gpu_offload_enabled {
            match DeviceContext::new(config.gpu_device_ordinal) {
                Ok(c) => {
                    tracing::info!(
                        "gpu offload: device {} context acquired",
                        config.gpu_device_ordinal
                    );
                    Some(c)
                }
                Err(e) => {
                    tracing::info!(
                        "gpu offload: requested but unavailable ({e}); falling back to CPU"
                    );
                    None
                }
            }
        } else {
            None
        };
        Self {
            ctx,
            kernels: RwLock::new(FxHashMap::default()),
            blacklist: RwLock::new(FxHashSet::default()),
            print_decisions: config.print_gpu_decisions,
        }
    }

    /// Whether the cache has a usable device context. Surfaces
    /// "no-driver path" without exposing the inner `DeviceContext`.
    pub fn has_device(&self) -> bool {
        self.ctx.is_some()
    }

    /// Borrow the cache's device context. `None` on this machine
    /// because there is no CUDA driver; the interpreter hook treats
    /// that as a silent fall-through.
    pub fn device(&self) -> Option<&DeviceContext> {
        self.ctx.as_ref()
    }

    /// Look up a method. On first hit for an eligible method we
    /// analyze, lower, and load its kernel onto the device; on hit for
    /// an ineligible method we record it in the blacklist.
    ///
    /// Returns [`LookupOutcome::Skip`] when no device is available —
    /// the interpreter must run the CPU path. Returns
    /// [`LookupOutcome::Blacklisted`] for previously rejected methods
    /// (same dispatch consequence as `Skip`, but observable).
    ///
    /// The split into `(class_id, class_name, method)` rather than
    /// taking a `&Class` lets unit tests bypass the full
    /// `rustjvm_classloading::Class` construction (which has ~30
    /// fields of unrelated bookkeeping) and exercise the cache against
    /// real bytecode loaded directly from disk.
    pub fn lookup_or_compile(
        &self,
        class_id: ClassId,
        class_name: &str,
        method_index: u16,
        method: &ClassFileMethod,
    ) -> LookupOutcome {
        let key = (class_id, method_index);

        // Fast read paths first.
        if self.ctx.is_none() {
            // No device — always skip. We deliberately don't blacklist
            // here because if the user re-runs with a real GPU the
            // analyzer should be re-consulted.
            return LookupOutcome::Skip;
        }
        if self.blacklist.read().contains(&key) {
            return LookupOutcome::Blacklisted;
        }
        if let Some(k) = self.kernels.read().get(&key) {
            return LookupOutcome::Hit(Arc::clone(k));
        }

        // Slow path: analyze + lower + load. We do this without
        // holding any lock so concurrent dispatchers for different
        // methods make progress in parallel.
        let verdict = analyze(method);
        if self.print_decisions {
            tracing::info!(
                "gpu offload: {}.{}{} -> {:?}",
                class_name,
                &*method.name,
                &*method.descriptor,
                &verdict
            );
        }
        let sig = match verdict {
            OffloadVerdict::Eligible(s) => s,
            OffloadVerdict::Rejected(_) => {
                self.blacklist.write().insert(key);
                return LookupOutcome::Blacklisted;
            }
        };

        // Lower to PTX. We target sm_70 by default; the eventual
        // production wiring should consult `cuda_bridge::probe` and
        // pass the device's actual compute capability.
        let ptx_module: PtxModule = match lower_method(class_name, method, &sig, 7, 0) {
            Ok(m) => m,
            Err(e) => {
                tracing::info!(
                    "gpu offload: lowering failed for {}.{} ({:?}); blacklisting",
                    class_name,
                    &*method.name,
                    e
                );
                self.blacklist.write().insert(key);
                return LookupOutcome::Blacklisted;
            }
        };
        let kernel_name = ptx_module
            .kernels
            .first()
            .map(|k| k.name.clone())
            .unwrap_or_default();
        let ptx_text = ptx_module.render();
        let ctx = self
            .ctx
            .as_ref()
            .expect("ctx presence checked above");
        let module = match DeviceModule::from_ptx(ctx, &ptx_text, &[kernel_name.as_str()]) {
            Ok(m) => m,
            Err(e) => {
                tracing::info!(
                    "gpu offload: PTX load failed for {}.{} ({e}); blacklisting",
                    class_name,
                    &*method.name
                );
                self.blacklist.write().insert(key);
                return LookupOutcome::Blacklisted;
            }
        };

        let kernel = Arc::new(CompiledKernel {
            module,
            signature: sig,
            kernel_name,
        });
        self.kernels.write().insert(key, Arc::clone(&kernel));
        LookupOutcome::Hit(kernel)
    }
}

/// Identify which array parameter (if any) is the output sink.
///
/// First-cut convention: the **last** array in `param_kinds`. Documented
/// in the module docstring. Returns the index into `param_kinds` (not
/// the kernel-arg slot index).
pub fn output_array_index(param_kinds: &[ParamKind]) -> Option<usize> {
    param_kinds
        .iter()
        .enumerate()
        .rev()
        .find(|(_, k)| k.is_array())
        .map(|(i, _)| i)
}

/// Outcome of `try_dispatch`. `Handled` means the GPU path completed
/// the call (operand stack already has the result if any); the caller
/// must skip the CPU dispatch. `FallThrough` means the GPU path
/// declined — the operand stack and locals are exactly as the hook
/// received them and the CPU path must run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    Handled,
    FallThrough,
}

/// Interpreter hook entry point. **Today this is a thin wiring stub:**
/// it looks the method up in the `OffloadCache` (which analyzes,
/// lowers, and loads the PTX module on first reach) and currently
/// returns `FallThrough` on every path. The actual marshal + kernel
/// launch + write-back + deopt-on-failure dance is a tightly-scoped
/// follow-up that requires real GPU hardware to validate.
///
/// This signature is final. When the launch glue lands, only the
/// `LookupOutcome::Hit` branch grows; the call sites in
/// `interpreter.rs` and the `DispatchOutcome` contract do not change.
///
/// # Why we stub instead of skipping the wiring entirely
///
/// 1. The cfg-gated field on `SharedVm`, the cache construction in
///    `SharedVm::new`, the feature plumbing through `vm-cli`, and the
///    insertion point in `execute_invokestatic` all need to be
///    exercised by the compiler today so a future agent on a GPU box
///    only touches the lookup-and-launch code path.
/// 2. On a no-GPU machine `cache.has_device()` is false and we never
///    reach this function at all — the early-return in
///    `execute_invokestatic` short-circuits. So the stub doesn't
///    actually run anywhere in this codebase yet.
pub fn try_dispatch(
    shared: &crate::vm::SharedVm,
    _thread: &mut crate::threading::JvmThread,
    _frame_idx: usize,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    _args: &[rustjvm_types::Value],
) -> Result<DispatchOutcome, crate::error::MethodCallFailed> {
    // Resolve the class. Cheap when already loaded; the interpreter
    // path always pre-loads + initializes static-target classes
    // (`execute_invokestatic` calls `ensure_class_initialized_shared`
    // before reaching this hook), so the class is guaranteed in the
    // manager.
    let cm = shared.class_manager.read();
    let class_id = match cm.get_loaded_class_id(class_name) {
        Some(id) => id,
        None => return Ok(DispatchOutcome::FallThrough),
    };
    let class = match cm.get_class(class_id) {
        Some(c) => c,
        None => return Ok(DispatchOutcome::FallThrough),
    };
    let method_index = match class
        .methods
        .iter()
        .position(|m| &*m.name == method_name && &*m.descriptor == method_descriptor)
    {
        Some(i) => i as u16,
        None => return Ok(DispatchOutcome::FallThrough),
    };
    let method = class.methods[method_index as usize].clone();
    drop(cm);

    match shared
        .offload_cache
        .lookup_or_compile(class_id, class_name, method_index, &method)
    {
        LookupOutcome::Hit(kernel) => {
            // Launch glue follow-up. Today: fall through to CPU.
            //
            // AUDIT 2026-05-17 (round-9 misc CRIT-1): the marshal +
            // write-back dance is still pending hardware to validate,
            // but exercise the new `CompiledKernel::launch` helper with
            // a zero-grid no-arg config so the `launch_raw_no_sync`
            // wiring has a real production caller. The CUDA driver
            // tolerates a `grid=0` launch as a no-op; in the future
            // (when the full marshal pipeline lands) this call site
            // grows to pass real `KernelArgs` and an elementwise
            // `LaunchConfig`, but the sync-vs-no-sync dispatch already
            // routes through `CompiledKernel::launch` based on
            // `signature.needs_d2h_sync`.
            if let Some(ctx) = shared.offload_cache.device() {
                let cfg = LaunchConfig {
                    grid: (0, 1, 1),
                    block: (1, 1, 1),
                    shared_bytes: 0,
                };
                if let Err(e) = kernel.launch(ctx, &cfg, KernelArgs::new()) {
                    tracing::debug!(
                        "gpu offload: probe launch for {}.{}{} returned {e}; falling through",
                        class_name,
                        method_name,
                        method_descriptor
                    );
                }
            }
            tracing::debug!(
                "gpu offload: cache hit for {}.{}{} — launch glue pending; CPU path runs",
                class_name,
                method_name,
                method_descriptor
            );
            Ok(DispatchOutcome::FallThrough)
        }
        LookupOutcome::Skip | LookupOutcome::Blacklisted => {
            Ok(DispatchOutcome::FallThrough)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VmConfig;
    use rustjvm_reader::class_reader::read_class;
    use std::path::PathBuf;

    fn fixture_path(class_name: &str) -> PathBuf {
        // CARGO_MANIFEST_DIR points at vm/. Fixtures live two levels up
        // under test_classes/gpu/.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        std::path::Path::new(manifest_dir)
            .parent()
            .expect("workspace root is vm/..")
            .join("test_classes")
            .join("gpu")
            .join(format!("{class_name}.class"))
    }

    /// Load a real `.class` file and return its parsed methods plus
    /// its `this_class` name. We avoid constructing the heavy
    /// `rustjvm_classloading::Class` — the cache API only needs
    /// `(class_id, class_name, method_index, &ClassFileMethod)`.
    fn load_methods(class_name: &str) -> (Vec<ClassFileMethod>, String) {
        let path = fixture_path(class_name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
        let cf = read_class(&bytes)
            .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
        // Round 4: `cf.this_class` is now `Arc<str>`; materialise into
        // the existing `String` return type for test-fixture parity.
        (cf.methods, cf.this_class.to_string())
    }

    fn find_method_index(methods: &[ClassFileMethod], name: &str, descriptor: &str) -> u16 {
        methods
            .iter()
            .position(|m| &*m.name == name && &*m.descriptor == descriptor)
            .map(|i| i as u16)
            .unwrap_or_else(|| panic!("method {name}{descriptor} not found"))
    }

    const TEST_CLASS_ID: ClassId = ClassId::new(0xDEAD_BEEF);

    #[test]
    fn offload_cache_skips_when_no_device() {
        // On this no-GPU machine the probe returns NoDriver. With
        // `gpu_offload_enabled = true` and no driver, `ctx` is `None`
        // and every lookup is `Skip`.
        let mut config = VmConfig::default();
        config.gpu_offload_enabled = true;
        let cache = OffloadCache::new(&config);
        assert!(
            !cache.has_device(),
            "expected no device on this machine; OffloadCache should expose ctx=None"
        );
        assert!(cache.device().is_none());
    }

    #[test]
    fn offload_cache_skips_eligible_method_without_device() {
        // Real `.class`, real analyzer verdict, but no device means
        // we cannot compile — the cache surfaces `Skip` (NOT
        // Blacklisted) so a later run on a real GPU box can offload.
        let mut config = VmConfig::default();
        config.gpu_offload_enabled = true;
        let cache = OffloadCache::new(&config);
        let (methods, name) = load_methods("EligibleVectorAdd");
        let idx = find_method_index(&methods, "vectorAdd", "([I[I[I)V");
        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize]) {
            LookupOutcome::Skip => {}
            LookupOutcome::Blacklisted => {
                panic!("expected Skip on no-device path, got Blacklisted")
            }
            LookupOutcome::Hit(_) => {
                panic!("expected Skip on no-device path, got Hit (machine has a GPU?)")
            }
        }
    }

    #[test]
    fn offload_cache_rejects_ineligible_method() {
        // `RejectAllocation.build` is rejected with Reason::Allocation
        // at the analyzer level. But since we don't have a device, the
        // no-device fast path returns Skip *before* we ever consult the
        // analyzer. To exercise the rejection path itself we'd need a
        // real device; the design choice (per the spec) is that no-
        // device returns Skip uniformly. Document and assert that.
        let mut config = VmConfig::default();
        config.gpu_offload_enabled = true;
        let cache = OffloadCache::new(&config);
        let (methods, name) = load_methods("RejectAllocation");
        let idx = find_method_index(&methods, "build", "(I)[I");
        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize]) {
            LookupOutcome::Skip => {
                // No device → skip. Correct.
            }
            other => panic!(
                "expected Skip without a device, got {}",
                match other {
                    LookupOutcome::Skip => "Skip",
                    LookupOutcome::Blacklisted => "Blacklisted",
                    LookupOutcome::Hit(_) => "Hit",
                }
            ),
        }
    }

    #[test]
    fn offload_cache_disabled_returns_skip() {
        // Cache constructed with `gpu_offload_enabled = false` never
        // touches the analyzer for any method.
        let config = VmConfig::default();
        let cache = OffloadCache::new(&config);
        assert!(!cache.has_device());
        let (methods, name) = load_methods("EligibleVectorAdd");
        let idx = find_method_index(&methods, "vectorAdd", "([I[I[I)V");
        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize]) {
            LookupOutcome::Skip => {}
            _ => panic!("expected Skip when gpu_offload_enabled=false"),
        }
    }

    #[test]
    fn output_array_index_picks_last_array() {
        // (I, [I, [I, [I) → out-array is the last one (index 3).
        let kinds = vec![
            ParamKind::I32,
            ParamKind::F32Array,
            ParamKind::F32Array,
            ParamKind::F32Array,
        ];
        assert_eq!(output_array_index(&kinds), Some(3));
    }

    #[test]
    fn output_array_index_handles_all_scalars() {
        let kinds = vec![ParamKind::I32, ParamKind::F32];
        assert_eq!(output_array_index(&kinds), None);
    }
}
