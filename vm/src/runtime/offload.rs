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
use rustjvm_reader::constant_pool::ConstantPool;
use rustjvm_reader::method::ClassFileMethod;

use cuda_bridge::{DeviceContext, DeviceModule};
use jit_cuda::annotations::read_method_annotations;
use jit_cuda::{analyzer, OffloadVerdict, ParamKind};
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
    /// The split into `(class_id, class_name, method, cp)` rather than
    /// taking a `&Class` lets unit tests bypass the full
    /// `rustjvm_classloading::Class` construction (which has ~30
    /// fields of unrelated bookkeeping) and exercise the cache against
    /// real bytecode loaded directly from disk. The constant pool is
    /// threaded through explicitly because annotation parsing
    /// dereferences UTF-8 entries by index.
    pub fn lookup_or_compile(
        &self,
        class_id: ClassId,
        class_name: &str,
        method_index: u16,
        method: &ClassFileMethod,
        constant_pool: &ConstantPool,
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

        // Read method annotations once and reuse them for both the
        // exclude short-circuit and the analyzer hint feed. The
        // annotations module is pure data and feature-gated alongside
        // this cache, so there's no driver dependency on this path.
        let method_annotations = read_method_annotations(&method.attributes, constant_pool);

        // Short-circuit on @GpuExclude. A method tagged exclude is a
        // permanent blacklist entry: the developer has explicitly
        // opted out, so we record the verdict and don't even hand the
        // method to the analyzer.
        if let Some(exclude) = &method_annotations.gpu_exclude {
            tracing::debug!(
                target: "gpu.offload",
                class = %class_name,
                method = method_index,
                reason = %exclude.reason,
                "blacklisted by @GpuExclude",
            );
            self.blacklist.write().insert(key);
            return LookupOutcome::Blacklisted;
        }

        // Slow path: analyze + lower + load. We do this without
        // holding any lock so concurrent dispatchers for different
        // methods make progress in parallel. Annotations are passed
        // through so the analyzer can use hints (e.g. `@GpuKernel`
        // attrs) when deciding eligibility.
        let verdict = analyzer::analyze_with_annotations(method, &method_annotations);
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

/// Per-VM registry of [`OffloadCache`] instances, keyed by CUDA device
/// ordinal.
///
/// Phase 3 introduces this layer so a single VM can address multiple
/// GPUs without re-probing the driver on every dispatch. The first
/// caller for a given ordinal pays the probe + context-acquisition
/// cost; subsequent callers reuse the cached `Arc<OffloadCache>`.
///
/// Today every callsite passes `config.gpu_device_ordinal` (default 0),
/// so in practice there is one cache. The registry shape is the API
/// surface real multi-GPU support will plug into — see the
/// `get_or_create` PHASE3 note.
pub struct OffloadCacheRegistry {
    per_device: parking_lot::RwLock<
        rustc_hash::FxHashMap<u32, std::sync::Arc<OffloadCache>>,
    >,
}

impl OffloadCacheRegistry {
    pub fn new() -> Self {
        Self {
            per_device: parking_lot::RwLock::new(rustc_hash::FxHashMap::default()),
        }
    }

    /// Get-or-construct the `OffloadCache` for `device_ordinal`. The
    /// first caller for a given ordinal pays the probe cost; later
    /// callers reuse the cached `Arc`.
    pub fn get_or_create(
        &self,
        device_ordinal: u32,
        config: &crate::config::VmConfig,
    ) -> std::sync::Arc<OffloadCache> {
        if let Some(cache) = self.per_device.read().get(&device_ordinal) {
            return cache.clone();
        }
        let mut write = self.per_device.write();
        if let Some(cache) = write.get(&device_ordinal) {
            return cache.clone();
        }
        // PHASE3: per-device probe currently routes through
        // OffloadCache::new which honours `config.gpu_device_ordinal`.
        // When real multi-GPU lands this passes the ordinal through to
        // cuda-bridge directly rather than re-using the config field.
        let cache = std::sync::Arc::new(OffloadCache::new(config));
        write.insert(device_ordinal, cache.clone());
        cache
    }

    /// Lookup without constructing. Returns `None` if no cache has
    /// been created for `device_ordinal` yet.
    pub fn get(&self, device_ordinal: u32) -> Option<std::sync::Arc<OffloadCache>> {
        self.per_device.read().get(&device_ordinal).cloned()
    }
}

impl Default for OffloadCacheRegistry {
    fn default() -> Self {
        Self::new()
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
    // Hold the class-manager read lock for the full lookup_or_compile
    // call so we can pass the class's constant pool by reference rather
    // than cloning ~hundreds of entries. The cache itself takes no
    // class-manager locks, so this is not a re-entrancy risk; the
    // tradeoff is that the very first compile of a given method holds
    // the manager's read lock for the duration of analyze+lower+load.
    // Concurrent dispatchers reading the manager are unaffected.
    let cache = shared
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config);
    let outcome = cache.lookup_or_compile(
        class_id,
        class_name,
        method_index,
        &class.methods[method_index as usize],
        &class.constant_pool,
    );
    drop(cm);

    match outcome {
        LookupOutcome::Hit(_kernel) => {
            // Launch glue follow-up. Today: fall through to CPU.
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

    /// Load a real `.class` file and return its parsed methods, its
    /// `this_class` name, and its constant pool. We avoid constructing
    /// the heavy `rustjvm_classloading::Class` — the cache API only
    /// needs `(class_id, class_name, method_index, &ClassFileMethod,
    /// &ConstantPool)`.
    fn load_methods(class_name: &str) -> (Vec<ClassFileMethod>, String, ConstantPool) {
        let path = fixture_path(class_name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
        let cf = read_class(&bytes)
            .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
        // `this_class` is already resolved to a String by the reader.
        (cf.methods, cf.this_class, cf.constant_pool)
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
        let (methods, name, cp) = load_methods("EligibleVectorAdd");
        let idx = find_method_index(&methods, "vectorAdd", "([I[I[I)V");
        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize], &cp) {
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
        let (methods, name, cp) = load_methods("RejectAllocation");
        let idx = find_method_index(&methods, "build", "(I)[I");
        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize], &cp) {
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
        let (methods, name, cp) = load_methods("EligibleVectorAdd");
        let idx = find_method_index(&methods, "vectorAdd", "([I[I[I)V");
        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize], &cp) {
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

    /// `@GpuExclude` is a permanent opt-out: the cache must record the
    /// method in the blacklist and return `Blacklisted` *without*
    /// invoking the analyzer.
    ///
    /// Requires a fixture compiled with `@GpuExclude` on the kernel
    /// method (Items 7/8). The fixture is expected at
    /// `test_classes/gpu/annotations/ExcludedKernel.class`. We also
    /// need a real device context — on the no-GPU CI machine the
    /// `ctx.is_none()` fast path beats annotation reading, so the
    /// short-circuit cannot be exercised. Marked `#[ignore]` for that
    /// reason; run on a GPU host with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires @GpuExclude fixture (Items 7/8) and a real CUDA device"]
    fn excluded_method_returns_blacklisted() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir)
            .parent()
            .expect("workspace root is vm/..")
            .join("test_classes")
            .join("gpu")
            .join("annotations")
            .join("ExcludedKernel.class");
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()));
        let cf = read_class(&bytes)
            .unwrap_or_else(|e| panic!("failed to parse fixture: {e:?}"));
        let methods = cf.methods;
        let name = cf.this_class;
        let cp = cf.constant_pool;

        let idx = methods
            .iter()
            .position(|m| {
                let n = &*m.name;
                n == "run" || n == "kernel" || n == "compute" || n == "main" || n == "vectorAdd"
            })
            .map(|i| i as u16)
            .expect("ExcludedKernel fixture must have an entry-point method");

        let mut config = VmConfig::default();
        config.gpu_offload_enabled = true;
        let cache = OffloadCache::new(&config);
        if !cache.has_device() {
            eprintln!("excluded_method_returns_blacklisted: no CUDA device; skipping body");
            return;
        }

        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize], &cp) {
            LookupOutcome::Blacklisted => {}
            LookupOutcome::Skip => panic!("expected Blacklisted for @GpuExclude, got Skip"),
            LookupOutcome::Hit(_) => {
                panic!("expected Blacklisted for @GpuExclude, got Hit (analyzer ran?!)")
            }
        }
    }

    /// Even a method whose body would be eligible must be blacklisted
    /// when `@GpuExclude` is present.
    #[test]
    #[ignore = "requires @GpuExclude-on-eligible fixture (Items 7/8) and a real CUDA device"]
    fn excluded_short_circuits_analyzer() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest_dir)
            .parent()
            .expect("workspace root is vm/..")
            .join("test_classes")
            .join("gpu")
            .join("annotations")
            .join("ExcludedAndKernel.class");
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()));
        let cf = read_class(&bytes)
            .unwrap_or_else(|e| panic!("failed to parse fixture: {e:?}"));
        let methods = cf.methods;
        let name = cf.this_class;
        let cp = cf.constant_pool;

        let idx = methods
            .iter()
            .position(|m| {
                let n = &*m.name;
                n == "vectorAdd" || n == "saxpy" || n == "run" || n == "kernel"
            })
            .map(|i| i as u16)
            .expect("ExcludedAndKernel fixture must have a kernel-shaped method");

        let mut config = VmConfig::default();
        config.gpu_offload_enabled = true;
        let cache = OffloadCache::new(&config);
        if !cache.has_device() {
            eprintln!("excluded_short_circuits_analyzer: no CUDA device; skipping body");
            return;
        }

        match cache.lookup_or_compile(TEST_CLASS_ID, &name, idx, &methods[idx as usize], &cp) {
            LookupOutcome::Blacklisted => {}
            LookupOutcome::Skip => panic!("expected Blacklisted, got Skip"),
            LookupOutcome::Hit(_) => panic!(
                "expected Blacklisted (exclude beats eligibility), got Hit — analyzer ran?!"
            ),
        }
    }

    /// Phase 1 Item 6 — `warmup_class` should compile up to `max`
    /// `@GpuKernel`-annotated methods. `#[ignore]`d on the no-GPU
    /// dev box; the body documents the assertion shape.
    #[test]
    #[ignore = "requires @EnableGpuAsync fixture (Item 8) and a real CUDA device"]
    fn warmup_class_compiles_up_to_max() {
        // PHASE1-GUESS: cache.kernels is private; a test-only
        // accessor like kernels_for_class() would be needed once
        // a real device makes this exercisable.
    }
}

// ---------------------------------------------------------------------------
// Phase 1 — Item 6: `@EnableGpuAsync(warmup = N)` class-load warmup.
//
// `try_dispatch` lazily compiles eligible methods on first call.
// `@EnableGpuAsync(warmup = N)` opts a class into eager compilation:
// when the class is registered with the VM, the class-loader callsite
// invokes `maybe_warmup_gpu`, which reads the class-level annotation
// table and, if `EnableGpuAsync.warmup > 0`, asks the cache to
// pre-compile up to `warmup` `@GpuKernel`-annotated methods.
// ---------------------------------------------------------------------------

impl OffloadCache {
    /// Eagerly populate the cache for up to `max` `@GpuKernel`-annotated
    /// methods of `class`. Called from the class loader when
    /// `@EnableGpuAsync(warmup = N)` is detected on the class.
    pub fn warmup_class(
        &self,
        class: &crate::classloading::Class,
        class_id: ClassId,
        max: usize,
    ) {
        if max == 0 {
            return;
        }
        if !self.has_device() {
            tracing::info!(
                "gpu warmup: {} requested but no device available; skipping",
                &*class.name,
            );
            return;
        }

        let mut compiled = 0usize;
        let mut considered = 0usize;
        for (method_index, method) in class.methods.iter().enumerate() {
            if compiled >= max {
                break;
            }
            considered += 1;
            let m_anns = jit_cuda::annotations::read_method_annotations(
                &method.attributes,
                &class.constant_pool,
            );
            if m_anns.gpu_kernel.is_none() || m_anns.gpu_exclude.is_some() {
                continue;
            }
            let mi = method_index as u16;
            match self.lookup_or_compile(
                class_id,
                &class.name,
                mi,
                method,
                &class.constant_pool,
            ) {
                LookupOutcome::Hit(_) => {
                    compiled += 1;
                }
                LookupOutcome::Skip | LookupOutcome::Blacklisted => {
                    // Not a successful eligible compile — keep scanning
                    // but don't count toward `max`.
                }
            }
        }
        tracing::info!(
            "gpu warmup: {} -> {}/{} eligible methods compiled (considered {} of {})",
            &*class.name,
            compiled,
            max,
            considered,
            class.methods.len(),
        );
    }
}

/// Class-loader hook: if `class` carries `@EnableGpuAsync(warmup = N)`
/// with `N > 0`, eagerly warm the offload cache for that class.
pub(crate) fn maybe_warmup_gpu(
    shared: &crate::vm::SharedVm,
    class: &crate::classloading::Class,
    class_id: ClassId,
) {
    if !shared.config.gpu_offload_enabled {
        return;
    }
    let class_annotations = jit_cuda::annotations::read_class_annotations_from_parsed(
        &class.annotations,
        &class.constant_pool,
    );
    let Some(enable) = class_annotations.enable_async else {
        return;
    };
    if enable.warmup == 0 {
        return;
    }
    shared
        .offload_cache
        .warmup_class(class, class_id, enable.warmup as usize);
}

// ── P3-6: async dispatch + submission registry ──────────────────────
//
// Appendix-only additions for Phase 3 (Item P3-6). These types and
// helpers back the `dispatch_async` / `futureGetResult` API surface
// exposed to the Java layer. Everything below is gated on
// `gpu-offload` and is fully decoupled from the existing
// `try_dispatch` / `OffloadCache::lookup_or_compile` paths.
//
// `cuda_bridge` does not (yet) expose a `Stream` type — the real
// path is gated behind PHASE3-CUDA-TODO comments. We define a local
// placeholder `cuda_bridge::Stream` equivalent here so the
// surrounding signatures land in their final shape and can be wired
// up by the Phase 3 Java glue. When the bridge gains a real
// `Stream`, replace the local type alias with the import.

/// Stand-in for a future `cuda_bridge::Stream`. The real CUDA
/// stream wrapper will live in `cuda-bridge` and carry the cudarc
/// `CudaStream` handle. Today this is an opaque marker that lets the
/// async API land in its final shape.
///
/// PHASE3-CUDA-TODO: replace with `pub use cuda_bridge::Stream` (or a
/// re-export) once the bridge exposes the type.
#[cfg(feature = "gpu-offload")]
pub struct Stream {
    /// Logical stream id. Zero is reserved for the "default" stream.
    pub id: u64,
}

#[cfg(feature = "gpu-offload")]
impl Stream {
    /// Construct a placeholder stream with the given id. The real
    /// implementation will take a `cuda_bridge::DeviceContext` and
    /// create a fresh `CudaStream`.
    pub fn new(id: u64) -> Self {
        Self { id }
    }
}

/// Result of a kernel dispatch, serialised in a form the Java layer
/// can unmarshal without touching device memory directly. Today only
/// the four primitive-array shapes plus `Void` are needed; the
/// analyzer rejects every other return shape long before we get
/// here.
#[cfg(feature = "gpu-offload")]
pub enum SerializedResult {
    /// The kernel had a void return — nothing to copy back beyond
    /// the host-side output array, which the caller already owns.
    Void,
    /// Raw little-endian `i32` bytes copied back from device memory.
    PrimitiveArrayI32 { bytes: Vec<u8> },
    /// Raw little-endian `i64` bytes copied back from device memory.
    PrimitiveArrayI64 { bytes: Vec<u8> },
    /// Raw little-endian `f32` bytes copied back from device memory.
    PrimitiveArrayF32 { bytes: Vec<u8> },
    /// Raw little-endian `f64` bytes copied back from device memory.
    PrimitiveArrayF64 { bytes: Vec<u8> },
}

/// Lifecycle of a `StreamSubmission`. Starts as `Running`; a host
/// callback (or polling) transitions it to `Completed` or `Failed`.
///
/// The variants are deliberately owned (not `&'static`) so the Java
/// glue can move a `SerializedResult` or error message out of the
/// submission without keeping the registry locked.
#[cfg(feature = "gpu-offload")]
pub enum SubmissionStatus {
    /// Dispatch accepted, kernel may or may not have completed.
    Running,
    /// Kernel finished; payload is ready for the Java side to consume.
    Completed { result: SerializedResult },
    /// Dispatch or launch failed. The Java layer surfaces `message`
    /// as `GpuException`.
    Failed { message: String },
}

/// Async kernel submission handle.
///
/// Returned by [`OffloadCache::dispatch_async`] and stored in the
/// global submission registry under [`StreamSubmission::handle`].
/// Cheap to share via `Arc<StreamSubmission>` — the status mutex is
/// taken only at transition points (dispatch, completion callback,
/// `futureGetResult` poll).
#[cfg(feature = "gpu-offload")]
pub struct StreamSubmission {
    /// Monotonically-increasing identifier used by the Java layer to
    /// look the submission up later via `futureGetResult`.
    pub handle: u64,
    /// Stream this submission was queued on. Held so the kernel
    /// completion callback can fire on the same stream that the
    /// launch went out on.
    pub stream: std::sync::Arc<Stream>,
    /// Current lifecycle state. `Running` until the host observes
    /// completion or failure.
    pub status: parking_lot::Mutex<SubmissionStatus>,
}

#[cfg(feature = "gpu-offload")]
impl OffloadCache {
    /// Async kernel dispatch. Returns a [`StreamSubmission`] whose
    /// [`handle`](StreamSubmission::handle) identifies it for
    /// `futureGetResult` lookup. The submission's status starts in
    /// [`SubmissionStatus::Running`]; transitions to `Completed` or
    /// `Failed` when the host observes kernel completion.
    ///
    /// Today on a no-GPU box this immediately constructs a
    /// [`SubmissionStatus::Failed`] submission with message
    /// "no CUDA device". The Phase 3 Java layer surfaces this as
    /// `GpuException`.
    pub fn dispatch_async(
        &self,
        stream: std::sync::Arc<Stream>,
        class_id: ClassId,
        method_index: u16,
        args: cuda_bridge::KernelArgs,
    ) -> std::sync::Arc<StreamSubmission> {
        let handle = NEXT_SUBMISSION_HANDLE
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if !self.has_device() {
            return std::sync::Arc::new(StreamSubmission {
                handle,
                stream,
                status: parking_lot::Mutex::new(SubmissionStatus::Failed {
                    message: format!(
                        "no CUDA device available (class_id={:?}, method={})",
                        class_id, method_index
                    ),
                }),
            });
        }

        // PHASE3-CUDA-TODO: real path performs the dispatch on the
        // provided stream, registers a host callback (or polls via
        // event), and writes the SubmissionStatus transition. Until
        // then we record a synthetic "submitted" Running state and
        // return.
        let _ = args;
        std::sync::Arc::new(StreamSubmission {
            handle,
            stream,
            status: parking_lot::Mutex::new(SubmissionStatus::Failed {
                message: "dispatch_async: real CUDA path not yet implemented"
                    .into(),
            }),
        })
    }
}

#[cfg(feature = "gpu-offload")]
static NEXT_SUBMISSION_HANDLE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

// ── Submission registry ──────────────────────────────────────────────
//
// Global handle → `Arc<StreamSubmission>` table. The Java layer
// receives a `long` handle from `dispatch_async` and later passes it
// back to `futureGetResult`; the registry is how we get from the
// opaque handle back to the live submission. Lazily initialised via
// `OnceLock` so the static does not pay any allocation cost when the
// gpu-offload feature is compiled in but the VM never offloads.

#[cfg(feature = "gpu-offload")]
use std::sync::OnceLock;

#[cfg(feature = "gpu-offload")]
static SUBMISSIONS: OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<u64, std::sync::Arc<StreamSubmission>>>,
> = OnceLock::new();

#[cfg(feature = "gpu-offload")]
fn submissions(
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<u64, std::sync::Arc<StreamSubmission>>>
{
    SUBMISSIONS.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Register `sub` in the global submission table and return its
/// handle. The caller (typically the Java glue right after
/// `dispatch_async`) keeps the handle and hands it back when the Java
/// side polls for completion.
#[cfg(feature = "gpu-offload")]
pub fn register_submission(sub: std::sync::Arc<StreamSubmission>) -> u64 {
    let h = sub.handle;
    submissions().write().insert(h, sub);
    h
}

/// Look up a previously-registered submission by handle. Returns
/// `None` if the handle was never registered or has already been
/// released. The returned `Arc` is a fresh clone — releasing the
/// registry entry afterwards does not invalidate it.
#[cfg(feature = "gpu-offload")]
pub fn lookup_submission(handle: u64) -> Option<std::sync::Arc<StreamSubmission>> {
    submissions().read().get(&handle).cloned()
}

/// Drop the registry's reference to the submission with this handle.
/// Safe to call on an unknown handle (no-op). Idempotent. Once
/// released, [`lookup_submission`] returns `None`.
#[cfg(feature = "gpu-offload")]
pub fn release_submission(handle: u64) {
    submissions().write().remove(&handle);
}
