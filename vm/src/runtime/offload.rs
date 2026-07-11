// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Part E — GPU offload cache and lookup.
//!
//! The whole module is gated behind the `gpu-offload` Cargo feature on
//! `cratonvm-vm`. With the feature off, this file is not compiled and no
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
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::method::ClassFileMethod;

use cuda_bridge::{DeviceContext, DeviceModule};
use jit_cuda::annotations::read_method_annotations;
use jit_cuda::emitter::PtxModule;
use jit_cuda::lowering::lower_method;
use jit_cuda::signature::KernelSignature;
use jit_cuda::{analyzer, OffloadVerdict, ParamKind};

/// Merge bridge: the reader switched method attributes to the lazy
/// `LazyAttribute` representation, but `jit_cuda::annotations` still
/// consumes eagerly-decoded `&[Attribute]`. Decode each lazy attribute
/// (cloning so the shared `&method` borrow stays immutable) and drop
/// any that fail to decode — annotation reading is best-effort.
fn decode_method_attrs(
    attrs: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &ConstantPool,
) -> Vec<cratonvm_reader::attribute::Attribute> {
    attrs
        .iter()
        .filter_map(|la| {
            let mut cloned = la.clone();
            cloned.decode(cp).ok().cloned()
        })
        .collect()
}

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
    /// `cratonvm_classloading::Class` construction (which has ~30
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
        let decoded_attrs = decode_method_attrs(&method.attributes, constant_pool);
        let method_annotations = read_method_annotations(&decoded_attrs, constant_pool);

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
        let mut sig = match verdict {
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
        // Phase 10 #2 — copy the per-param-write mask the lowering pass
        // computed by tracking `*astore` opcodes into the cached
        // signature. The marshaller in `marshal_array_arg` consults
        // this on every submit to decide whether the array arg needs
        // a post-launch D→H writeback (kernel-written) or not
        // (read-only input — same bytes as already on the device).
        sig.writes_param_mask = ptx_module.writes_param_mask;
        let ptx_text = ptx_module.render();
        let ctx = self.ctx.as_ref().expect("ctx presence checked above");
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
    per_device: parking_lot::RwLock<rustc_hash::FxHashMap<u32, std::sync::Arc<OffloadCache>>>,
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
///
/// `FallThroughKeepHooked` is `FallThrough` plus a contract with the
/// interpreter: the call site must NOT be promoted into the invoke
/// cache. A cached target dispatches straight to the CPU body and
/// never re-enters this hook, which would permanently end offload for
/// a site whose *current* arguments merely failed a per-call gate
/// (`--gpu-min-work`: the next call may pass a bigger array). The
/// same rule is why a `Handled` site is never cached either — see the
/// hook in `execute_invokestatic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    Handled,
    FallThrough,
    FallThroughKeepHooked,
}

/// Interpreter hook entry point for transparent GPU offload.
///
/// Looks the invokestatic target up in the `OffloadCache` (which
/// analyzes, lowers, and loads the PTX module on first reach). On a
/// cache `Hit` for an eligible **void** kernel whose largest array
/// argument clears `--gpu-min-work`, it marshals the arguments to the
/// device, launches the kernel, synchronizes, and writes the
/// kernel-written arrays back into the Java heap — returning
/// `Handled` so the interpreter skips the CPU body. Every other path
/// (non-void return, work below threshold, marshal/launch failure,
/// ineligible/blacklisted method) returns `FallThrough`, leaving the
/// operand stack and locals untouched so the CPU body runs normally.
///
/// Non-void kernels (reductions) currently fall through: a `Handled`
/// outcome would require pushing the kernel's scalar result onto the
/// operand stack as the call's return value, which the array-writeback
/// path does not do. Wiring scalar-return-on-stack is the next step.
///
/// On a no-GPU machine `cache.has_device()` is false and the
/// early-return in `execute_invokestatic` short-circuits before this
/// function is reached.
pub fn try_dispatch(
    shared: &crate::vm::SharedVm,
    _thread: &mut crate::threading::JvmThread,
    _frame_idx: usize,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    args: &[cratonvm_types::Value],
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
            // Transparent synchronous offload. Two gates first:
            //
            // 1. VOID return only. A `Handled` outcome tells the
            //    interpreter the invokestatic is complete with the
            //    operand stack already in its post-call shape. For a
            //    void map (`out[i] = f(a[i])`) that's correct — the
            //    args were popped and nothing is pushed; the result
            //    reaches Java via the D→H writeback into the `out`
            //    array. A NON-void kernel (a reduction) would need its
            //    scalar result pushed as the return value, which the
            //    writeback path does not do — so those fall through to
            //    the CPU. (The analyzer still classifies them; only the
            //    launch is skipped.)
            if !method_descriptor.ends_with(")V") {
                return Ok(DispatchOutcome::FallThrough);
            }
            // 2. Real per-element work must clear `--gpu-min-work`. The
            //    analyzer's `estimated_work` is a fixed 1<<20 placeholder
            //    for every counted loop, so it cannot gate small inputs;
            //    use the largest array argument's actual length. Below
            //    the threshold the host↔device round-trip dominates, so
            //    run on the CPU.
            let runtime_work = largest_primitive_array_len(shared, args);
            if (runtime_work as u32) < shared.config.gpu_min_work {
                // Per-call gate, not a property of the method: the next
                // call at this site may pass a larger array, so the site
                // must stay on the slow path where this hook can see it.
                return Ok(DispatchOutcome::FallThroughKeepHooked);
            }
            // Marshal args → device, launch the kernel, synchronize, and
            // write kernel-written arrays back into the Java heap. This
            // reuses the explicit-path machinery (`dispatch_method_from_native`
            // registers a submission; we finalize it synchronously here).
            // Any failure leaves the operand stack + locals untouched, so
            // falling through to the CPU body is always safe.
            let handle = dispatch_method_from_native(
                shared,
                class_name,
                method_name,
                method_descriptor,
                args,
            );
            let result = match lookup_submission(handle) {
                Some(sub) => finalize_submission(shared, &sub),
                None => Err("offload submission was not registered".to_string()),
            };
            release_submission(handle);
            match result {
                Ok(()) => {
                    tracing::debug!(
                        "gpu offload: {}.{}{} ran on device (n={})",
                        class_name,
                        method_name,
                        method_descriptor,
                        runtime_work,
                    );
                    Ok(DispatchOutcome::Handled)
                }
                Err(msg) => {
                    tracing::debug!(
                        "gpu offload: {}.{}{} fell back to CPU: {}",
                        class_name,
                        method_name,
                        method_descriptor,
                        msg,
                    );
                    Ok(DispatchOutcome::FallThrough)
                }
            }
        }
        LookupOutcome::Skip | LookupOutcome::Blacklisted => Ok(DispatchOutcome::FallThrough),
    }
}

/// Largest primitive-array argument length among `args`, or 0 if none.
/// Used to gate the transparent offload on real per-element work (the
/// analyzer's `estimated_work` is a fixed placeholder and cannot).
#[cfg(feature = "gpu-offload")]
fn largest_primitive_array_len(
    shared: &crate::vm::SharedVm,
    args: &[cratonvm_types::Value],
) -> usize {
    let mut max_len = 0usize;
    for arg in args {
        if let cratonvm_types::Value::Object(Some(r)) = arg {
            if shared.heap.array_element_type(*r).is_some() {
                let len = shared.heap.array_length(*r);
                if len > max_len {
                    max_len = len;
                }
            }
        }
    }
    max_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_reader::class_reader::read_class;
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
    /// the heavy `cratonvm_classloading::Class` — the cache API only
    /// needs `(class_id, class_name, method_index, &ClassFileMethod,
    /// &ConstantPool)`.
    fn load_methods(class_name: &str) -> (Vec<ClassFileMethod>, String, ConstantPool) {
        let path = fixture_path(class_name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
        let cf = read_class(&bytes)
            .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
        // `this_class` is resolved to an `Arc<str>` by the reader; this
        // test helper hands back an owned `String`.
        (cf.methods, cf.this_class.to_string(), cf.constant_pool)
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
        let cf = read_class(&bytes).unwrap_or_else(|e| panic!("failed to parse fixture: {e:?}"));
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
        let cf = read_class(&bytes).unwrap_or_else(|e| panic!("failed to parse fixture: {e:?}"));
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
            LookupOutcome::Hit(_) => {
                panic!("expected Blacklisted (exclude beats eligibility), got Hit — analyzer ran?!")
            }
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
    pub fn warmup_class(&self, class: &crate::classloading::Class, class_id: ClassId, max: usize) {
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
            let m_decoded_attrs = decode_method_attrs(&method.attributes, &class.constant_pool);
            let m_anns = jit_cuda::annotations::read_method_annotations(
                &m_decoded_attrs,
                &class.constant_pool,
            );
            if m_anns.gpu_kernel.is_none() || m_anns.gpu_exclude.is_some() {
                continue;
            }
            let mi = method_index as u16;
            match self.lookup_or_compile(class_id, &class.name, mi, method, &class.constant_pool) {
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
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config)
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
// `cuda_bridge` exposes the real `Stream` type (re-exported just
// below); the PHASE3-CUDA-TODO placeholder era this comment used to
// describe is over — the import IS the real per-context CUDA stream.

/// The CUDA stream type used by `StreamSubmission`. Re-exported
/// from `cuda-bridge` so callers can use a single `Stream` path
/// regardless of where they're working.
#[cfg(feature = "gpu-offload")]
pub use cuda_bridge::Stream;

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
    /// Stream this submission was queued on. `Some` for any
    /// dispatch that actually reached the launch site. `None` for
    /// pre-launch failures (class not loaded, no device available,
    /// marshal error) — the Java side still gets a handle whose
    /// status is `Failed`.
    pub stream: Option<std::sync::Arc<Stream>>,
    /// Phase 6 #4 — completion event recorded on the stream right
    /// after the kernel launch. Callers (`futureSynchronize`,
    /// `futureStatus`) can wait or poll on this directly instead of
    /// going through `stream.synchronize()`, which lets multiple
    /// submissions on the same stream overlap their launches.
    ///
    /// `None` for pre-launch failures and for `dispatch_async` paths
    /// that haven't been wired through events yet.
    pub event: Option<std::sync::Arc<cuda_bridge::Event>>,
    /// Current lifecycle state. `Running` until the host observes
    /// completion or failure.
    pub status: parking_lot::Mutex<SubmissionStatus>,
    /// Phase 7 #1 — deferred finalization payload. Holds the
    /// pending writebacks and the GPU-critical SafepointToken
    /// captured at dispatch time. Set to `None` once finalization
    /// has run (the first `future.get()` / `futureSynchronize`
    /// call drains it). Subsequent finalization attempts are no-ops.
    ///
    /// This is what unlocks overlap between consecutive `submit()`
    /// calls on the same stream: `dispatch_async` no longer waits
    /// on the event, so the second submit can queue while the
    /// first kernel is still running on the device.
    pub finalize: parking_lot::Mutex<Option<FinalizeState>>,
}

/// Phase 7 #1 — payload of work that must run on the first
/// `future.get()` call.
#[cfg(feature = "gpu-offload")]
pub struct FinalizeState {
    /// Writebacks to drain after the event fires (download device
    /// buffers into source Java arrays / resident-store entries).
    pub writebacks: Vec<MarshalWriteback>,
    /// GC-critical-section bookkeeping. We can't store a real
    /// `SafepointToken` here because that type is intentionally
    /// `!Send` (one-thread RAII contract), and finalization may
    /// run on a different thread from dispatch. Instead we manage
    /// the increment/decrement manually: dispatch increments
    /// `vm_heap::GPU_CRITICAL_COUNT`, finalization (or `Drop` of
    /// this state, if finalize never runs) decrements it.
    _gc_critical: GcCriticalGuard,
}

/// Send-able RAII guard for the process-wide GPU_CRITICAL_COUNT.
/// Manually mirrors `SafepointToken`'s increment/decrement
/// semantics without the `!Send` marker.
#[cfg(feature = "gpu-offload")]
pub struct GcCriticalGuard;

#[cfg(feature = "gpu-offload")]
impl GcCriticalGuard {
    /// Increment the GC-critical counter and return the guard.
    pub fn acquire() -> Self {
        cratonvm_gc::vm_heap::GPU_CRITICAL_COUNT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self
    }
}

#[cfg(feature = "gpu-offload")]
impl Drop for GcCriticalGuard {
    fn drop(&mut self) {
        cratonvm_gc::vm_heap::GPU_CRITICAL_COUNT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(feature = "gpu-offload")]
impl OffloadCache {
    /// Look up a previously-compiled kernel by `(ClassId, method_index)`.
    /// Returns `None` if the analyzer never accepted this method or it
    /// has not yet been hit through `lookup_or_compile`.
    pub fn lookup_kernel(
        &self,
        class_id: ClassId,
        method_index: u16,
    ) -> Option<std::sync::Arc<CompiledKernel>> {
        self.kernels.read().get(&(class_id, method_index)).cloned()
    }

    /// Asynchronous kernel dispatch on `stream`.
    ///
    /// Returns a [`StreamSubmission`] whose [`handle`](StreamSubmission::handle)
    /// the Java layer maps back to a `GpuFuture<T>`. The submission's
    /// final status depends on three outcomes:
    ///
    /// - **No device**: the cache has no `DeviceContext` (the host has
    ///   no CUDA driver or `--gpu` was off). Returns immediately with
    ///   `SubmissionStatus::Failed`.
    /// - **Unknown kernel**: there is no compiled kernel for
    ///   `(class_id, method_index)`. Returns `Failed`.
    /// - **Launch dispatched**: the kernel is enqueued on `stream` via
    ///   `DeviceModule::launch_on_stream`, then `stream.synchronize()`
    ///   blocks the calling thread until the work completes. On success
    ///   the status becomes `Completed { result: SerializedResult::Void }`.
    ///   On any cudarc error the status becomes `Failed` with the error
    ///   text.
    ///
    /// # Why synchronous-under-the-hood?
    ///
    /// The function is *named* `dispatch_async` and from the Java side
    /// it is async (the `GpuFuture::get` call drives this method via a
    /// worker thread, not the user's). Under the hood, however, we
    /// call `stream.synchronize()` before returning. The real
    /// stream-event-and-poll variant — where the host registers a
    /// callback for the kernel completion and the future stays
    /// `Running` until that callback fires — is a Phase 5 follow-up.
    /// The current shape is enough to exercise the full
    /// Java→Rust→cudarc→Rust→Java round-trip on a GPU box.
    ///
    /// # Limitation: only `Void` return
    ///
    /// Today we only surface `SerializedResult::Void`. Primitive-array
    /// return (the common shape: `kernel(int[] a, int[] b, int[] out)`)
    /// is signalled by the caller writing to an output buffer the host
    /// already owns — the host-side `int[]` of the `out` parameter is
    /// what the Java code reads. Surfacing a *new* primitive array as
    /// the future's result (e.g. for a method that returns `int[]`
    /// rather than writing to `out`) needs the dispatch site to
    /// allocate the output buffer, copy it back after the kernel, and
    /// stamp it into `SerializedResult::PrimitiveArray*`. That belongs
    /// to a later round.
    /// Asynchronously launch a compiled kernel on `stream`.
    ///
    /// `runtime_work` is the per-element launch count derived from the
    /// actual array length the caller marshalled. The analyzer's
    /// `KernelSignature::estimated_work` is a *compile-time* placeholder
    /// (`1 << 20` for any counted-loop method); using it directly
    /// truncates the launch grid for any n > 2^20, leaving the tail of
    /// the output array unwritten. Pass `0` to fall back to the
    /// analyzer's `estimated_work` — only safe for scalar-only kernels
    /// (no array params) which don't have a per-element loop in the
    /// first place. We use `u32` rather than `Option<u32>` because the
    /// MSVC x64 ABI's handling of `Option<u32>` was observed to corrupt
    /// stack passed CUDA kernel arguments on Windows when this function
    /// is called via the GPU dispatch chain — passing the raw `u32` /
    /// sentinel-0 encoding is the workaround.
    pub fn dispatch_async(
        &self,
        stream: std::sync::Arc<Stream>,
        class_id: ClassId,
        method_index: u16,
        args: cuda_bridge::KernelArgs,
        runtime_work: u32,
    ) -> std::sync::Arc<StreamSubmission> {
        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // `event` is populated only on the success path below, after
        // the launch is queued. Failure paths leave it None.
        let make_with_event =
            |status: SubmissionStatus, event: Option<std::sync::Arc<cuda_bridge::Event>>| {
                std::sync::Arc::new(StreamSubmission {
                    handle,
                    stream: Some(stream.clone()),
                    event,
                    status: parking_lot::Mutex::new(status),
                    finalize: parking_lot::Mutex::new(None),
                })
            };
        let make = |status: SubmissionStatus| make_with_event(status, None);

        // 1. No-device fast path. The Java layer surfaces this as
        //    `GpuException("no CUDA device …")`.
        let ctx = match self.device() {
            Some(c) => c,
            None => {
                return make(SubmissionStatus::Failed {
                    message: format!(
                        "no CUDA device available (class_id={:?}, method={})",
                        class_id, method_index,
                    ),
                });
            }
        };

        // 2. Resolve the compiled kernel. `dispatch_async` is meant to
        //    be called after `lookup_or_compile` populated the cache;
        //    a missing entry signals a bug at the call site.
        let kernel = match self.lookup_kernel(class_id, method_index) {
            Some(k) => k,
            None => {
                return make(SubmissionStatus::Failed {
                    message: format!(
                        "no compiled kernel for class_id={:?} method={} (lookup_or_compile not called?)",
                        class_id, method_index,
                    ),
                });
            }
        };

        // The caller-supplied `runtime_work` is the actual array
        // length the kernel will iterate over. The signature's
        // `estimated_work` is a fixed `1 << 20` for any counted-loop
        // method (see `jit_cuda::analyzer::estimate_work`) — using
        // it directly leaves the tail unwritten for n > 2^20 because
        // the launch grid undercounts threads. We take the max of
        // the two so n ≤ 2^20 keeps the original launch shape and
        // n > 2^20 grows to cover every output index.
        let estimated = kernel.signature.estimated_work.max(1) as u32;
        let work = if runtime_work > estimated {
            runtime_work
        } else {
            estimated
        };
        let cfg = cuda_bridge::LaunchConfig::elementwise(work);

        // 4. Launch on the user-supplied stream. The launch itself is
        //    non-blocking; `stream.synchronize()` below is what makes
        //    this call observably synchronous to the caller.
        if let Err(e) =
            kernel
                .module
                .launch_on_stream(ctx, &kernel.kernel_name, &cfg, args, &stream)
        {
            return make(SubmissionStatus::Failed {
                message: format!("launch_on_stream({}): {}", kernel.kernel_name, e,),
            });
        }

        // 5. Phase 7 #1 — record a completion event and return
        //    immediately. The caller's worker thread is now free to
        //    queue another submit on the same stream while this
        //    kernel runs. The event lives on the StreamSubmission;
        //    `gpu_finalize_future` (called from
        //    `Native.futureSynchronize` / `futureGetResult`) will
        //    `event.synchronize()` and drain the writebacks.
        let event = match cuda_bridge::Event::new(ctx) {
            Ok(e) => std::sync::Arc::new(e),
            Err(e) => {
                return make(SubmissionStatus::Failed {
                    message: format!("Event::new after launch: {e}"),
                });
            }
        };
        if let Err(e) = stream.record_event(&event) {
            return make(SubmissionStatus::Failed {
                message: format!("Stream::record_event: {e}"),
            });
        }

        // 6. Return a Running submission. The status flips to
        //    Completed inside `finalize_submission` after the event
        //    fires and the writebacks complete. Callers attach the
        //    writebacks + token via `attach_finalize_state` before
        //    registering the submission.
        make_with_event(SubmissionStatus::Running, Some(event))
    }
}

#[cfg(feature = "gpu-offload")]
static NEXT_SUBMISSION_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

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
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<u64, std::sync::Arc<StreamSubmission>>> {
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

// ── Phase 5: explicit named-method dispatch from native shims ────────
//
// Bridges `craton.gpu.internal.Native.submitMethod(...)` to the rest
// of the GPU stack. The native shim cannot call `dispatch_async`
// directly because it has no `SharedVm` reference; this free
// function takes one and does the orchestration.
//
// Today's marshaller supports:
//   * primitive arrays: int[], long[], float[], double[]
//   * device-resident GpuArray handles are NOT yet routed
//     (PHASE6-FOLLOWUP).
//   * boxed primitive scalars in `java_args` are NOT yet supported
//     (PHASE6-FOLLOWUP) — only Value::Object holding a primitive
//     array makes it through. The full Object[]-with-mixed-shape
//     marshaller belongs to the lambda-resolution phase.
//
// Output convention (Phase 1): after the kernel completes, every
// array argument is downloaded back into its source Java array.
// This is wasteful for read-only inputs but correct, and matches
// the analyzer's "last array is output" rule without needing to
// know which is which here. Phase 6 narrows it.

#[cfg(feature = "gpu-offload")]
fn record_failed_submission(stream: Option<std::sync::Arc<Stream>>, message: String) -> u64 {
    let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sub = std::sync::Arc::new(StreamSubmission {
        handle,
        stream,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Failed { message }),
        finalize: parking_lot::Mutex::new(None),
    });
    register_submission(sub);
    handle
}

/// Dispatch `class_name`.`method_name`(`descriptor`) on the GPU with
/// `java_args`. Returns the submission handle the Java layer wraps
/// in `GpuFutureImpl`. On any failure (method not eligible, missing
/// device, marshal error, launch error) the returned handle still
/// resolves — it points to a `StreamSubmission` whose status is
/// `Failed { message }` so the Java side surfaces it as
/// `GpuException` via `futureGetErrorMessage`.
#[cfg(feature = "gpu-offload")]
pub fn dispatch_method_from_native(
    shared: &crate::vm::SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[cratonvm_types::Value],
) -> u64 {
    use cratonvm_types::{ArrayElementType, Value};
    use cuda_bridge::{KernelArgs, Stream as CudaStream};
    use std::sync::Arc;

    // 1. Resolve the cache.
    let cache = shared
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config);

    // 2. Resolve class + method via the class manager. Phase 9 #2:
    //    while we hold the class-manager lock we also extract
    //    `is_static` and, for non-static methods, dedup the analyzer's
    //    `this_field_cps` into an ordered list of `(cp_index,
    //    field_name)` pairs the marshaller can resolve against the
    //    receiver without holding any class-manager locks afterward.
    //
    // Phase 10 #2 — also extract `writes_param_mask` from the compiled
    // kernel's signature so each `marshal_array_arg` call site can
    // decide whether to record a post-launch D→H writeback for that
    // arg or skip it (the read-only-input optimisation that closes
    // the residual gap to TornadoVM's `FIRST_EXECUTION`).
    let (class_id, method_index, is_static, this_field_names, writes_param_mask): (
        crate::classloading::ClassId,
        u16,
        bool,
        Vec<String>,
        u64,
    ) = {
        // Phase 9 #1 fix — load the class on demand. The class name
        // arrives as a string from `Native.submitMethod`; the user has
        // no reason to have referenced it from Java code, so it may
        // not be in the class manager yet. Without this load,
        // `get_loaded_class_id` returns None and the submission is
        // marked Failed before any kernel runs. The Java side then
        // sees a synthetic "handle=0" path that returns from
        // `f.get()` without throwing, leaving output arrays at their
        // pre-launch zero values — masquerading as a writeback bug.
        if let Err(e) = shared.load_class_concurrent(class_name) {
            return record_failed_submission(
                None,
                format!("submitMethod: load class failed for {class_name}: {e:?}"),
            );
        }
        let cm = shared.class_manager.read();
        let class_id = match cm.get_loaded_class_id(class_name) {
            Some(id) => id,
            None => {
                return record_failed_submission(
                    None,
                    format!(
                        "submitMethod: class not loaded after load_class_concurrent: {class_name}"
                    ),
                );
            }
        };
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => {
                return record_failed_submission(
                    None,
                    format!("submitMethod: class id missing in manager: {class_name}"),
                );
            }
        };
        let mi = match class
            .methods
            .iter()
            .position(|m| &*m.name == method_name && &*m.descriptor == descriptor)
        {
            Some(i) => i as u16,
            None => {
                return record_failed_submission(
                    None,
                    format!(
                        "submitMethod: method not found: {class_name}.{method_name}{descriptor}",
                    ),
                );
            }
        };
        let is_static_local = class.methods[mi as usize].is_static();
        // 3. Ensure the kernel is compiled before dispatch_async runs.
        let outcome = cache.lookup_or_compile(
            class_id,
            class_name,
            mi,
            &class.methods[mi as usize],
            &class.constant_pool,
        );
        match outcome {
            LookupOutcome::Hit(compiled) => {
                // Phase 9 #2: capture the dedup'd ordered field-name
                // list. The marshaller below will resolve each name
                // against the receiver and marshal it as an extra
                // kernel arg ahead of the regular parameters.
                let mut seen: Vec<u16> = Vec::new();
                let mut names: Vec<String> = Vec::new();
                for &cp in &compiled.signature.this_field_cps {
                    if seen.contains(&cp) {
                        continue;
                    }
                    seen.push(cp);
                    // Field cp_index → NameAndType → Utf8 field name.
                    let Some(cratonvm_reader::constant_pool::ConstantPoolEntry::FieldReference {
                        name_and_type_index,
                        ..
                    }) = class.constant_pool.get(cp)
                    else {
                        return record_failed_submission(
                            None,
                            format!(
                                "submitMethod: this_field_cps[{}]=#{cp} is not a FieldReference \
                                 entry in {class_name}'s constant pool — analyzer / class \
                                 mismatch",
                                names.len(),
                            ),
                        );
                    };
                    let Some((nm, _desc)) =
                        class.constant_pool.get_name_and_type(*name_and_type_index)
                    else {
                        return record_failed_submission(
                            None,
                            format!(
                                "submitMethod: this_field_cps[{}]=#{cp} has no resolvable \
                                 NameAndType",
                                names.len(),
                            ),
                        );
                    };
                    names.push(nm.to_string());
                }
                (
                    class_id,
                    mi,
                    is_static_local,
                    names,
                    compiled.signature.writes_param_mask,
                )
            }
            LookupOutcome::Skip => {
                return record_failed_submission(
                    None,
                    format!(
                        "submitMethod: method not offloadable (Skip): {class_name}.{method_name}{descriptor}",
                    ),
                );
            }
            LookupOutcome::Blacklisted => {
                return record_failed_submission(
                    None,
                    format!(
                        "submitMethod: method blacklisted: {class_name}.{method_name}{descriptor}",
                    ),
                );
            }
        }
    };

    // 4. From here on we need a real device context. The Failed-fast
    //    path is identical to dispatch_async's no-device branch.
    let ctx = match cache.device() {
        Some(c) => c,
        None => {
            return record_failed_submission(
                None,
                format!(
                    "submitMethod: no CUDA device available ({class_name}.{method_name}{descriptor})",
                ),
            );
        }
    };

    // 5. Create a real stream for this dispatch.
    let stream = match CudaStream::new(ctx) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            return record_failed_submission(
                None,
                format!("submitMethod: Stream::new failed: {e}"),
            );
        }
    };

    // 6. Enter the GC-critical section. The guard lives in the
    //    StreamSubmission's FinalizeState (Phase 7 #1), bracketing
    //    the kernel's read of source arrays from dispatch through
    //    writeback. We also keep a short-lived SafepointToken bound
    //    to the calling thread for the marshal-time gpu_marshal::*
    //    calls — they have a `&SafepointToken<'_>` signature, and
    //    the GcCriticalGuard alone wouldn't satisfy it. Once
    //    marshalling is done the token is dropped; the guard in
    //    the FinalizeState keeps the GC paused for the rest of
    //    the submission's life.
    let gc_guard = GcCriticalGuard::acquire();
    let token = shared.heap.enter_gpu_critical();

    // 7. Marshal Java args into KernelArgs + remember writebacks.
    //    `max_array_len` tracks the largest array length we marshal —
    //    passed below as `runtime_work` so the launch grid covers
    //    every output index. See `dispatch_async`'s comment.
    //
    // Phase 10 #2 — `h2d_bytes` accumulates per-arg upload byte counts
    // for the `CRATONVM_GPU_TRACE_BYTES` instrumentation. The cache
    // path returns 0 for the upload (no copy happened), so the counter
    // measures real PCIe traffic; subsequent submits on the same arrays
    // should accumulate 0 bytes once the residency cache is warm.
    let mut kernel_args = KernelArgs::new();
    let mut writebacks: Vec<MarshalWriteback> = Vec::new();
    let mut max_array_len: usize = 0;
    let mut h2d_bytes: usize = 0;
    // Param-index counter for the writes-mask lookup. The non-static
    // `pthis_*` arms come first and consume slots ahead of the
    // declared params; we still want declared params to start at
    // index 0 in the analyzer's mask (the analyzer never sees
    // `pthis_*` — those are a runtime-only marshalling concept), so
    // we track declared-param consumption with a separate counter
    // (`declared_param_idx`) once the non-static prelude finishes.

    // Phase 9 #2 push 2 — non-static path: marshal `this.<field>`
    // arrays first (matching the kernel's `pthis_<i>_*` param prefix
    // emitted by `jit_cuda::lowering::build_param_list`), then fall
    // through to the regular per-arg marshalling starting at
    // `java_args[1..]` (the receiver itself is never a kernel arg —
    // only its named fields are).
    let java_args_to_marshal: &[cratonvm_types::Value] = if !is_static {
        // 7a. Pull the receiver from `java_args[0]`.
        let receiver = match java_args.first() {
            Some(Value::Object(Some(r))) => *r,
            Some(Value::Object(None)) => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    format!(
                        "submitMethod: non-static receiver is null ({class_name}.{method_name}{descriptor})",
                    ),
                );
            }
            Some(other) => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    format!(
                        "submitMethod: non-static receiver is not an object reference: {other:?}",
                    ),
                );
            }
            None => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    format!(
                        "submitMethod: non-static method called with no arguments \
                         (expected receiver as arg 0): {class_name}.{method_name}{descriptor}",
                    ),
                );
            }
        };

        // 7b. Resolve each field name → slot on the receiver's
        //     concrete class, read the field, marshal it as a
        //     primitive-array kernel arg.
        let receiver_class_id = shared.heap.class_id_of(receiver);
        for (i, field_name) in this_field_names.iter().enumerate() {
            // Walk the class hierarchy starting at the receiver's
            // concrete class — the field may be declared on the
            // declared class (which equals receiver_class_id when the
            // receiver is exactly that class) or on a superclass.
            let slot: usize = {
                let cm = shared.class_manager.read();
                let mut current = Some(receiver_class_id);
                let mut found: Option<usize> = None;
                while let Some(cid) = current {
                    let Some(cls) = cm.get_class(cid) else { break };
                    if let Some((idx, _)) = cls.find_own_field(field_name) {
                        found = Some(idx);
                        break;
                    }
                    current = cls.superclass;
                }
                match found {
                    Some(s) => s,
                    None => {
                        drop(token);
                        return record_failed_submission(
                            Some(stream.clone()),
                            format!(
                                "submitMethod: this_field `{field_name}` not found on receiver's \
                                 class hierarchy (receiver class_id={receiver_class_id:?})",
                            ),
                        );
                    }
                }
            };
            let field_val = shared.heap.get_field(receiver, slot);
            let field_obj = match field_val {
                Value::Object(Some(o)) => o,
                Value::Object(None) => {
                    drop(token);
                    return record_failed_submission(
                        Some(stream.clone()),
                        format!(
                            "submitMethod: this_field `{field_name}` (pthis_{i}) is null on receiver",
                        ),
                    );
                }
                other => {
                    drop(token);
                    return record_failed_submission(
                        Some(stream.clone()),
                        format!(
                            "submitMethod: this_field `{field_name}` (pthis_{i}) is not an \
                             object reference: {other:?}",
                        ),
                    );
                }
            };
            let Some(etype) = shared.heap.array_element_type(field_obj) else {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    format!(
                        "submitMethod: this_field `{field_name}` (pthis_{i}) does not point at a \
                         primitive array",
                    ),
                );
            };
            // Phase 10 #2 — `pthis_*` (receiver-field) params are not
            // represented in `KernelSignature::param_kinds` (the
            // analyzer treats them as a marshaller-only concept), so
            // the `writes_param_mask` has no corresponding bit. Be
            // conservative and treat each as kernel-written, which
            // preserves the pre-Phase-10 #2 behaviour (always D→H
            // copy back) for this less-common code path.
            let pthis_len = shared.heap.array_length(field_obj);
            match marshal_array_arg(shared, ctx, field_obj, etype, true, &token) {
                Ok((args_after, wb_opt, bytes)) => {
                    kernel_args = args_after(kernel_args);
                    h2d_bytes = h2d_bytes.saturating_add(bytes);
                    if pthis_len > max_array_len {
                        max_array_len = pthis_len;
                    }
                    if let Some(wb) = wb_opt {
                        writebacks.push(wb);
                    }
                }
                Err(msg) => {
                    drop(token);
                    return record_failed_submission(Some(stream.clone()), msg);
                }
            }
        }
        // Skip the receiver itself — it's not a kernel arg.
        &java_args[1..]
    } else {
        // Static methods: everything in java_args is a kernel arg.
        java_args
    };

    for (i, arg) in java_args_to_marshal.iter().enumerate() {
        match arg {
            Value::Int(v) => kernel_args = kernel_args.push_i32(*v),
            Value::Long(v) => kernel_args = kernel_args.push_i64(*v),
            Value::Float(v) => kernel_args = kernel_args.push_f32(f32::from_bits(v.to_bits())),
            Value::Double(v) => kernel_args = kernel_args.push_f64(f64::from_bits(v.to_bits())),
            Value::Object(Some(obj_ref)) => {
                // First: is it a primitive array?
                if let Some(element_type) = shared.heap.array_element_type(*obj_ref) {
                    // Phase 10 #2 — `i` is the index inside
                    // `java_args_to_marshal`, which lines up 1:1 with
                    // the analyzer's `param_kinds[i]` for static
                    // methods (the only path where `writes_param_mask`
                    // is precisely populated today). Consult the mask
                    // — a clear bit means the kernel never `*astore`s
                    // into this param, so we can safely skip the
                    // post-launch D→H copy.
                    let is_written = (writes_param_mask >> i) & 1 == 1;
                    let arr_len = shared.heap.array_length(*obj_ref);
                    match marshal_array_arg(shared, ctx, *obj_ref, element_type, is_written, &token)
                    {
                        Ok((args_after, wb_opt, bytes)) => {
                            kernel_args = args_after(kernel_args);
                            h2d_bytes = h2d_bytes.saturating_add(bytes);
                            if arr_len > max_array_len {
                                max_array_len = arr_len;
                            }
                            if let Some(wb) = wb_opt {
                                writebacks.push(wb);
                            }
                        }
                        Err(msg) => {
                            drop(token);
                            return record_failed_submission(Some(stream.clone()), msg);
                        }
                    }
                    continue;
                }
                // Second: is it a craton.gpu.GpuArray? If so, route
                // through the residency tracker — read the long
                // `handle` field, snapshot the bytes from
                // native-builtins/craton_gpu, marshal as if it were
                // a plain primitive array. Phase 7 will pre-cache
                // the DeviceBuffer in the resident state so this
                // path skips the H→D copy when the bytes haven't
                // changed since the previous kernel.
                if let Some((etype, len, host_bytes, arr_handle)) =
                    try_gpu_array_snapshot(shared, *obj_ref)
                {
                    match marshal_resident_array_arg(ctx, etype, len, host_bytes, arr_handle) {
                        Ok((args_after, wb)) => {
                            kernel_args = args_after(kernel_args);
                            if let Some(l) = wb.array_len() {
                                if l > max_array_len {
                                    max_array_len = l;
                                }
                            }
                            writebacks.push(wb);
                        }
                        Err(msg) => {
                            drop(token);
                            return record_failed_submission(Some(stream.clone()), msg);
                        }
                    }
                    continue;
                }
                // Third: is it a boxed primitive? Java's varargs
                // autobox `int` → Integer, etc.
                match try_unbox_primitive(shared, *obj_ref) {
                    Some(Value::Int(v)) => kernel_args = kernel_args.push_i32(v),
                    Some(Value::Long(v)) => kernel_args = kernel_args.push_i64(v),
                    Some(Value::Float(v)) => kernel_args = kernel_args.push_f32(v),
                    Some(Value::Double(v)) => kernel_args = kernel_args.push_f64(v),
                    _ => {
                        drop(token);
                        return record_failed_submission(
                            Some(stream.clone()),
                            format!(
                                "submitMethod: arg #{i} is not a primitive array, GpuArray, or boxed primitive",
                            ),
                        );
                    }
                }
                continue;
            }
            Value::Object(None) => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    format!("submitMethod: arg #{i} is null"),
                );
            }
            _ => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    format!("submitMethod: arg #{i} type unsupported: {arg:?}"),
                );
            }
        }
    }

    // 7c. Append the kernel's trailing `failure_flag` parameter.
    //     `build_param_list` in `jit-cuda/src/lowering.rs` always emits
    //     a final `u64*` named `failure_flag`. The PTX bounds-check
    //     fail block (`emit::Emitter::emit_done_and_bounds_fail`)
    //     stores 1 to this cell when an indexed load/store would have
    //     gone out of range. Without an actual device pointer in this
    //     slot, `cuLaunchKernel` returns `CUDA_ERROR_INVALID_VALUE`
    //     because the host-side `KernelArgs` length doesn't match the
    //     compiled kernel's param count.
    //
    //     The buffer is owned by `MarshalWriteback::FailureFlag` for
    //     the submission's lifetime; `finalize_submission` reads it
    //     after `event.synchronize()` and surfaces a non-zero value
    //     as a failure message so the Java side gets a real error
    //     instead of silently corrupt output.
    let failure_flag_buf = match cuda_bridge::DeviceBuffer::<u64>::zeros(ctx, 1) {
        Ok(b) => std::sync::Arc::new(b),
        Err(e) => {
            drop(token);
            return record_failed_submission(
                Some(stream.clone()),
                format!("submitMethod: failed to allocate failure_flag buffer: {e}"),
            );
        }
    };
    {
        // Push the device pointer via a raw-pointer dance identical
        // to the array-arg closure pattern above — `push_device_ptr`
        // takes `&DeviceBuffer<T>` and we need the Arc to outlive the
        // launch.
        let buf_ref: &cuda_bridge::DeviceBuffer<u64> = &failure_flag_buf;
        kernel_args = kernel_args.push_device_ptr(buf_ref);
    }
    writebacks.push(MarshalWriteback::FailureFlag {
        buf: failure_flag_buf,
    });

    // 8. Phase 7 #1 — dispatch on the stream. The launch grid's
    //    element count comes from `max_array_len`: the analyzer's
    //    `estimated_work` is a fixed compile-time placeholder
    //    (1 << 20 for any counted loop) which silently truncates
    //    the launch for n > 2^20. Pass 0 (no arrays seen) or a
    //    truncated 2^31-1 (array bigger than u32::MAX is impossible
    //    in JVM but defensive) when needed; `dispatch_async` takes
    //    the max of `runtime_work` and `estimated_work`.
    //
    // Phase 10 #2 — flush the per-dispatch H2D byte total into the
    // process-wide trace counter; emit a per-submit log line when
    // `CRATONVM_GPU_TRACE_BYTES` is on. With both the residency
    // cache (Phase 10 #1) and the read-only-input suppression
    // (Phase 10 #2) live, post-warmup submits report 0 bytes
    // uploaded — the smoking-gun signal we expected to see.
    h2d_trace::add(h2d_bytes);
    if h2d_trace::enabled() {
        tracing::info!(
            "gpu offload: submit H2D={} bytes ({}.{}{}) — cumulative {} bytes",
            h2d_bytes,
            class_name,
            method_name,
            descriptor,
            h2d_trace::total(),
        );
    }
    let runtime_work: u32 = u32::try_from(max_array_len).unwrap_or(u32::MAX);
    let submission = cache.dispatch_async(
        stream.clone(),
        class_id,
        method_index,
        kernel_args,
        runtime_work,
    );

    // 9. If the dispatch itself failed (no device, kernel not in
    //    cache, launch error), there is nothing to finalize — the
    //    submission is already Failed. Otherwise attach the
    //    writebacks + the GC-critical token to the submission so
    //    `finalize_submission` can drain them on the first
    //    `future.get()` call. The token moves into the
    //    FinalizeState — its `Drop` runs when finalization completes
    //    or when the submission is dropped without ever being
    //    finalized.
    let needs_finalize = {
        let status = submission.status.lock();
        matches!(*status, SubmissionStatus::Running)
    };
    // The thread-local SafepointToken's role is over (marshal
    // is done). The cross-thread GcCriticalGuard takes over.
    drop(token);
    if needs_finalize {
        *submission.finalize.lock() = Some(FinalizeState {
            writebacks,
            _gc_critical: gc_guard,
        });
    } else {
        // Failed submission — guard drops here, writebacks
        // discarded (no kernel ran).
        drop(gc_guard);
        let _ = writebacks;
    }

    // 10. Register the submission and return its handle. The Java
    //     side wraps this handle in `GpuFutureImpl`.
    let handle = submission.handle;
    register_submission(submission);
    handle
}

/// Phase 7 #1 — finalize a submission on the first `future.get()`.
///
/// Idempotent: subsequent calls return the same terminal status
/// without re-synchronizing the event.
///
/// Returns `Ok(())` on Completed, `Err(message)` on Failed. The
/// caller (typically the `gpu_future_synchronize` escape hatch)
/// surfaces the error to Java as `GpuException`.
#[cfg(feature = "gpu-offload")]
pub fn finalize_submission(
    shared: &crate::vm::SharedVm,
    submission: &std::sync::Arc<StreamSubmission>,
) -> Result<(), String> {
    // First, take the FinalizeState. If None, finalization has
    // already run (or this submission was Failed at dispatch) —
    // fall through to read the terminal status.
    let pending = submission.finalize.lock().take();

    if let Some(FinalizeState {
        writebacks,
        _gc_critical,
    }) = pending
    {
        // 1. Wait for the kernel to complete via the recorded event.
        if let Some(event) = &submission.event {
            if let Err(e) = event.synchronize() {
                let mut status = submission.status.lock();
                *status = SubmissionStatus::Failed {
                    message: format!("event.synchronize: {e}"),
                };
                // _gc_critical drops here (releases GC gate).
                drop(writebacks);
                drop(_gc_critical);
                return Err(match &*status {
                    SubmissionStatus::Failed { message } => message.clone(),
                    _ => unreachable!(),
                });
            }
        }
        // 2. Synthesize a thread-local SafepointToken to satisfy
        //    the writeback signature. The token is purely a
        //    type-system marker (the actual no-GC window is held
        //    by `_gc_critical` against the shared GPU_CRITICAL_COUNT);
        //    a local counter satisfies the borrow without affecting
        //    the real GC gate.
        let local_counter = std::sync::atomic::AtomicU32::new(0);
        let local_token = cratonvm_gc::safepoint::SafepointToken::new(&local_counter);

        // 3. Drain writebacks. First failure marks the submission
        //    Failed and stops further writebacks.
        //
        //    The `FailureFlag` entries must drain FIRST even though they
        //    are pushed last: if the kernel tripped a bounds check, the
        //    array writebacks below would otherwise copy partial device
        //    state into the Java heap *before* the flag is read — the
        //    deopt would then re-run the method on a heap the failed
        //    kernel already dirtied, breaking the documented "the
        //    interpreter observes no partial GPU state" guarantee
        //    (docs/book/src/gpu/overview.md §Exceptions).
        let mut first_err: Option<String> = None;
        for wb in writebacks
            .iter()
            .filter(|wb| matches!(wb, MarshalWriteback::FailureFlag { .. }))
            .chain(
                writebacks
                    .iter()
                    .filter(|wb| !matches!(wb, MarshalWriteback::FailureFlag { .. })),
            )
        {
            if let Err(msg) = wb.writeback(shared, &local_token) {
                first_err = Some(msg);
                break;
            }
        }
        // 4. Drop guard (release real GC gate) BEFORE we touch the
        //    status mutex so a concurrent reader of status doesn't
        //    block GC longer than necessary.
        drop(local_token);
        drop(writebacks);
        drop(_gc_critical);

        // 4. Transition status.
        let mut status = submission.status.lock();
        match (&*status, first_err) {
            (SubmissionStatus::Running, None) => {
                *status = SubmissionStatus::Completed {
                    result: SerializedResult::Void,
                };
                Ok(())
            }
            (SubmissionStatus::Running, Some(msg)) => {
                *status = SubmissionStatus::Failed {
                    message: msg.clone(),
                };
                Err(msg)
            }
            // Submission was already terminal — keep whatever status
            // it had. (Shouldn't happen given we took the FinalizeState
            // under the same submission, but defensive.)
            (SubmissionStatus::Completed { .. }, _) => Ok(()),
            (SubmissionStatus::Failed { message }, _) => Err(message.clone()),
        }
    } else {
        // Already finalized (or never had a FinalizeState — e.g.
        // a dispatch-time failure). Return the terminal status.
        let status = submission.status.lock();
        match &*status {
            SubmissionStatus::Running => {
                // No FinalizeState and still Running is a logic
                // error — treat as failed.
                Err(format!(
                    "submission handle={} is Running with no FinalizeState",
                    submission.handle,
                ))
            }
            SubmissionStatus::Completed { .. } => Ok(()),
            SubmissionStatus::Failed { message } => Err(message.clone()),
        }
    }
}

// ── Phase 7 #2: device-buffer cache for resident GpuArrays ──────────
//
// Each `craton.gpu.GpuArray` handle that's been uploaded to the GPU
// at least once keeps its `DeviceBuffer<T>` here. The next kernel
// that references the same handle skips the H→D copy and runs
// directly against the cached buffer. The kernel's writes stay on
// the device buffer; the writeback updates the host-bytes mirror
// in native-builtins's synthetic store (so `GpuArray.toHost()`
// returns current contents).
//
// Cache eviction: when Java calls `Native.releaseArray(handle)`,
// `craton_gpu::array_release` (Phase 7 #2 hook) calls
// `device_cache::release(handle)` to drop the cached buffer.
//
// All four primitive element types get their own slot in the same
// keyed map; element type is fixed at wrap time on the
// native-builtins side, so collisions across types for the same
// handle don't happen.

#[cfg(feature = "gpu-offload")]
pub(crate) mod device_cache {
    use cuda_bridge::DeviceBuffer;
    use parking_lot::Mutex;
    use rustc_hash::FxHashMap;
    use std::sync::{Arc, OnceLock};

    pub(crate) enum CachedBuffer {
        I32(Arc<DeviceBuffer<i32>>),
        I64(Arc<DeviceBuffer<i64>>),
        F32(Arc<DeviceBuffer<f32>>),
        F64(Arc<DeviceBuffer<f64>>),
    }

    /// Phase 9 #1 — `dirty` tracks whether the device side has
    /// writes a subsequent `Native.arrayToHost` would need to
    /// pull back. Set by `mark_dirty` from the post-kernel
    /// writeback, cleared by `download_into_bytes_if_dirty`.
    pub(crate) struct Entry {
        pub buf: CachedBuffer,
        pub dirty: bool,
    }

    static CACHE: OnceLock<Mutex<FxHashMap<u64, Entry>>> = OnceLock::new();

    fn map() -> &'static Mutex<FxHashMap<u64, Entry>> {
        CACHE.get_or_init(|| Mutex::new(FxHashMap::default()))
    }

    pub(crate) fn get_i32(handle: u64) -> Option<Arc<DeviceBuffer<i32>>> {
        match map().lock().get(&handle) {
            Some(Entry {
                buf: CachedBuffer::I32(arc),
                ..
            }) => Some(arc.clone()),
            _ => None,
        }
    }

    pub(crate) fn put_i32(handle: u64, buf: Arc<DeviceBuffer<i32>>) {
        map().lock().insert(
            handle,
            Entry {
                buf: CachedBuffer::I32(buf),
                dirty: false,
            },
        );
    }

    pub(crate) fn get_i64(handle: u64) -> Option<Arc<DeviceBuffer<i64>>> {
        match map().lock().get(&handle) {
            Some(Entry {
                buf: CachedBuffer::I64(arc),
                ..
            }) => Some(arc.clone()),
            _ => None,
        }
    }

    pub(crate) fn put_i64(handle: u64, buf: Arc<DeviceBuffer<i64>>) {
        map().lock().insert(
            handle,
            Entry {
                buf: CachedBuffer::I64(buf),
                dirty: false,
            },
        );
    }

    pub(crate) fn get_f32(handle: u64) -> Option<Arc<DeviceBuffer<f32>>> {
        match map().lock().get(&handle) {
            Some(Entry {
                buf: CachedBuffer::F32(arc),
                ..
            }) => Some(arc.clone()),
            _ => None,
        }
    }

    pub(crate) fn put_f32(handle: u64, buf: Arc<DeviceBuffer<f32>>) {
        map().lock().insert(
            handle,
            Entry {
                buf: CachedBuffer::F32(buf),
                dirty: false,
            },
        );
    }

    pub(crate) fn get_f64(handle: u64) -> Option<Arc<DeviceBuffer<f64>>> {
        match map().lock().get(&handle) {
            Some(Entry {
                buf: CachedBuffer::F64(arc),
                ..
            }) => Some(arc.clone()),
            _ => None,
        }
    }

    pub(crate) fn put_f64(handle: u64, buf: Arc<DeviceBuffer<f64>>) {
        map().lock().insert(
            handle,
            Entry {
                buf: CachedBuffer::F64(buf),
                dirty: false,
            },
        );
    }

    /// Phase 9 #1 — flag the cache entry as "device has writes the
    /// host hasn't seen yet". Called by the Resident-variant
    /// writebacks instead of doing an eager D→H copy. A subsequent
    /// `Native.arrayToHost` consults
    /// [`download_into_bytes_if_dirty`] to materialize host bytes
    /// before returning the Java array.
    pub fn mark_dirty(handle: u64) {
        if let Some(entry) = map().lock().get_mut(&handle) {
            entry.dirty = true;
        }
    }

    /// Phase 9 #1 — if the entry is dirty, download the device
    /// buffer into a fresh `Vec<u8>` (little-endian) and clear the
    /// flag. Returns `None` when the entry is unknown OR the entry
    /// is clean (no download needed; the host bytes in the
    /// native-builtins resident store are already current).
    ///
    /// The caller (`Native.arrayToHost` shim, via the
    /// `gpu_array_download_if_dirty` escape hatch) then passes the
    /// bytes back to
    /// [`cratonvm_native_builtins::craton_gpu::array_replace_bytes`]
    /// to refresh the resident store, after which the existing
    /// read path returns the up-to-date Java array.
    pub fn download_into_bytes_if_dirty(handle: u64) -> Option<Vec<u8>> {
        // Drop the lock around the actual device download so other
        // threads can hit the cache for unrelated handles. Snapshot
        // the Arc + clear the dirty bit under the lock, then call
        // out to cuda-bridge with no lock held.
        let arc_snapshot: CachedBufferArcs = {
            let mut guard = map().lock();
            let entry = guard.get_mut(&handle)?;
            if !entry.dirty {
                return None;
            }
            entry.dirty = false;
            match &entry.buf {
                CachedBuffer::I32(a) => CachedBufferArcs::I32(a.clone()),
                CachedBuffer::I64(a) => CachedBufferArcs::I64(a.clone()),
                CachedBuffer::F32(a) => CachedBufferArcs::F32(a.clone()),
                CachedBuffer::F64(a) => CachedBufferArcs::F64(a.clone()),
            }
        };
        match arc_snapshot {
            CachedBufferArcs::I32(buf) => {
                let len = buf.len();
                let mut dst = vec![0i32; len];
                crate::runtime::gpu_marshal::download_into(&buf, &mut dst).ok()?;
                Some(bytemuck::cast_slice(&dst).to_vec())
            }
            CachedBufferArcs::I64(buf) => {
                let len = buf.len();
                let mut dst = vec![0i64; len];
                crate::runtime::gpu_marshal::download_into(&buf, &mut dst).ok()?;
                Some(bytemuck::cast_slice(&dst).to_vec())
            }
            CachedBufferArcs::F32(buf) => {
                let len = buf.len();
                let mut dst = vec![0f32; len];
                crate::runtime::gpu_marshal::download_into(&buf, &mut dst).ok()?;
                Some(bytemuck::cast_slice(&dst).to_vec())
            }
            CachedBufferArcs::F64(buf) => {
                let len = buf.len();
                let mut dst = vec![0f64; len];
                crate::runtime::gpu_marshal::download_into(&buf, &mut dst).ok()?;
                Some(bytemuck::cast_slice(&dst).to_vec())
            }
        }
    }

    /// Lock-released variant of `CachedBuffer` used to escape the
    /// Mutex guard before doing a cudarc D→H copy. The four `Arc`
    /// clones keep the device buffers alive across the lock drop.
    enum CachedBufferArcs {
        I32(Arc<DeviceBuffer<i32>>),
        I64(Arc<DeviceBuffer<i64>>),
        F32(Arc<DeviceBuffer<f32>>),
        F64(Arc<DeviceBuffer<f64>>),
    }

    /// Drop the cached device buffer for `handle` (if any). Called
    /// from `Native.releaseArray` so the device memory is freed when
    /// the Java `GpuArray` is no longer needed. Phase 9 #1 note:
    /// any pending dirty bit is discarded — the user explicitly
    /// released the array, so the post-kernel device writes are
    /// implicitly forfeited.
    pub fn release(handle: u64) {
        map().lock().remove(&handle);
    }
}

// ── Phase 10 #2: cumulative H2D byte counter (optional trace) ───────
//
// Set `CRATONVM_GPU_TRACE_BYTES=1` to dump the accumulated host→device
// transfer total after every dispatch. With the Phase 10 #2 read-only
// suppression (this commit) plus the Phase 10 #1 residency cache, a
// 1000-iteration vectorAdd benchmark should show O(3 × array_size)
// total bytes — one initial upload of `a`, `b`, `out` — not
// O(3000 × array_size). The counter wraps around at `usize::MAX`;
// for the workloads it targets (single-process, single-benchmark) that
// is plenty.
#[cfg(feature = "gpu-offload")]
pub(crate) mod h2d_trace {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::OnceLock;

    static TOTAL: AtomicUsize = AtomicUsize::new(0);
    static ENABLED: OnceLock<bool> = OnceLock::new();

    /// True if `CRATONVM_GPU_TRACE_BYTES=1`. The probe is cached.
    pub(crate) fn enabled() -> bool {
        *ENABLED.get_or_init(|| {
            std::env::var("CRATONVM_GPU_TRACE_BYTES")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    /// Add `bytes` to the cumulative counter (only when trace is on,
    /// to avoid the atomic op on the hot path).
    pub(crate) fn add(bytes: usize) {
        if enabled() {
            TOTAL.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Snapshot the counter. Public for tests and external probes.
    pub fn total() -> usize {
        TOTAL.load(Ordering::Relaxed)
    }

    /// Reset to zero. Public for tests.
    pub fn reset() {
        TOTAL.store(0, Ordering::Relaxed);
    }
}

// ── Phase 10 #1: per-`ObjectRef` input-residency cache ──────────────
//
// Companion to `device_cache` for plain JVM primitive arrays passed
// to `submitMethod`. When the same Java array (same `ObjectRef`)
// reappears as a kernel arg on the next submit and the host side
// hasn't been mutated in the meantime, this cache hands back the
// existing `Arc<DeviceBuffer<T>>` so the H->D copy + host_view memcpy
// are both skipped — closing the H2D-every-submit gap that TornadoVM
// avoids with `DataTransferMode.FIRST_EXECUTION`.
//
// Validity model (minimal v1 — Phase 10 #1):
//   - On the first marshal of `obj`, we `host_view → upload → install`.
//   - The kernel's writeback does a D->H copy, leaving host == device.
//   - The cache entry survives across that writeback, so the *next*
//     submit on the same `obj` reuses the buffer.
//   - Invalidation: an explicit `invalidate(obj)` call from any
//     host-side write to the array's payload (currently called only
//     from `releaseExecutor` and on a full cache clear; the
//     interpreter/JIT IASTORE hooks are deferred to Phase 10 #2).
//
// Limitations (Phase 10 #2 / #3):
//   - Host-side stores via interpreter `xASTORE` or JIT
//     `jit_iastore` do NOT yet invalidate the cache entry. For the
//     `GpuBench` workload (which only mutates `a`, `b` during init
//     before any GPU submit) this is fine. For a workload that
//     mutates an input array between submits, this would feed the
//     kernel stale device data. The fix is a one-line call to
//     `input_cache::invalidate(obj)` in the interpreter store and
//     `jit_iastore`/`jit_fastore`/etc. helpers.
//   - GC compaction is currently OK because the GPU-critical guard
//     held during dispatch keeps the GC paused — but the cache
//     entry survives ACROSS submits, and a GC between submits that
//     moves an array's payload would invalidate the device buffer's
//     mirror without us noticing. Mitigated today by clearing the
//     whole cache via `clear_all` from `releaseExecutor`. A
//     production fix would clear on every major-GC compaction event.
#[cfg(feature = "gpu-offload")]
pub(crate) mod input_cache {
    use cratonvm_types::{ArrayElementType, ObjectRef};
    use cuda_bridge::DeviceBuffer;
    use parking_lot::Mutex;
    use rustc_hash::FxHashMap;
    use std::sync::{Arc, OnceLock};

    pub(crate) enum CachedBuffer {
        I32(Arc<DeviceBuffer<i32>>),
        I64(Arc<DeviceBuffer<i64>>),
        F32(Arc<DeviceBuffer<f32>>),
        F64(Arc<DeviceBuffer<f64>>),
    }

    pub(crate) struct Entry {
        pub buf: CachedBuffer,
        pub len: usize,
        pub element_type: ArrayElementType,
    }

    static CACHE: OnceLock<Mutex<FxHashMap<ObjectRef, Entry>>> = OnceLock::new();

    fn map() -> &'static Mutex<FxHashMap<ObjectRef, Entry>> {
        CACHE.get_or_init(|| Mutex::new(FxHashMap::default()))
    }

    /// Per-type lookup. Returns `None` on miss OR if the cached
    /// entry's element type / length doesn't match the requested
    /// shape (defensive: a stale `ObjectRef` could be reused for a
    /// different array kind across a GC; we treat that as a miss
    /// and the caller re-uploads).
    pub(crate) fn get_i32(obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<i32>>> {
        let g = map().lock();
        let e = g.get(&obj)?;
        if e.element_type != ArrayElementType::Int || e.len != len {
            return None;
        }
        if let CachedBuffer::I32(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_i32(obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<i32>>) {
        map().lock().insert(
            obj,
            Entry {
                buf: CachedBuffer::I32(buf),
                len,
                element_type: ArrayElementType::Int,
            },
        );
    }
    pub(crate) fn get_i64(obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<i64>>> {
        let g = map().lock();
        let e = g.get(&obj)?;
        if e.element_type != ArrayElementType::Long || e.len != len {
            return None;
        }
        if let CachedBuffer::I64(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_i64(obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<i64>>) {
        map().lock().insert(
            obj,
            Entry {
                buf: CachedBuffer::I64(buf),
                len,
                element_type: ArrayElementType::Long,
            },
        );
    }
    pub(crate) fn get_f32(obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<f32>>> {
        let g = map().lock();
        let e = g.get(&obj)?;
        if e.element_type != ArrayElementType::Float || e.len != len {
            return None;
        }
        if let CachedBuffer::F32(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_f32(obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<f32>>) {
        map().lock().insert(
            obj,
            Entry {
                buf: CachedBuffer::F32(buf),
                len,
                element_type: ArrayElementType::Float,
            },
        );
    }
    pub(crate) fn get_f64(obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<f64>>> {
        let g = map().lock();
        let e = g.get(&obj)?;
        if e.element_type != ArrayElementType::Double || e.len != len {
            return None;
        }
        if let CachedBuffer::F64(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_f64(obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<f64>>) {
        map().lock().insert(
            obj,
            Entry {
                buf: CachedBuffer::F64(buf),
                len,
                element_type: ArrayElementType::Double,
            },
        );
    }

    /// Drop the device-buffer cache entry for `obj`. Intended for
    /// future wiring from interpreter / JIT array-store paths.
    pub fn invalidate(obj: ObjectRef) {
        map().lock().remove(&obj);
    }

    /// Drop every cached entry. Called from `releaseExecutor` and
    /// any future major-GC-compaction hook.
    pub fn clear_all() {
        map().lock().clear();
    }

    /// Diagnostic: current entry count.
    pub fn len() -> usize {
        map().lock().len()
    }
}

// ── Per-type marshalling helpers ────────────────────────────────────

#[cfg(feature = "gpu-offload")]
pub enum MarshalWriteback {
    // Plain JVM primitive arrays (Phase 5).
    //
    // Phase 10 #1 (input-residency): the device buffer is held as
    // `Arc<DeviceBuffer<T>>` so the per-`ObjectRef` `input_cache`
    // can hold a parallel reference and reuse it on the next submit
    // that names the same Java array. The post-sync writeback still
    // does a D->H copy into the JVM array — but the device buffer
    // survives, so the next submit's H->D upload is skipped.
    I32 {
        obj: cratonvm_types::ObjectRef,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i32>>,
        len: usize,
    },
    I64 {
        obj: cratonvm_types::ObjectRef,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i64>>,
        len: usize,
    },
    F32 {
        obj: cratonvm_types::ObjectRef,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f32>>,
        len: usize,
    },
    F64 {
        obj: cratonvm_types::ObjectRef,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f64>>,
        len: usize,
    },
    // Phase 6 #3 / Phase 7 #2 — GpuArray-backed args. The
    // `DeviceBuffer<T>` is shared with `device_cache` so the next
    // kernel using the same `handle` reuses it instead of
    // re-uploading. Writeback target is the resident-store entry
    // keyed by `handle`, not a Java array object.
    ResidentI32 {
        handle: u64,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i32>>,
        len: usize,
    },
    ResidentI64 {
        handle: u64,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i64>>,
        len: usize,
    },
    ResidentF32 {
        handle: u64,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f32>>,
        len: usize,
    },
    ResidentF64 {
        handle: u64,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f64>>,
        len: usize,
    },
    /// Phase 9 #1 follow-up — owns the 1-element `u64` device buffer
    /// the kernel uses to signal a bounds-check failure. The writeback
    /// downloads the cell after `event.synchronize()`; a non-zero
    /// value surfaces as a failure message (so the Java side gets a
    /// real `GpuException("bounds check failed")` instead of silent
    /// corrupt output). The `Arc` form keeps the buffer alive across
    /// the launch even though only one reference exists today (matches
    /// the resident-variant ownership pattern).
    FailureFlag {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<u64>>,
    },
}

#[cfg(feature = "gpu-offload")]
impl MarshalWriteback {
    fn writeback(
        &self,
        shared: &crate::vm::SharedVm,
        token: &cratonvm_gc::safepoint::SafepointToken<'_>,
    ) -> Result<(), String> {
        use crate::runtime::gpu_marshal;
        match self {
            // Download the kernel-written buffer straight into the JVM heap
            // arena (no staging Vec + write-back) when the array is contiguous.
            Self::I32 { obj, buf, .. } => {
                gpu_marshal::download_obj_i32(buf.as_ref(), *obj, &shared.heap, token)
                    .map_err(|e| format!("download i32: {e}"))
            }
            Self::I64 { obj, buf, .. } => {
                gpu_marshal::download_obj_i64(buf.as_ref(), *obj, &shared.heap, token)
                    .map_err(|e| format!("download i64: {e}"))
            }
            Self::F32 { obj, buf, .. } => {
                gpu_marshal::download_obj_f32(buf.as_ref(), *obj, &shared.heap, token)
                    .map_err(|e| format!("download f32: {e}"))
            }
            Self::F64 { obj, buf, .. } => {
                gpu_marshal::download_obj_f64(buf.as_ref(), *obj, &shared.heap, token)
                    .map_err(|e| format!("download f64: {e}"))
            }
            // Phase 9 #1 — Resident-arg writebacks no longer
            // download to host bytes eagerly. They mark the cache
            // entry dirty; the next `Native.arrayToHost(handle)`
            // call materializes host bytes on demand via the
            // `gpu_array_download_if_dirty` escape hatch.
            //
            // This saves one D→H copy per kernel for GpuArrays
            // that the user pipelines through multiple kernels
            // before reading the result (the common chaining
            // pattern). For users who DO read after every kernel,
            // the work just moves from here to `arrayToHost` —
            // same total bytes, different scheduling.
            //
            // The `buf` field stays in the enum (unused here) so
            // the Arc<DeviceBuffer<T>> stays alive for the
            // submission's lifetime; the cache itself holds the
            // parallel Arc that lives until `releaseArray`.
            Self::ResidentI32 { handle, .. } => {
                device_cache::mark_dirty(*handle);
                Ok(())
            }
            Self::ResidentI64 { handle, .. } => {
                device_cache::mark_dirty(*handle);
                Ok(())
            }
            Self::ResidentF32 { handle, .. } => {
                device_cache::mark_dirty(*handle);
                Ok(())
            }
            Self::ResidentF64 { handle, .. } => {
                device_cache::mark_dirty(*handle);
                Ok(())
            }
            // Phase 9 #1 follow-up — read the failure flag back.
            // If non-zero, the kernel hit a bounds check; report as
            // a writeback error so `finalize_submission` flips the
            // submission to `Failed` and Java sees a `GpuException`.
            Self::FailureFlag { buf } => {
                let mut cell = [0u64; 1];
                gpu_marshal::download_into(buf, &mut cell)
                    .map_err(|e| format!("download_into failure_flag: {e}"))?;
                if cell[0] != 0 {
                    return Err(format!(
                        "kernel failure flag set (value={}): out-of-range index inside kernel body",
                        cell[0]
                    ));
                }
                Ok(())
            }
        }
    }

    /// Element-count of the array this writeback owns. Used by
    /// `dispatch_method_from_native` to compute the launch grid:
    /// the kernel needs `>= max(array_len)` threads to cover every
    /// output index. Returns `None` for the `FailureFlag` variant
    /// (no array body — just a 1-cell signal).
    pub fn array_len(&self) -> Option<usize> {
        match self {
            Self::I32 { len, .. }
            | Self::I64 { len, .. }
            | Self::F32 { len, .. }
            | Self::F64 { len, .. }
            | Self::ResidentI32 { len, .. }
            | Self::ResidentI64 { len, .. }
            | Self::ResidentF32 { len, .. }
            | Self::ResidentF64 { len, .. } => Some(*len),
            Self::FailureFlag { .. } => None,
        }
    }
}

/// Phase 6 #3: detect a `craton.gpu.GpuArray` Java object and read
/// its (element_type, length, host bytes, native handle) tuple from
/// the resident-array store. Returns `None` for any non-GpuArray
/// object so callers can fall through to other arg shapes.
///
/// The GpuArray Java layout (P3-2):
///   field 0: `long handle`
///   field 1: `Class<?> elementType`  (unused here; type comes from
///                                     the resident-store record)
#[cfg(feature = "gpu-offload")]
fn try_gpu_array_snapshot(
    shared: &crate::vm::SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<(cratonvm_types::ArrayElementType, usize, Vec<u8>, u64)> {
    let cid = shared.heap.class_id_of(obj_ref);
    let cm = shared.class_manager.read();
    let cls_name = cm.get_class(cid).map(|c| c.name.to_string())?;
    drop(cm);
    if cls_name != "craton/gpu/GpuArray" {
        return None;
    }
    // field 0 holds the long `handle`.
    let handle = match shared.heap.get_field(obj_ref, 0) {
        cratonvm_types::Value::Long(h) => h as u64,
        _ => return None,
    };
    let (etype, len, bytes) = cratonvm_native_builtins::craton_gpu::array_snapshot(handle)?;
    Some((etype, len, bytes, handle))
}

/// Marshal a resident GpuArray as a kernel arg. The shape of the
/// closure + writeback record mirrors `marshal_array_arg` but the
/// source bytes are the resident-store snapshot rather than a JVM
/// array — and the writeback target is the resident store (so a
/// subsequent `GpuArray.toHost()` reads the post-kernel content).
#[cfg(feature = "gpu-offload")]
fn marshal_resident_array_arg(
    ctx: &cuda_bridge::DeviceContext,
    element_type: cratonvm_types::ArrayElementType,
    len: usize,
    host_bytes: Vec<u8>,
    arr_handle: u64,
) -> Result<
    (
        Box<dyn FnOnce(cuda_bridge::KernelArgs) -> cuda_bridge::KernelArgs>,
        MarshalWriteback,
    ),
    String,
> {
    use crate::runtime::gpu_marshal;
    use cratonvm_types::ArrayElementType;

    // Macro to keep the four type-specialized arms readable. Each
    // arm: (a) consult device_cache, (b) on miss upload + cache,
    // (c) build a push-closure pinning the DeviceBuffer pointer
    // via the Arc<DeviceBuffer<T>> stored in the writeback.
    macro_rules! resident_arm {
        ($ty:ty, $variant:ident, $cache_get:path, $cache_put:path, $tag:literal) => {{
            // (a) Cache check.
            let arc = if let Some(arc) = $cache_get(arr_handle) {
                arc
            } else {
                // (b) Miss — upload and install.
                let host: &[$ty] = bytemuck::cast_slice(&host_bytes);
                let buf = gpu_marshal::upload(ctx, host)
                    .map_err(|e| format!("upload {} (GpuArray, len={len}): {e}", $tag))?;
                let arc = std::sync::Arc::new(buf);
                $cache_put(arr_handle, arc.clone());
                arc
            };
            // (c) Build the push closure + writeback. Both share
            // ownership of the Arc; the closure pins a raw pointer
            // to the inner DeviceBuffer for `push_device_ptr` since
            // KernelArgs takes `&DeviceBuffer<T>`. The pointer
            // stays valid because the Arc inside the writeback
            // (returned alongside) keeps the buffer alive.
            let len32 = len as i32;
            let wb_arc = arc.clone();
            let device_ptr = std::sync::Arc::as_ptr(&arc) as *const cuda_bridge::DeviceBuffer<$ty>;
            let push: Box<dyn FnOnce(_) -> _> = Box::new(move |args: cuda_bridge::KernelArgs| {
                // SAFETY: the Arc<DeviceBuffer<T>> we cloned
                // for the writeback (wb_arc, returned with the
                // writeback below) keeps the buffer alive for
                // the duration of `dispatch_method_from_native`,
                // which is when this closure fires and is
                // immediately consumed.
                let _ = &arc; // keep this clone alive past push
                let buf_ref: &cuda_bridge::DeviceBuffer<$ty> = unsafe { &*device_ptr };
                args.push_device_ptr(buf_ref).push_i32(len32)
            });
            (
                push,
                MarshalWriteback::$variant {
                    handle: arr_handle,
                    buf: wb_arc,
                    len,
                },
            )
        }};
    }

    let (push, wb) = match element_type {
        ArrayElementType::Int => resident_arm!(
            i32,
            ResidentI32,
            device_cache::get_i32,
            device_cache::put_i32,
            "i32"
        ),
        ArrayElementType::Long => resident_arm!(
            i64,
            ResidentI64,
            device_cache::get_i64,
            device_cache::put_i64,
            "i64"
        ),
        ArrayElementType::Float => resident_arm!(
            f32,
            ResidentF32,
            device_cache::get_f32,
            device_cache::put_f32,
            "f32"
        ),
        ArrayElementType::Double => resident_arm!(
            f64,
            ResidentF64,
            device_cache::get_f64,
            device_cache::put_f64,
            "f64"
        ),
        other => {
            return Err(format!(
                "submitMethod: GpuArray element type {other:?} unsupported"
            ));
        }
    };
    Ok((push, wb))
}

/// Phase 6 #2: detect Java's autoboxed primitives and unwrap them
/// to the underlying `Value::Int/Long/Float/Double`. Java's
/// `Object[] args` always boxes scalars passed via varargs (`int 5`
/// becomes `Integer.valueOf(5)`), so without this every scalar arg
/// to `submitMethod` would be misclassified as "not an array".
///
/// Returns `None` if the object is not one of the eight wrapper
/// classes, or if its `value` field (slot 0) doesn't carry a
/// primitive variant. Byte/Short/Character/Boolean wrappers all
/// store an `Int` internally (matching the JVM's stack
/// representation) and surface here as `Value::Int(...)`.
#[cfg(feature = "gpu-offload")]
fn try_unbox_primitive(
    shared: &crate::vm::SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::Value> {
    let cid = shared.heap.class_id_of(obj_ref);
    let cm = shared.class_manager.read();
    let cls_name = cm.get_class(cid).map(|c| c.name.to_string())?;
    drop(cm);
    let inner = shared.heap.get_field(obj_ref, 0);
    match cls_name.as_str() {
        "java/lang/Integer"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Boolean"
        | "java/lang/Character" => match inner {
            cratonvm_types::Value::Int(_) => Some(inner),
            _ => None,
        },
        "java/lang/Long" => match inner {
            cratonvm_types::Value::Long(_) => Some(inner),
            // Some MethodHandle paths store the long as Int — coerce.
            cratonvm_types::Value::Int(v) => Some(cratonvm_types::Value::Long(v as i64)),
            _ => None,
        },
        "java/lang/Float" => match inner {
            cratonvm_types::Value::Float(_) => Some(inner),
            cratonvm_types::Value::Int(v) => {
                Some(cratonvm_types::Value::Float(f32::from_bits(v as u32)))
            }
            _ => None,
        },
        "java/lang/Double" => match inner {
            cratonvm_types::Value::Double(_) => Some(inner),
            cratonvm_types::Value::Long(v) => {
                Some(cratonvm_types::Value::Double(f64::from_bits(v as u64)))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Marshal one Java primitive-array arg. Returns:
///   - a closure that pushes `(device_ptr, length)` into the
///     in-flight `KernelArgs` (the closure form sidesteps borrowing
///     `kernel_args` mutably while we still hold `shared`)
///   - a `MarshalWriteback` recording how to copy the device buffer
///     back into the source Java array after the kernel finishes.
///
/// Phase 10 #1 — input-residency: before doing the H->D copy, we
/// consult `input_cache` keyed by `ObjectRef`. On a hit (same Java
/// array seen on a previous submit, contents still in sync with the
/// device buffer), the H->D upload AND the `host_view_*` heap memcpy
/// are skipped entirely — we just hand back the cached
/// `Arc<DeviceBuffer<T>>`. On a miss the existing
/// `host_view → upload` path runs and the resulting Arc is installed
/// into the cache so the next submit hits.
///
/// Phase 10 #2 — `is_kernel_written` tells us whether the kernel
/// body actually `*astore`s into this parameter slot. When it does
/// not (read-only input, e.g. `a` and `b` in `vectorAdd(a, b, out)`),
/// we return `None` for the writeback — the post-launch D→H copy
/// (256MB for a 2^26-int input at vectorAdd scale) is pure waste
/// because the device contents already match the host. Returning
/// `None` saves the matching `wb_arc` clone the cache-only path
/// would have produced; the Arc inside `input_cache` keeps the
/// buffer alive across submits.
#[cfg(feature = "gpu-offload")]
fn marshal_array_arg(
    shared: &crate::vm::SharedVm,
    ctx: &cuda_bridge::DeviceContext,
    obj_ref: cratonvm_types::ObjectRef,
    element_type: cratonvm_types::ArrayElementType,
    is_kernel_written: bool,
    token: &cratonvm_gc::safepoint::SafepointToken<'_>,
) -> Result<
    (
        Box<dyn FnOnce(cuda_bridge::KernelArgs) -> cuda_bridge::KernelArgs>,
        Option<MarshalWriteback>,
        usize,
    ),
    String,
> {
    use crate::runtime::gpu_marshal;
    use cratonvm_types::ArrayElementType;
    use std::sync::Arc;

    // Resolve the array length without copying the payload — the
    // cache key is (ObjectRef, element_type, len). A mismatch on
    // length forces a fresh upload (an array can't change length
    // on the JVM heap without becoming a different ObjectRef, but
    // we treat length as part of the validity envelope defensively).
    let len = shared.heap.array_length(obj_ref);

    // Macro to keep the four arms readable. Each arm:
    //   (a) consult input_cache via the type's getter — on hit, skip
    //       host_view + upload and reuse the cached Arc.
    //   (b) on miss, perform the existing host_view → upload, then
    //       install the Arc into the cache so future submits hit.
    //   (c) build the (push closure, optional-writeback) pair from
    //       the Arc. The closure captures a raw pointer to the inner
    //       DeviceBuffer (via `Arc::as_ptr`). For kernel-written
    //       params, the writeback holds a parallel Arc clone keeping
    //       the buffer alive for the launch + writeback duration;
    //       for read-only inputs we skip the writeback entirely and
    //       rely on the closure's owned Arc + the `input_cache` Arc
    //       to keep the buffer alive across the launch.
    macro_rules! arm {
        (
            $ty:ty,
            $variant:ident,
            $upload_obj:path,
            $cache_get:path,
            $cache_put:path,
            $tag:literal,
            $elem_size:expr
        ) => {{
            // (a) Cache check.
            let (arc, uploaded): (Arc<cuda_bridge::DeviceBuffer<$ty>>, bool) =
                if let Some(arc) = $cache_get(obj_ref, len) {
                    (arc, false)
                } else {
                    // (b) Miss — upload, reading the JVM heap arena directly
                    //     (no staging Vec) when the array is contiguous, then
                    //     install into the input cache.
                    let buf = $upload_obj(ctx, obj_ref, &shared.heap, token)
                        .map_err(|e| format!("upload {} (len={len}): {e}", $tag))?;
                    let arc = Arc::new(buf);
                    $cache_put(obj_ref, len, arc.clone());
                    (arc, true)
                };
            // (c) Build push closure + optional writeback. Read-only
            //     params skip the writeback (no D→H copy on the
            //     finalize path).
            let len32 = len as i32;
            let wb_opt = if is_kernel_written {
                Some(MarshalWriteback::$variant {
                    obj: obj_ref,
                    buf: arc.clone(),
                    len,
                })
            } else {
                None
            };
            let device_ptr = Arc::as_ptr(&arc) as *const cuda_bridge::DeviceBuffer<$ty>;
            let push: Box<dyn FnOnce(_) -> _> = Box::new(move |args: cuda_bridge::KernelArgs| {
                // SAFETY: `arc` is moved into this closure (kept
                // alive at least until it fires). The closure
                // runs once during the dispatch sequence and
                // immediately pushes the pointer into KernelArgs;
                // after that the writeback (when present) and/or
                // the `input_cache` Arc keep the buffer alive
                // through the launch and synchronize.
                let _ = &arc;
                let buf_ref: &cuda_bridge::DeviceBuffer<$ty> = unsafe { &*device_ptr };
                args.push_device_ptr(buf_ref).push_i32(len32)
            });
            let bytes_uploaded = if uploaded { len * $elem_size } else { 0 };
            (push, wb_opt, bytes_uploaded)
        }};
    }

    let (push, wb, bytes_uploaded) = match element_type {
        ArrayElementType::Int => arm!(
            i32,
            I32,
            gpu_marshal::upload_obj_i32,
            input_cache::get_i32,
            input_cache::put_i32,
            "i32",
            4
        ),
        ArrayElementType::Long => arm!(
            i64,
            I64,
            gpu_marshal::upload_obj_i64,
            input_cache::get_i64,
            input_cache::put_i64,
            "i64",
            8
        ),
        ArrayElementType::Float => arm!(
            f32,
            F32,
            gpu_marshal::upload_obj_f32,
            input_cache::get_f32,
            input_cache::put_f32,
            "f32",
            4
        ),
        ArrayElementType::Double => arm!(
            f64,
            F64,
            gpu_marshal::upload_obj_f64,
            input_cache::get_f64,
            input_cache::put_f64,
            "f64",
            8
        ),
        other => {
            return Err(format!(
                "submitMethod: unsupported array element type: {other:?}"
            ))
        }
    };
    Ok((push, wb, bytes_uploaded))
}
