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
//!
//! # Reduction (scalar-return) kernels
//!
//! [`try_dispatch`] transparently offloads a proven reduction
//! (`KernelSignature::is_reduction` — a counted loop with a
//! loop-carried accumulator and a scalar return) in addition to void
//! kernels, but ONLY for `)I`/`)J` descriptors: the compiled PTX
//! accumulates via `atom.global.add`, which is bit-exact for int/long
//! (two's-complement wraparound doesn't depend on summation order) but
//! NOT for `)F`/`)D` (GPU float atomic-add reorders the per-thread
//! sum, unlike Java's sequential left-to-right fp accumulation) —
//! those keep falling through to the CPU. The accumulator buffer
//! (`ret_ptr`, the kernel param `build_param_list` appends after the
//! declared params and before `failure_flag`) is allocated
//! zero-initialized by [`dispatch_method_from_native`]; the value is
//! downloaded by [`finalize_submission`] into
//! [`SerializedResult::ScalarI32`]/[`SerializedResult::ScalarI64`] and
//! surfaced to the interpreter as
//! [`DispatchOutcome::HandledWithValue`].

use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use crate::classloading::ClassId;
use crate::config::VmConfig;
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::method::ClassFileMethod;

use cratonvm_native_api::registry::GpuErrorKind;
use cuda_bridge::{DeviceContext, DeviceModule};
use jit_cuda::annotations::read_method_annotations;
use jit_cuda::emitter::PtxModule;
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
    /// Occupancy-selected block size, memoised after the first launch.
    /// `0` means "not yet queried".
    ///
    /// `cuOccupancyMaxPotentialBlockSize` is a driver round trip and it
    /// was being paid on EVERY dispatch, for an answer that depends
    /// only on the kernel — not on how many elements this particular
    /// launch covers. An inference step is hundreds of small kernels,
    /// so a per-dispatch driver call is multiplied by hundreds before
    /// anything else is measured.
    pub block_size: std::sync::atomic::AtomicU32,
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
    /// GpuStream affinity registry — Java-visible stream handle
    /// (minted by [`stream_create`](OffloadCache::stream_create),
    /// wrapped by `Native.newStream` or an executor's lazily-created
    /// default stream — see
    /// `native-builtins/src/craton_gpu.rs::resolve_or_create_default_stream`)
    /// -> the real `cuda_bridge::Stream` it names.
    /// [`dispatch_method_from_native_on_stream`] resolves a
    /// caller-supplied handle here instead of always minting a fresh
    /// private stream — see that function's stream-resolution step
    /// for the ordering contract this buys.
    streams: RwLock<FxHashMap<u64, Arc<Stream>>>,
    /// Monotonic counter for `streams`' keys. Independent per
    /// `OffloadCache` (i.e. per device ordinal) and from
    /// `NEXT_SUBMISSION_HANDLE` (submissions and streams are
    /// different Java-visible handle spaces reached through different
    /// `Native.*` entry points, so nothing ever confuses the two).
    next_stream_handle: std::sync::atomic::AtomicU64,
    /// Internal streams the chunked writeback rotates its per-chunk
    /// launches over. Created once on first use and reused: a chunked
    /// dispatch needs several streams so consecutive chunks can run
    /// concurrently, and these are private to the offload path (the
    /// `streams` map above holds Java-visible `GpuStream` handles).
    chunk_streams: RwLock<Vec<std::sync::Arc<Stream>>>,
    /// Reused page-locked staging slabs for the chunked writeback, one
    /// per element type. See `staging_slot!` for why they are cached.
    /// Reused per-chunk completion events. `cuEventCreate` is not free
    /// and a chunked dispatch needs one per chunk; re-recording a pooled
    /// event costs nothing. Handed out only when the pool holds the sole
    /// reference to every event, so a submission still waiting on one
    /// never has it re-recorded under it.
    chunk_events: RwLock<Vec<std::sync::Arc<cuda_bridge::Event>>>,
    /// The built-in kernel module (GEMM and the fp16 converters), loaded
    /// once per device on first use.
    ///
    /// Lazily rather than at construction: most programs never call a
    /// built-in, and loading PTX costs a driver JIT of every kernel in
    /// the module. `None` means "not yet attempted"; a load failure is
    /// reported to the caller and retried next time, since the usual
    /// cause is a transient out-of-memory rather than bad PTX.
    builtin_module: RwLock<Option<Arc<cuda_bridge::DeviceModule>>>,
    chunk_stage_i32: RwLock<Option<std::sync::Arc<cuda_bridge::PinnedHostBuffer<i32>>>>,
    chunk_stage_i64: RwLock<Option<std::sync::Arc<cuda_bridge::PinnedHostBuffer<i64>>>>,
    chunk_stage_f32: RwLock<Option<std::sync::Arc<cuda_bridge::PinnedHostBuffer<f32>>>>,
    chunk_stage_f64: RwLock<Option<std::sync::Arc<cuda_bridge::PinnedHostBuffer<f64>>>>,
    /// Compute capability of `ctx`'s device, as `(major, minor)`.
    ///
    /// This is the `sm_XX` every kernel on this cache is lowered for.
    /// It used to be hardcoded to `(7, 0)`, so an sm_75 RTX 2060 (or
    /// anything newer) was handed PTX that declared `.target sm_70` and
    /// the driver JIT could not use any instruction introduced after
    /// Volta. Probed once at construction; `(7, 0)` remains the floor
    /// when the probe fails or reports something older, because the
    /// lowering emits Volta-era PTX unconditionally.
    sm: (u32, u32),
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
        // Ask the device it will actually launch on for its compute
        // capability, so kernels are lowered for the real `sm_XX`
        // instead of the Volta floor. Clamped up only: the lowering
        // emits sm_70-era PTX, so a device older than that (or a
        // failed probe) keeps the floor and the driver rejects the
        // module later if it truly cannot run it.
        let sm = if ctx.is_some() {
            match cuda_bridge::probe_device(config.gpu_device_ordinal) {
                Ok(caps) if (caps.compute_major, caps.compute_minor) >= (7, 0) => {
                    tracing::info!(
                        "gpu offload: device {} is {} (sm_{}{}), lowering for it",
                        caps.ordinal,
                        caps.name,
                        caps.compute_major,
                        caps.compute_minor
                    );
                    (caps.compute_major, caps.compute_minor)
                }
                _ => (7, 0),
            }
        } else {
            (7, 0)
        };
        Self {
            ctx,
            kernels: RwLock::new(FxHashMap::default()),
            blacklist: RwLock::new(FxHashSet::default()),
            print_decisions: config.print_gpu_decisions,
            streams: RwLock::new(FxHashMap::default()),
            next_stream_handle: std::sync::atomic::AtomicU64::new(1),
            chunk_streams: RwLock::new(Vec::new()),
            chunk_events: RwLock::new(Vec::new()),
            builtin_module: RwLock::new(None),
            chunk_stage_i32: RwLock::new(None),
            chunk_stage_i64: RwLock::new(None),
            chunk_stage_f32: RwLock::new(None),
            chunk_stage_f64: RwLock::new(None),
            sm,
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

    /// The built-in kernel module for this device, loading it on first
    /// call.
    ///
    /// See [`crate::runtime::kernels`] for what is in it and why those
    /// kernels are not lowered from bytecode.
    pub fn builtin_module(&self) -> Result<Arc<cuda_bridge::DeviceModule>, String> {
        if let Some(m) = self.builtin_module.read().as_ref() {
            return Ok(Arc::clone(m));
        }
        let ctx = self
            .device()
            .ok_or_else(|| "no CUDA device available for the built-in kernels".to_string())?;

        let mut slot = self.builtin_module.write();
        // Another thread may have loaded it while we waited for the write
        // lock; loading twice would be correct but would JIT the module a
        // second time for nothing.
        if let Some(m) = slot.as_ref() {
            return Ok(Arc::clone(m));
        }
        let module = crate::runtime::kernels::load(ctx)
            .map_err(|e| format!("loading the built-in kernel module: {e}"))?;
        let module = Arc::new(module);
        *slot = Some(Arc::clone(&module));
        Ok(module)
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
        // Pool-aware analysis so `ldc`/`ldc_w`/`ldc2_w` of primitive
        // constants (Integer/Float/Long/Double CP entries) are admitted;
        // without the pool the analyzer must reject every ldc.
        let verdict =
            analyzer::analyze_with_annotations_and_pool(method, &method_annotations, constant_pool);
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

        // Lower to PTX for the compute capability `self.sm` probed at
        // construction. This used to be a hardcoded `(7, 0)`, which
        // declared `.target sm_70` on every device and left the driver
        // JIT unable to use anything introduced after Volta.
        let ptx_module: PtxModule = match jit_cuda::lowering::lower_method_with_pool(
            class_name,
            method,
            constant_pool,
            &sig,
            self.sm.0,
            self.sm.1,
        ) {
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
        // The precise read set, replacing the analyzer's conservative
        // u64::MAX. `writes & !reads` is what the chunked writeback may
        // stream out before the failure flag is known.
        sig.reads_param_mask = ptx_module.reads_param_mask;
        // Where the counted loop's trip count comes from, so the launch
        // grid is sized to the loop rather than to the largest array
        // argument. See `WorkBound`.
        sig.work_bound = ptx_module.work_bound;
        let ptx_text = ptx_module.render();
        dump_ptx_if_requested(class_name, &method.name, &ptx_text);
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

        // AUDIT 2026-08-28: honour `@GpuKernel(blockX = ...)`.
        //
        // `block_x` was parsed off the annotation and then read by
        // nothing, so a user who picked a block size got the
        // occupancy-tuned one instead and no indication that their
        // choice had been discarded. Seeding the memo with it is exactly
        // what the memo is for: a non-zero value means "already decided",
        // so the launch path skips the `cuOccupancyMaxPotentialBlockSize`
        // round trip and uses this. Zero keeps the autotuned default,
        // which is what `blockX`'s Java documentation already promises
        // that value means.
        //
        // `blockY` / `blockZ` are deliberately still not honoured: the
        // only launch shape the emitter produces is 1-D, so a Y or Z
        // extent has nothing to apply to. The analyzer rejects a kernel
        // that asks for one rather than ignoring it — same reasoning as
        // `UnsupportedGridShape`.
        let declared_block = method_annotations
            .gpu_kernel
            .as_ref()
            .map(|k| k.block_x)
            .unwrap_or(0);
        let kernel = Arc::new(CompiledKernel {
            module,
            signature: sig,
            kernel_name,
            block_size: std::sync::atomic::AtomicU32::new(declared_block),
        });
        self.kernels.write().insert(key, Arc::clone(&kernel));
        LookupOutcome::Hit(kernel)
    }
}

/// Write the PTX just lowered for `class.method` to
/// `$CRATONVM_GPU_DUMP_PTX/<class>.<method>.ptx`, when that variable
/// names a directory.
///
/// Diagnostic only, and off unless asked for. It exists because the
/// question "is this kernel bit-exact with the CPU?" is answered by
/// reading the emitted instructions' rounding modifiers, and there was
/// previously no way to see them short of rebuilding the VM with a
/// `println!`. The 2026-08-21 ray-tracer divergence — unrounded
/// `mul.f32`/`add.f32` being contracted into one `FFMA` by ptxas — was
/// exactly that kind of question.
///
/// A failure to write is reported once at `warn` and otherwise ignored:
/// a diagnostic knob must never take down a run.
fn dump_ptx_if_requested(class_name: &str, method_name: &str, ptx_text: &str) {
    // Through the config boundary, not `std::env` directly: this IS a declared
    // flag (`CRATONVM_DBG=gpu-dump-ptx`), so it must be served by the immutable
    // snapshot like every other one. A raw read also made the grouped spelling
    // silently do nothing here.
    let Ok(dir) = cratonvm_types::flags::runtime_var("CRATONVM_GPU_DUMP_PTX") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    // Method names carry JVM-legal characters that are not filename-legal
    // (`<init>`, `<clinit>`); map anything outside a conservative set.
    let safe: String = format!("{class_name}.{method_name}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = std::path::Path::new(&dir).join(format!("{safe}.ptx"));
    if let Err(e) = std::fs::write(&path, ptx_text) {
        tracing::warn!("gpu offload: CRATONVM_GPU_DUMP_PTX write to {path:?} failed: {e}");
    } else {
        tracing::info!("gpu offload: wrote PTX for {class_name}.{method_name} to {path:?}");
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

/// Outcome of `try_dispatch`. `Handled` / `HandledWithValue` mean the
/// GPU path completed the call; the caller must skip the CPU dispatch.
/// `FallThrough` means the GPU path declined — the operand stack and
/// locals are exactly as the hook received them and the CPU path must
/// run.
///
/// `HandledWithValue` (Part E) carries a kernel's scalar return value
/// (an integer reduction accumulator today — see the Hit arm below)
/// for the interpreter hook to push onto the caller's operand stack.
/// `try_dispatch` itself cannot do that push: `push_invoke_return_value`
/// / `coerce_value_for_return` live in `interpreter.rs`, on the far
/// side of this module's public boundary, and the exact push sequence
/// (tag-exact long handling, `native_return_pushed_to_stack` follow-up)
/// only needs to exist once — the fallback invokestatic path already
/// has it a few dozen lines below the hook, so `HandledWithValue`
/// mirrors that instead of duplicating it here.
///
/// `FallThroughKeepHooked` is `FallThrough` plus a contract with the
/// interpreter: the call site must NOT be promoted into the invoke
/// cache. A cached target dispatches straight to the CPU body and
/// never re-enters this hook, which would permanently end offload for
/// a site whose *current* arguments merely failed a per-call gate
/// (`--gpu-min-work`: the next call may pass a bigger array). The
/// same rule is why a `Handled`/`HandledWithValue` site is never
/// cached either — see the hook in `execute_invokestatic`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DispatchOutcome {
    Handled,
    HandledWithValue(cratonvm_types::Value),
    FallThrough,
    FallThroughKeepHooked,
}

/// Interpreter hook entry point for transparent GPU offload.
///
/// Looks the invokestatic target up in the `OffloadCache` (which
/// analyzes, lowers, and loads the PTX module on first reach). On a
/// cache `Hit` for an eligible kernel whose largest array argument
/// clears `--gpu-min-work`, it marshals the arguments to the device,
/// launches the kernel, and synchronizes:
///
/// - A **void** kernel writes the kernel-written arrays back into the
///   Java heap and returns `Handled` — the interpreter skips the CPU
///   body with the operand stack already in its post-call shape (args
///   popped, nothing pushed).
/// - An **integer reduction** kernel (`)I`/`)J` return,
///   `KernelSignature::is_reduction` proven by the analyzer) downloads
///   the atomically-accumulated scalar and returns `HandledWithValue`
///   — the interpreter hook pushes it as the call's return value.
///   Float/double reductions are NOT offered here: GPU float
///   atomic-add reorders the per-thread summation, which is not
///   bit-identical to Java's sequential left-to-right fp accumulation,
///   while int/long atomic add is exact (two's-complement wraparound
///   doesn't care about summation order) — those keep falling through
///   to the CPU.
///
/// Every other path (non-void non-reduction return, work below
/// threshold, marshal/launch failure, ineligible/blacklisted method)
/// returns `FallThrough`, leaving the operand stack and locals
/// untouched so the CPU body runs normally.
///
/// On a no-GPU machine `cache.has_device()` is false and the
/// early-return in `execute_invokestatic` short-circuits before this
/// function is reached.
pub fn try_dispatch(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::JvmThread,
    frame_idx: usize,
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
    let cm = shared.classes.class_manager.read();
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
        LookupOutcome::Hit(kernel) => {
            // Transparent synchronous offload. Two gates first:
            //
            // 1. VOID return, or a proven INTEGER reduction. A `Handled`
            //    outcome tells the interpreter the invokestatic is
            //    complete with the operand stack already in its
            //    post-call shape — correct for a void map
            //    (`out[i] = f(a[i])`): args popped, nothing pushed, the
            //    result reaches Java via the D→H writeback into `out`.
            //    A `HandledWithValue` outcome additionally carries the
            //    scalar the interpreter hook must push. Only `)I`/`)J`
            //    reductions qualify: GPU float atomic-add reorders the
            //    per-thread summation and is not bit-identical to
            //    Java's sequential fp accumulation, so `)F`/`)D`
            //    reductions (and any other non-void, non-reduction
            //    shape, e.g. an array return) still fall through to the
            //    CPU. (The analyzer/cache still classify and compile
            //    them; only the transparent launch is skipped here.)
            let is_void = method_descriptor.ends_with(")V");
            let is_int_reduction =
                kernel.signature.is_reduction && method_descriptor.ends_with(")I");
            let is_long_reduction =
                kernel.signature.is_reduction && method_descriptor.ends_with(")J");
            if !is_void && !is_int_reduction && !is_long_reduction {
                return Ok(DispatchOutcome::FallThrough);
            }
            // 2. Real per-element work must clear `--gpu-min-work`. The
            //    analyzer's `estimated_work` is a fixed 1<<20 placeholder
            //    for every counted loop, so it cannot gate small inputs;
            //    use the largest array argument's actual length. Below
            //    the threshold the host↔device round-trip dominates, so
            //    run on the CPU. Applies identically to reductions —
            //    their array-sized inputs vary per call exactly like a
            //    void kernel's — so a small-input reduction call also
            //    gets `FallThroughKeepHooked` rather than a permanent
            //    de-offload.
            let runtime_work = largest_primitive_array_len(shared, args);
            if (runtime_work as u32) < shared.config.gpu_min_work {
                // Per-call gate, not a property of the method: the next
                // call at this site may pass a larger array, so the site
                // must stay on the slow path where this hook can see it.
                return Ok(DispatchOutcome::FallThroughKeepHooked);
            }
            // Marshal args → device, launch the kernel, synchronize, and
            // write kernel-written arrays back into the Java heap (void)
            // or download the accumulator (reduction). This reuses the
            // explicit-path machinery (`dispatch_method_from_native`
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
            // Keep our own submission handle alive across
            // `release_submission` below (which only drops the
            // registry's reference) so a reduction can still read the
            // completed `SerializedResult` off `submission.status`
            // after the registry entry is gone.
            let submission = lookup_submission(handle);
            let result = match &submission {
                Some(sub) => finalize_submission(shared, sub),
                None => Err("offload submission was not registered".to_string()),
            };
            release_submission(handle);
            match result {
                Ok(()) => {
                    tracing::debug!(
                        "gpu offload: {}.{}{} ran on device (n={}, thread={:?}, frame={})",
                        class_name,
                        method_name,
                        method_descriptor,
                        runtime_work,
                        thread.name,
                        frame_idx,
                    );
                    if is_int_reduction || is_long_reduction {
                        let scalar = submission.as_ref().and_then(|sub| {
                            let status = sub.status.lock();
                            match &*status {
                                SubmissionStatus::Completed {
                                    result: SerializedResult::ScalarI32(v),
                                } => Some(cratonvm_types::Value::Int(*v)),
                                SubmissionStatus::Completed {
                                    result: SerializedResult::ScalarI64(v),
                                } => Some(cratonvm_types::Value::Long(*v)),
                                _ => None,
                            }
                        });
                        return Ok(match scalar {
                            Some(value) => DispatchOutcome::HandledWithValue(value),
                            None => {
                                // Kernel finished but the submission
                                // carries no scalar result — a Part E
                                // wiring bug, not a device error. Fall
                                // through rather than push a bogus
                                // value onto the operand stack.
                                tracing::debug!(
                                    "gpu offload: {}.{}{} completed without a scalar \
                                     accumulator result; falling back to CPU",
                                    class_name,
                                    method_name,
                                    method_descriptor,
                                );
                                DispatchOutcome::FallThrough
                            }
                        });
                    }
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
            if shared.mem.heap.array_element_type(*r).is_some() {
                let len = shared.mem.heap.array_length(*r);
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

    // ── Known-issues followups #3: poll_submission_status ────────────
    //
    // These exercise `poll_submission_status` against hand-built
    // `StreamSubmission`s (bypassing `dispatch_async`/`submitMethod`)
    // so they run on this no-GPU dev box without a real `Stream` or
    // `Event` — the same reason the rest of this file's tests stop at
    // the "no device" boundary. A real-device poll (Running ->
    // Event::query -> inline finalize -> Completed, and the
    // host-callback fast path) needs a live CUDA context to construct
    // a `Stream`/`Event` at all and is exercised on GPU hardware, not
    // here.

    #[test]
    fn poll_unknown_handle_returns_none() {
        let shared = crate::vm::SharedVm::new(VmConfig::default());
        // `NEXT_SUBMISSION_HANDLE` starts at 1 and only increases, so
        // 0 is never issued to a real submission — always "unknown".
        assert!(poll_submission_status(&shared, 0).is_none());
    }

    #[test]
    fn poll_dispatch_time_failed_submission_returns_failed() {
        let shared = crate::vm::SharedVm::new(VmConfig::default());
        // Mirrors exactly what `record_failed_submission` builds for a
        // pre-launch failure (no device, unknown kernel, marshal
        // error, ...): `stream`/`event` both `None`, status already
        // terminal at `Failed`.
        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sub = std::sync::Arc::new(StreamSubmission {
            handle,
            stream: None,
            event: None,
            status: parking_lot::Mutex::new(SubmissionStatus::Failed {
                message: "test: dispatch-time failure".to_string(),
                kind: GpuErrorKind::Unknown,
            }),
            finalize: parking_lot::Mutex::new(None),
            device_done: std::sync::atomic::AtomicBool::new(false),
        });
        register_submission(sub);

        assert_eq!(
            poll_submission_status(&shared, handle),
            Some(PollOutcome::Failed),
        );

        release_submission(handle);
        assert!(lookup_submission(handle).is_none());
    }

    #[test]
    fn poll_running_with_no_recorded_event_reports_running() {
        // Defensive branch: `Running` with `event: None` shouldn't
        // happen on any real `dispatch_async` path (every success
        // route records an event before returning `Running`), but
        // `poll_submission_status` must not panic or misreport if it
        // ever does — it can't claim more than "still running" with
        // nothing to query.
        let shared = crate::vm::SharedVm::new(VmConfig::default());
        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sub = std::sync::Arc::new(StreamSubmission {
            handle,
            stream: None,
            event: None,
            status: parking_lot::Mutex::new(SubmissionStatus::Running),
            finalize: parking_lot::Mutex::new(None),
            device_done: std::sync::atomic::AtomicBool::new(false),
        });
        register_submission(sub);

        assert_eq!(
            poll_submission_status(&shared, handle),
            Some(PollOutcome::Running),
        );

        release_submission(handle);
    }

    // ── Known-issues followups #3: completion reaper ──────────────────
    //
    // These test the reaper mechanism directly against hand-built
    // `StreamSubmission`s (same rationale as the `poll_submission_status`
    // tests above: no real `Stream`/`Event` on this no-GPU dev box) —
    // `stream`/`event` stay `None`, so `finalize_submission` skips the
    // event-sync step and goes straight to draining `writebacks` and
    // transitioning status, exactly as it would for a real submission
    // whose event has already fired. `dispatch_async`'s own wiring
    // (`ensure_completion_reaper_started` + the host callback calling
    // `enqueue_completion`) needs a live CUDA context to reach its
    // success path at all and is validated on GPU hardware instead
    // (see `fixed-suite-bugs/gpu-offload-followups-20260711.md`).

    #[test]
    fn reaper_finalizes_submission_without_any_poll_call() {
        // The whole point of this followup: completion happens with
        // zero calls to `poll_submission_status` / `finalize_submission`
        // / `get()` — only `enqueue_completion`, exactly like the host
        // callback in `dispatch_async` performs on its own.
        let vm = crate::vm::Vm::new(VmConfig::default());
        let weak_vm = vm
            .shared
            .self_arc
            .read()
            .as_ref()
            .cloned()
            .expect("Vm::new populates self_arc");

        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sub = std::sync::Arc::new(StreamSubmission {
            handle,
            stream: None,
            event: None,
            status: parking_lot::Mutex::new(SubmissionStatus::Running),
            finalize: parking_lot::Mutex::new(Some(FinalizeState {
                writebacks: Vec::new(),
                _gc_critical: GcCriticalGuard::acquire(),
            })),
            device_done: std::sync::atomic::AtomicBool::new(false),
        });
        register_submission(sub.clone());

        ensure_completion_reaper_started();
        enqueue_completion(handle, weak_vm, sub.clone());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !matches!(&*sub.status.lock(), SubmissionStatus::Running) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "completion reaper did not finalize handle={handle} within 5s",
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            matches!(&*sub.status.lock(), SubmissionStatus::Completed { .. }),
            "expected Completed, got {:?}",
            std::mem::discriminant(&*sub.status.lock()),
        );

        release_submission(handle);
    }

    // `finalize_enqueued_handle` (the per-handle work
    // `completion_reaper_loop` does, factored out — see its doc
    // comment) is tested directly here rather than through the
    // process-global reaper thread: `ensure_completion_reaper_started`
    // is `Once`-guarded for the whole test binary process, so only
    // ONE test may ever call it (that's
    // `reaper_finalizes_submission_without_any_poll_call` below) — a
    // second test racing it for which `weak_vm` "wins" would be
    // order-dependent and flaky.

    #[test]
    fn finalize_enqueued_handle_upgrades_and_finalizes() {
        let vm = crate::vm::Vm::new(VmConfig::default());
        let weak_vm = vm
            .shared
            .self_arc
            .read()
            .as_ref()
            .cloned()
            .expect("Vm::new populates self_arc");

        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sub = std::sync::Arc::new(StreamSubmission {
            handle,
            stream: None,
            event: None,
            status: parking_lot::Mutex::new(SubmissionStatus::Running),
            finalize: parking_lot::Mutex::new(Some(FinalizeState {
                writebacks: Vec::new(),
                _gc_critical: GcCriticalGuard::acquire(),
            })),
            device_done: std::sync::atomic::AtomicBool::new(false),
        });
        register_submission(sub.clone());

        finalize_enqueued_handle(&weak_vm, handle);

        assert!(matches!(
            &*sub.status.lock(),
            SubmissionStatus::Completed { .. }
        ));
        release_submission(handle);
    }

    #[test]
    fn finalize_enqueued_handle_noop_when_vm_dropped() {
        // A `SharedVm` built directly (not via `Vm::new`, as most of
        // this file's own tests do) never populates `self_arc`, so
        // `Weak::default()` is what `dispatch_method_from_native_on_stream`
        // would pass through. `finalize_enqueued_handle` must degrade
        // to a no-op in that case rather than panicking — the
        // existing poll-based path (`poll_submission_status`, `get()`)
        // is what such a caller relies on instead. Also covers the
        // genuine "VM torn down mid-flight" case, since an upgrade
        // failure looks identical either way.
        let weak_vm: std::sync::Weak<crate::vm::SharedVm> = std::sync::Weak::default();
        assert!(weak_vm.upgrade().is_none());

        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sub = std::sync::Arc::new(StreamSubmission {
            handle,
            stream: None,
            event: None,
            status: parking_lot::Mutex::new(SubmissionStatus::Running),
            finalize: parking_lot::Mutex::new(Some(FinalizeState {
                writebacks: Vec::new(),
                _gc_critical: GcCriticalGuard::acquire(),
            })),
            device_done: std::sync::atomic::AtomicBool::new(false),
        });
        register_submission(sub.clone());

        finalize_enqueued_handle(&weak_vm, handle);

        assert!(matches!(&*sub.status.lock(), SubmissionStatus::Running));
        release_submission(handle);
    }

    // ── GpuStream affinity — stream registry ─────────────────────────
    //
    // Exercise `OffloadCache::stream_create` / `stream_release` /
    // `resolve_stream` directly. On this no-GPU dev box `ctx` is
    // always `None` (see `offload_cache_skips_when_no_device` above),
    // so `stream_create` always returns `None` — the same "no device"
    // contract every other GPU-dependent path in this file already
    // has. A live create/resolve/release round trip needs a real CUDA
    // context and is exercised on GPU hardware, not here.

    #[test]
    fn stream_create_without_device_returns_none() {
        let mut config = VmConfig::default();
        config.gpu_offload_enabled = true;
        let cache = OffloadCache::new(&config);
        assert!(!cache.has_device());
        assert_eq!(cache.stream_create(), None);
    }

    #[test]
    fn resolve_unknown_stream_handle_returns_none() {
        let config = VmConfig::default();
        let cache = OffloadCache::new(&config);
        // Never created anything — every handle, including the
        // sentinel-ish 0 and 1 (the first value `stream_create` would
        // hand out on a real device), must resolve to `None`.
        assert!(cache.resolve_stream(0).is_none());
        assert!(cache.resolve_stream(1).is_none());
    }

    #[test]
    fn release_unknown_stream_handle_is_a_safe_no_op() {
        let config = VmConfig::default();
        let cache = OffloadCache::new(&config);
        // Must not panic on a handle that was never created, and must
        // be idempotent (mirrors `release_submission`'s contract).
        cache.stream_release(42);
        cache.stream_release(42);
        assert!(cache.resolve_stream(42).is_none());
    }

    #[test]
    fn dispatch_method_from_native_is_the_none_stream_wrapper() {
        // `dispatch_method_from_native` must still exist with its old
        // 5-arg signature (existing callers: `try_dispatch`,
        // `vm/tests/gpu_offload_features.rs`) and behave exactly like
        // `dispatch_method_from_native_on_stream(..., None)`. This box
        // has no classpath configured, so the dispatch fails at
        // class-load — the exact failure branch doesn't matter here,
        // only that both entry points hand back a valid (nonzero),
        // resolvable, terminally-`Failed` submission handle.
        let shared = crate::vm::SharedVm::new(VmConfig::default());
        let h1 = dispatch_method_from_native(&shared, "NoSuchClass", "m", "()V", &[]);
        let h2 =
            dispatch_method_from_native_on_stream(&shared, "NoSuchClass", "m", "()V", &[], None);
        assert!(h1 > 0 && h2 > 0 && h1 != h2);
        for h in [h1, h2] {
            let sub = lookup_submission(h).expect("handle must resolve to a submission");
            assert!(matches!(
                &*sub.status.lock(),
                SubmissionStatus::Failed { .. }
            ));
            release_submission(h);
        }
    }

    #[test]
    fn dispatch_method_from_native_on_stream_unknown_handle_still_fails_cleanly() {
        // A bogus `stream_handle` must never panic. On this no-GPU box
        // the no-device check (step 4) trips before stream resolution
        // (step 5) is ever reached, so this can't directly observe the
        // "unknown or released stream handle" message — that specific
        // branch needs a real device (`cache.has_device()` true) to
        // reach, same limitation as `offload_cache_rejects_ineligible_method`
        // above. What's verified here is the plumbing: passing
        // `Some(_)` all the way through doesn't change the "always get
        // a valid, terminally-Failed handle back" contract.
        let shared = crate::vm::SharedVm::new(VmConfig::default());
        let handle = dispatch_method_from_native_on_stream(
            &shared,
            "NoSuchClass",
            "m",
            "()V",
            &[],
            Some(999_999),
        );
        assert!(handle > 0);
        let sub = lookup_submission(handle).expect("handle must resolve to a submission");
        assert!(matches!(
            &*sub.status.lock(),
            SubmissionStatus::Failed { .. }
        ));
        release_submission(handle);
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
/// can unmarshal without touching device memory directly. `Void`, the
/// four primitive-array shapes, and (Part E) the four scalar-return
/// shapes are what `dispatch_method_from_native` /
/// `finalize_submission` populate today; surfacing a *new* primitive
/// array as the future's result (rather than writing to a caller-owned
/// `out` param) is still unwired — see `dispatch_async`'s doc comment.
#[cfg(feature = "gpu-offload")]
pub enum SerializedResult {
    /// The kernel had a void return — nothing to copy back beyond
    /// the host-side output array, which the caller already owns.
    Void,
    /// Scalar-return accumulator readback (`)I` descriptor). Set by
    /// `finalize_submission` from `MarshalWriteback::ScalarI32`'s
    /// download. The common case is a proven integer reduction
    /// (`KernelSignature::is_reduction`), but any scalar-`int`-return
    /// kernel gets this variant — the atomic-add-vs-plain-store choice
    /// is baked into the compiled PTX, not visible here.
    ScalarI32(i32),
    /// Scalar-return accumulator readback (`)J` descriptor). See
    /// `ScalarI32`.
    ScalarI64(i64),
    /// Scalar-return accumulator readback (`)F` descriptor). GPU float
    /// atomic-add reorders the per-thread summation and is therefore
    /// not bit-identical to Java's sequential fp accumulation — the
    /// transparent `try_dispatch` hook never requests this variant
    /// (its Hit arm gates reductions to `)I`/`)J` only), but the
    /// explicit `submitMethod` API surface can still dispatch a float
    /// scalar-return kernel deliberately.
    ScalarF32(f32),
    /// Scalar-return accumulator readback (`)D` descriptor). Same
    /// float-nondeterminism caveat as `ScalarF32`.
    ScalarF64(f64),
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
    /// as `GpuException`, and picks which `GpuException` subclass from
    /// `kind` — recorded here, at the point of failure, rather than
    /// reconstructed on the Java side by matching substrings against
    /// the driver's wording.
    Failed { message: String, kind: GpuErrorKind },
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
    /// Known-issues followups #3 — best-effort completion flag set by
    /// a `Stream::add_host_callback` closure registered right after
    /// the completion event in `OffloadCache::dispatch_async`. When
    /// the driver flips this to `true`, [`poll_submission_status`]
    /// can skip the `Event::query` round trip entirely.
    ///
    /// Always `false` for submissions that never reach the launch
    /// site (pre-launch failures via `record_failed_submission`) —
    /// harmless, since those are already terminal and
    /// `poll_submission_status` never consults this field for a
    /// non-`Running` status.
    ///
    /// The callback closure touches nothing but this atomic (see the
    /// registration site for why: `cuLaunchHostFunc` callbacks must
    /// never call back into the CUDA driver).
    pub device_done: std::sync::atomic::AtomicBool,
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

    /// GpuStream affinity — mint a new Java-visible CUDA stream bound
    /// to this cache's device context.
    ///
    /// Returns `None` when the cache has no device (`ctx` is `None` —
    /// no `--gpu`/no driver) or the underlying `Stream::new` call
    /// fails; callers treat that identically to every other
    /// no-driver fallback in this file (`Native.newStream`'s override
    /// hands back a purely local synthetic handle instead — see
    /// `native-builtins/src/craton_gpu.rs::builtin_new_stream`).
    ///
    /// The returned handle is the SAME value later passed to
    /// [`dispatch_method_from_native_on_stream`]'s `stream_handle`
    /// parameter (resolved via [`resolve_stream`](Self::resolve_stream))
    /// to pin a dispatch onto this exact stream.
    ///
    /// # Ordering contract
    ///
    /// Two submissions dispatched onto the SAME registered stream
    /// serialize in submission order — that is what a CUDA stream
    /// gives for free (kernels/copies enqueued on one stream execute
    /// in enqueue order; the host never has to arrange this itself).
    /// This composes with, and does not replace, the existing
    /// per-buffer `last_write` event choreography the device-residency
    /// cache (`device_cache`/`marshal_array_arg`) already does: that
    /// mechanism serializes *data hazards* on a shared buffer across
    /// ANY two streams (it makes a later kernel's read/write wait on
    /// an earlier kernel's completion event regardless of which
    /// stream either ran on); stream affinity additionally serializes
    /// *launch order* for everything queued on one specific stream,
    /// which is a strictly stronger, purely additive guarantee for
    /// same-stream submissions.
    pub fn stream_create(&self) -> Option<u64> {
        let ctx = self.ctx.as_ref()?;
        let stream = match Stream::new(ctx) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::info!("gpu stream: Stream::new failed: {e}");
                return None;
            }
        };
        let handle = self
            .next_stream_handle
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.streams.write().insert(handle, stream);
        Some(handle)
    }

    /// Drop this cache's reference to a previously-created stream.
    /// Safe to call on an unknown or already-released handle
    /// (no-op) — same idempotent-release convention as
    /// [`release_submission`].
    ///
    /// This does not necessarily destroy the underlying CUDA stream
    /// immediately: any [`StreamSubmission`] already dispatched onto
    /// it via `resolve_stream` holds its own `Arc<Stream>` clone
    /// (`StreamSubmission::stream`), so the stream stays alive until
    /// every submission that used it is also finalized and dropped.
    /// A dispatch against this handle *after* release fails with
    /// "unknown or released stream handle" rather than silently
    /// reusing a stream Java already asked us to forget.
    pub fn stream_release(&self, handle: u64) {
        self.streams.write().remove(&handle);
    }

    /// Resolve a Java-visible stream handle back to its `Arc<Stream>`.
    /// Returns `None` for a handle this cache never minted (wrong
    /// device / typo'd handle) or one that was already released.
    pub fn resolve_stream(&self, handle: u64) -> Option<Arc<Stream>> {
        self.streams.read().get(&handle).cloned()
    }

    /// The per-chunk completion events, reused across dispatches.
    ///
    /// Returns an empty vec when the pool cannot be used — either events
    /// could not be created, or a previous submission still holds one, in
    /// which case the caller creates its own rather than re-recording an
    /// event someone is waiting on.
    #[cfg(feature = "gpu-offload")]
    fn chunk_event_pool(
        &self,
        ctx: &cuda_bridge::DeviceContext,
        want: usize,
    ) -> Vec<std::sync::Arc<cuda_bridge::Event>> {
        {
            let held = self.chunk_events.read();
            if held.len() >= want && held.iter().all(|e| std::sync::Arc::strong_count(e) == 1) {
                return held[..want].to_vec();
            }
        }
        let mut slot = self.chunk_events.write();
        if slot.len() >= want && slot.iter().all(|e| std::sync::Arc::strong_count(e) == 1) {
            return slot[..want].to_vec();
        }
        let mut made = Vec::with_capacity(want);
        for _ in 0..want {
            match cuda_bridge::Event::new(ctx) {
                Ok(e) => made.push(std::sync::Arc::new(e)),
                Err(_) => return Vec::new(),
            }
        }
        // Only cache when nothing else is holding the old set; otherwise
        // hand these out one-shot and leave the pool alone.
        if slot.iter().all(|e| std::sync::Arc::strong_count(e) == 1) {
            *slot = made.clone();
        }
        made
    }

    /// The internal stream pool the chunked writeback rotates over,
    /// created on first use.
    ///
    /// Returns an empty vec if streams cannot be created, which the
    /// caller reads as "do not chunk" and falls back to the single
    /// whole-array launch.
    #[cfg(feature = "gpu-offload")]
    /// A stream the built-in kernels launch on, created once and reused.
    ///
    /// Built-ins are not on a caller-named `GpuStream`: nothing in the
    /// Java API lets a caller place a `GpuBlas.gemm` on a particular
    /// stream yet. Reusing one is what makes consecutive GEMMs ordered
    /// with respect to each other, which is what a caller chaining
    /// projections actually wants, and it avoids a `cuStreamCreate` per
    /// call.
    ///
    /// Borrows the chunked-writeback pool's first stream rather than
    /// adding another: that pool already exists per device, is created
    /// lazily, and a built-in launch and a chunked writeback never
    /// contend for ordering (a chunked writeback belongs to one
    /// bytecode dispatch, which has already been given its own stream).
    fn default_internal_stream(&self) -> Result<std::sync::Arc<Stream>, cuda_bridge::DeviceError> {
        let ctx = self
            .device()
            .ok_or(cuda_bridge::DeviceError::NoDriver)?;
        if let Some(s) = self.chunk_stream_pool(ctx).first() {
            return Ok(std::sync::Arc::clone(s));
        }
        // The pool declines to build itself when stream creation fails;
        // try once directly so the caller gets the driver's own error
        // rather than a bare "no streams".
        Stream::new(ctx).map(std::sync::Arc::new)
    }

    fn chunk_stream_pool(&self, ctx: &cuda_bridge::DeviceContext) -> Vec<std::sync::Arc<Stream>> {
        {
            let have = self.chunk_streams.read();
            if !have.is_empty() {
                return have.clone();
            }
        }
        let mut slot = self.chunk_streams.write();
        // Another thread may have filled it while the read lock was down.
        if !slot.is_empty() {
            return slot.clone();
        }
        let mut made = Vec::with_capacity(chunk_streams_wanted());
        for _ in 0..chunk_streams_wanted() {
            match Stream::new(ctx) {
                Ok(s) => made.push(std::sync::Arc::new(s)),
                Err(e) => {
                    tracing::debug!(
                        "gpu offload: chunk stream pool unavailable ({e}); \
                                     falling back to the whole-array writeback"
                    );
                    return Vec::new();
                }
            }
        }
        *slot = made.clone();
        made
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
    ///   `DeviceModule::launch_on_stream`, a completion `Event` is
    ///   recorded right after the launch, and the submission is
    ///   returned immediately with status `Running` — `dispatch_async`
    ///   itself never blocks on the device. On success the status
    ///   eventually becomes `Completed { result }` once something
    ///   finalizes the submission (see below). On any cudarc launch
    ///   error the status becomes `Failed` with the error text right
    ///   away.
    ///
    /// # Why synchronous-under-the-hood? (get() still blocks; isDone() no longer has to)
    ///
    /// `dispatch_async` used to call `stream.synchronize()` before
    /// returning — genuinely synchronous under an async-sounding name.
    /// Phase 7 #1 replaced that: the function now only launches +
    /// records an event and returns `Running` right away, deferring
    /// the wait-and-drain-writebacks work to [`finalize_submission`].
    /// `finalize_submission` is what actually blocks — it calls
    /// `event.synchronize()` — and it only runs on the *first* call
    /// made through it, whether that's the Java side's blocking
    /// `GpuFuture::get()` (via `futureGetResult` / `futureSynchronize`)
    /// or, as of known-issues followups item 3, a non-blocking poll.
    ///
    /// That poll is [`poll_submission_status`]: it asks the driver
    /// "has the event fired?" via `Event::query` (optionally answered
    /// even cheaper by a `Stream::add_host_callback` flag set at
    /// dispatch time — see the callback registered right after the
    /// event in this function) instead of waiting for it. If the
    /// event has already fired, it runs the same finalize path inline
    /// — bounded work, no device wait — and reports the resulting
    /// terminal status; otherwise it reports `Running` and returns
    /// immediately. This is what backs `Native.futureIsDone`: the
    /// Java side can now ask "is it done yet?" without blocking a
    /// thread on `event.synchronize()`, while `get()` / `futureGetResult`
    /// keep their original blocking contract unchanged.
    ///
    /// # Limitation: `Void` and scalar returns only
    ///
    /// `SerializedResult::Void` covers the common shape
    /// (`kernel(int[] a, int[] b, int[] out)`) — the result is signalled
    /// by the caller writing to an output buffer the host already
    /// owns, no return value to surface. `SerializedResult::ScalarI32
    /// / ScalarI64 / ScalarF32 / ScalarF64` (Part E) cover scalar-return
    /// kernels (`)I` / `)J` / `)F` / `)D`, most commonly a proven
    /// reduction): `dispatch_method_from_native` allocates and marshals
    /// the accumulator buffer the kernel writes into, and
    /// `finalize_submission` downloads it into the matching
    /// `SerializedResult` variant. Surfacing a *new primitive array* as
    /// the future's result (e.g. for a method that returns `int[]`
    /// rather than writing to `out`) still needs the dispatch site to
    /// allocate the output buffer, copy it back after the kernel, and
    /// stamp it into `SerializedResult::PrimitiveArray*`. That belongs
    /// to a later round.
    /// Asynchronously launch a compiled kernel on `stream`.
    ///
    /// `runtime_work` is the per-element launch count derived from the
    /// actual array length the caller marshalled. Pass `0` for
    /// scalar-only kernels (no array params, no per-element loop) — the
    /// launch grid then falls back to the analyzer's
    /// `KernelSignature::estimated_work`. We use `u32` rather than
    /// `Option<u32>` because the MSVC x64 ABI's handling of
    /// `Option<u32>` was observed to corrupt stack passed CUDA kernel
    /// arguments on Windows when this function is called via the GPU
    /// dispatch chain — passing the raw `u32` / sentinel-0 encoding is
    /// the workaround.
    ///
    /// AUDIT (Part E launch-config fix): previously this took
    /// `max(runtime_work, estimated_work)`, which meant the analyzer's
    /// fixed `1 << 20` placeholder silently floored every launch to at
    /// least a million threads — wasteful for any real array smaller
    /// than that (the common case) and not actually needed: a genuine
    /// `runtime_work` of `n` always wants exactly `n` threads, never
    /// `max(n, 2^20)`. `runtime_work > 0` now wins outright; `estimated_work`
    /// is consulted only for the `0`-sentinel scalar-only case, where
    /// there is no array length to derive a grid from.
    ///
    /// `finalize_state`, if present, is attached to the returned
    /// submission's [`StreamSubmission::finalize`] *before* the
    /// completion host callback is registered (see step 6/7 below) —
    /// this ordering is load-bearing, not incidental: known-issues
    /// followups #3's completion reaper can run `finalize_submission`
    /// as soon as the callback fires, and if that fired before
    /// `finalize` were populated it would find `None` and give up
    /// without ever completing the submission. `None` means "no
    /// writebacks to drain" (a caller with nothing to finalize, or a
    /// test harness).
    ///
    /// `weak_vm` is what the completion reaper (spawned lazily, at
    /// most once per process — see [`ensure_completion_reaper_started`])
    /// upgrades to call `finalize_submission` from its own thread,
    /// off the caller entirely. A `Weak::default()` (unpopulated
    /// `SharedVm::self_arc`, true for any `SharedVm` not constructed
    /// via `Vm::new`) degrades gracefully: the reaper's `upgrade()`
    /// always fails, and the existing poll-based completion path
    /// (`poll_submission_status`, `get()`) remains fully correct.

    /// (`poll_submission_status`, `get()`) remains fully correct.
    pub fn dispatch_async(
        &self,
        stream: std::sync::Arc<Stream>,
        class_id: ClassId,
        method_index: u16,
        args: cuda_bridge::KernelArgs,
        runtime_work: u32,
        finalize_state: Option<FinalizeState>,
        weak_vm: std::sync::Weak<crate::vm::SharedVm>,
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
                    device_done: std::sync::atomic::AtomicBool::new(false),
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
                    kind: GpuErrorKind::Launch,
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
                    kind: GpuErrorKind::Compile,
                });
            }
        };

        // The caller-supplied `runtime_work` is the actual array
        // length the kernel will iterate over — always exact when
        // present, so it wins outright (no more flooring against the
        // analyzer's fixed `1 << 20` counted-loop placeholder; see the
        // doc comment above). Only the `0` sentinel (scalar-only
        // kernel, no array params to size a grid from) falls back to
        // `estimated_work`.
        let work = if runtime_work > 0 {
            runtime_work
        } else {
            kernel.signature.estimated_work.max(1) as u32
        };
        // Occupancy-tuned block size (queries the driver for this
        // kernel's `cuOccupancyMaxPotentialBlockSize`) instead of the
        // fixed `DEFAULT_ELEMENTWISE_BLOCK` — falls back to the same
        // 256 default when the query is unavailable (stub mode, or the
        // driver call fails).
        // Occupancy-tuned block size, queried ONCE per kernel and then
        // memoised: the driver's answer is a property of the kernel, not
        // of this launch's element count, and paying for it on every
        // dispatch is hundreds of driver round trips per inference step.
        let cfg = match kernel.block_size.load(std::sync::atomic::Ordering::Relaxed) {
            0 => {
                let cfg = kernel
                    .module
                    .elementwise_for_kernel(ctx, &kernel.kernel_name, work);
                kernel
                    .block_size
                    .store(cfg.block.0, std::sync::atomic::Ordering::Relaxed);
                cfg
            }
            block => cuda_bridge::LaunchConfig::elementwise_with_block(work, block),
        };

        // 3b. Chunked, overlapped writeback.
        //
        //     A single whole-array launch cannot overlap anything: the
        //     device->host copy can only start once the entire kernel has
        //     finished. Splitting the iteration space lets chunk N's copy
        //     run while chunk N+1's kernel does, which on the four-sphere
        //     ray tracer at 2.76M elements is 0.97 ms against 1.62 ms
        //     serial. `take_chunkable_writeback` decides whether this
        //     submission qualifies -- crucially, only for an array the
        //     kernel writes and never reads.
        //
        //     Any failure here falls back to the whole-array launch
        //     below with the writeback put back, so a chunking problem
        //     costs performance and never correctness.
        let mut finalize_state = finalize_state;
        let mut chunked = false;
        if let Some(fs) = finalize_state.as_mut() {
            if let Some(plain) =
                take_chunkable_writeback(&kernel.signature, &mut fs.writebacks, work)
            {
                let pool = self.chunk_stream_pool(ctx);
                if pool.is_empty() {
                    fs.writebacks.push(plain);
                } else {
                    // Cast: `work` is a JVM array length, so it fits usize.
                    let events = self.chunk_event_pool(ctx, chunk_count_wanted());
                    match launch_chunked(
                        self,
                        &events,
                        ctx,
                        &kernel,
                        &args,
                        work as usize,
                        &pool,
                        plain,
                    ) {
                        Ok(wb) => {
                            // Give the submission a completion event that
                            // really covers every chunk: the user stream
                            // waits on each chunk event before the
                            // `record_event` below, so `isDone()` and the
                            // reaper stay honest even though the work ran
                            // on the pool streams.
                            if let MarshalWriteback::Chunked { chunks, .. } = &wb {
                                for c in chunks {
                                    if let Err(e) = stream.wait_event(&c.done) {
                                        return make(SubmissionStatus::Failed {
                                            message: format!(
                                                "chunked join wait (lo={}): {e}",
                                                c.lo
                                            ),
                                            kind: GpuErrorKind::Launch,
                                        });
                                    }
                                }
                            }
                            // The chunked writeback must drain BEFORE the
                            // failure flag, so its per-chunk waits overlap
                            // with the GPU work still outstanding. That is
                            // the whole point, and it is why chunking is
                            // gated on a write-only array: see
                            // `take_chunkable_writeback`.
                            fs.writebacks.insert(0, wb);
                            chunked = true;
                        }
                        Err(msg) => {
                            tracing::debug!(
                                "gpu offload: chunked dispatch unavailable ({msg}); \
                                 falling back to the whole-array launch"
                            );
                            // `launch_chunked` consumed the writeback on the
                            // error path, so rebuild the submission as failed
                            // rather than silently dropping the array's
                            // writeback and returning stale Java state.
                            return make(SubmissionStatus::Failed {
                                message: format!("chunked dispatch failed: {msg}"),
                                kind: GpuErrorKind::Launch,
                            });
                        }
                    }
                }
            }
        }
        let finalize_state = finalize_state;

        // 4. Launch on the user-supplied stream. The launch itself is
        //    non-blocking; `stream.synchronize()` below is what makes
        //    this call observably synchronous to the caller.
        // Skipped when the chunked path above already launched every
        // chunk on the pool streams.
        if !chunked {
            if let Err(e) =
                kernel
                    .module
                    .launch_on_stream(ctx, &kernel.kernel_name, &cfg, args, &stream)
            {
                return make(SubmissionStatus::Failed {
                    message: format!("launch_on_stream({}): {}", kernel.kernel_name, e,),
                    kind: kind_of_device_error(&e),
                });
            }
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
                    kind: kind_of_device_error(&e),
                });
            }
        };
        if let Err(e) = stream.record_event(&event) {
            return make(SubmissionStatus::Failed {
                message: format!("Stream::record_event: {e}"),
                kind: kind_of_device_error(&e),
            });
        }

        // 6. Build the Running submission. The status flips to
        //    Completed inside `finalize_submission` after the event
        //    fires and the writebacks complete.
        let submission = make_with_event(SubmissionStatus::Running, Some(event));

        // 6b. Attach `finalize_state` BEFORE registering the host
        //     callback below (step 7) — see the ordering note on this
        //     function's doc comment. Both stub mode (where
        //     `add_host_callback` runs the closure synchronously,
        //     before it even returns) and real hardware (where the
        //     kernel can in principle complete before this host
        //     thread's next instruction runs) can otherwise race the
        //     callback against this attach.
        *submission.finalize.lock() = finalize_state;

        // 7. Known-issues followups #3 — start (idempotent) the
        //    process-wide completion reaper, then register a host
        //    callback that both flags `device_done` (the existing
        //    poll-fast-path optimization for `isDone()`/`getNow()`)
        //    and enqueues this submission's handle onto the reaper so
        //    it gets finalized on its own, with no Java call required.
        //    Registration failing for any reason is non-fatal:
        //    `poll_submission_status` falls back to `Event::query`,
        //    which is always correct on its own, and a still-`Running`
        //    submission just waits for an explicit `get()`/`isDone()`
        //    exactly like before this followup landed.
        //
        //    The closure clones the submission's `Arc` and otherwise
        //    only touches `AtomicBool::store` + `enqueue_completion`
        //    (a plain mutex/condvar push, no CUDA driver call) —
        //    `cuLaunchHostFunc` callbacks run on a driver-owned thread
        //    and must never re-enter the CUDA driver (no
        //    `Stream`/`Event`/`DeviceBuffer` calls) or block for long.
        //    Keeping the submission alive via the clone until the
        //    callback fires is a side effect, not a goal, but it is a
        //    benign one: it means the submission (and any device
        //    buffers referenced by its pending `FinalizeState`) can't
        //    be freed out from under a kernel that's still in-flight
        //    on the device, even if every other reference (registry +
        //    caller) is dropped first.
        //
        //    AUDIT 2026-08-03: that "benign" reasoning above missed a
        //    real consequence — being the *last* strong reference means
        //    THIS closure is the one whose drop runs `StreamSubmission`'s
        //    (and transitively `cuda_bridge::Stream`'s / cudarc's
        //    `CudaStream`'s) destructor, and `CudaStream::drop` issues a
        //    real CUDA driver call (stream teardown/sync against the
        //    default stream). Doing that from inside a `cuLaunchHostFunc`
        //    callback is exactly the "must never re-enter the CUDA
        //    driver" rule stated two paragraphs up — verified on
        //    hardware (RTX 2060) via a reproducible `cudarc` panic,
        //    `DriverError(CUDA_ERROR_NOT_PERMITTED, "operation not
        //    permitted")`, out of `CudaStream`'s `Drop` impl, on every
        //    dispatch of a synchronous (non-Future-API) kernel like
        //    `GpuDotBench.dotReduce` — that path never calls
        //    `register_submission`, so by the time the driver actually
        //    invokes this callback the synchronous caller has already
        //    finished and dropped its own reference, leaving this
        //    closure's clone as the last one, 100% of the time.
        //    Fix: move `cb_submission` INTO `enqueue_completion` instead
        //    of letting it drop locally here. The reaper thread — an
        //    ordinary thread, not a CUDA callback — becomes the one that
        //    (maybe) runs the final drop, which is exactly where CUDA
        //    driver calls are allowed.
        ensure_completion_reaper_started();
        let cb_submission = submission.clone();
        let cb_weak_vm = weak_vm;
        if let Err(e) = stream.add_host_callback(Box::new(move || {
            cb_submission
                .device_done
                .store(true, std::sync::atomic::Ordering::Release);
            let cb_handle = cb_submission.handle;
            // Transfers ownership of `cb_submission` into the reaper
            // queue — see the AUDIT note above. Do not reintroduce a
            // local `Arc<StreamSubmission>`/`Arc<Stream>` drop in this
            // closure; that is precisely what re-enters the CUDA driver
            // from a host callback.
            enqueue_completion(cb_handle, cb_weak_vm, cb_submission);
        })) {
            tracing::debug!(
                "gpu offload: add_host_callback registration failed ({e}); \
                 poll_submission_status will fall back to Event::query for handle={handle}",
            );
        }

        submission
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
/// Dispatch a built-in GEMM: `C[MxN] = A[MxK] * B[KxN]`, row-major.
///
/// `a_handle`, `b_handle` and `c_handle` are `craton.gpu.GpuArray`
/// handles. Their device buffers come from the same `device_cache` the
/// bytecode dispatch path uses, so a weight matrix uploaded for one call
/// is still resident for the next — which is the whole point of taking
/// handles rather than Java arrays here. A decode step multiplies by the
/// same weights every token; re-uploading them would cost more than the
/// arithmetic.
///
/// Returns a submission handle with the same lifecycle as any other:
/// await it with `Native.futureSynchronize`, release it with
/// `Native.releaseFuture`. `C`'s contents land back in its `GpuArray` on
/// finalization.
///
/// `trans_a` / `trans_b` read the corresponding operand transposed. This
/// is expressed as strides rather than as separate kernels — see
/// [`crate::runtime::kernels::Strides`] — so the operand's element count
/// is unchanged and only its indexing differs.
///
/// `stream_handle` places the launch on a Java-visible `GpuStream`,
/// making it ordered with respect to everything else on that stream.
/// `None` uses the shared built-in stream, which orders built-ins against
/// each other and nothing else.
///
/// Errors are reported as a `Failed` submission rather than a panic, so
/// the Java side sees a `GpuException` carrying the reason.
#[cfg(feature = "gpu-offload")]
#[allow(clippy::too_many_arguments)]
pub fn dispatch_gemm(
    shared: &crate::vm::SharedVm,
    kind: crate::runtime::kernels::GemmKind,
    a_handle: u64,
    b_handle: u64,
    c_handle: u64,
    m: i32,
    n: i32,
    k: i32,
    trans_a: bool,
    trans_b: bool,
    stream_handle: Option<u64>,
) -> u64 {
    use crate::runtime::kernels;

    let cache = shared
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config);

    // Shape first: an out-of-range shape becomes an out-of-bounds global
    // read on the device, which is either silent garbage or a fault
    // attributed to some later launch.
    let (a_elems, b_elems, c_elems) = match kernels::gemm_shape(m, n, k) {
        Ok(t) => t,
        Err(e) => return record_failed_submission(None, GpuErrorKind::Compile, e),
    };

    let ctx = match cache.device() {
        Some(c) => c,
        None => {
            return record_failed_submission(
                None,
                GpuErrorKind::Launch,
                "gemm: no CUDA device available".to_string(),
            )
        }
    };

    let module = match cache.builtin_module() {
        Ok(m) => m,
        Err(e) => return record_failed_submission(None, GpuErrorKind::Compile, e),
    };

    let stream = match stream_handle {
        // A caller-named stream. An unknown handle is an error rather than
        // a silent fallback to the default: the whole reason to name a
        // stream is ordering, and quietly running somewhere else would
        // produce a race the caller specifically asked to avoid.
        Some(h) => match cache.resolve_stream(h) {
            Some(s) => s,
            None => {
                return record_failed_submission(
                    None,
                    GpuErrorKind::Launch,
                    format!("gemm: unknown or released stream handle {h}"),
                )
            }
        },
        None => match cache.default_internal_stream() {
            Ok(s) => s,
            Err(e) => {
                return record_failed_submission(None, kind_of_device_error(&e), e.to_string())
            }
        },
    };

    // C is always f32 and always written, so it is resolved (and, on a
    // cache miss, uploaded) the same way an output array is on the
    // bytecode path.
    let c_buf = match resident_f32(ctx, c_handle, c_elems, "C") {
        Ok(b) => b,
        Err(e) => return record_failed_submission(Some(stream), kind_of_device_message(&e), e),
    };

    let launch = kernels::gemm_launch_config(m, n);

    // `Strides::of` wants the operand's column count AS STORED, which is
    // not the logical one when the operand is transposed:
    //
    //   A is logically MxK. Untransposed it is stored MxK, so stored
    //   cols = K. Transposed, the logical MxK is a view of a stored KxM,
    //   so stored cols = M.
    //
    //   B is logically KxN. Untransposed, stored KxN, cols = N.
    //   Transposed, it is a view of a stored NxK, so cols = K.
    //
    // Passing the logical width instead is the classic version of this
    // bug: it agrees with the correct answer on a square operand and
    // indexes into the wrong element on every other shape.
    let a_strides = kernels::Strides::of(trans_a, if trans_a { m } else { k });
    let b_strides = kernels::Strides::of(trans_b, if trans_b { k } else { n });

    let args = match kind {
        kernels::GemmKind::F32 => {
            let a = match resident_f32(ctx, a_handle, a_elems, "A") {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(
                        Some(stream),
                        kind_of_device_message(&e),
                        e,
                    )
                }
            };
            let b = match resident_f32(ctx, b_handle, b_elems, "B") {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(
                        Some(stream),
                        kind_of_device_message(&e),
                        e,
                    )
                }
            };
            let args = kernels::gemm_args(&*a, &*b, &*c_buf, m, n, k, a_strides, b_strides);
            // The Arcs must outlive the launch; the writeback below holds
            // one for C, and these two keep A and B alive across it.
            let _keep = (a, b);
            args
        }
        kernels::GemmKind::F16 => {
            let a = match resident_i16(ctx, a_handle, a_elems, "A") {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(
                        Some(stream),
                        kind_of_device_message(&e),
                        e,
                    )
                }
            };
            let b = match resident_i16(ctx, b_handle, b_elems, "B") {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(
                        Some(stream),
                        kind_of_device_message(&e),
                        e,
                    )
                }
            };
            let args = kernels::gemm_args(&*a, &*b, &*c_buf, m, n, k, a_strides, b_strides);
            let _keep = (a, b);
            args
        }
    };

    // Which tile the launch config chose has to be the same decision the
    // entry point makes, or the grid is sized for one shape and the kernel
    // indexes for the other. Both ask `use_large_tile`.
    let entry = kind.entry_name(kernels::use_large_tile(m, n));
    if let Err(e) = module.launch_on_stream(ctx, entry, &launch, args, &stream) {
        return record_failed_submission(
            Some(stream),
            kind_of_device_error(&e),
            format!("gemm: launch_on_stream({entry}): {e}"),
        );
    }

    let event = match cuda_bridge::Event::new(ctx) {
        Ok(e) => std::sync::Arc::new(e),
        Err(e) => {
            return record_failed_submission(
                Some(stream),
                kind_of_device_error(&e),
                format!("gemm: Event::new after launch: {e}"),
            )
        }
    };
    if let Err(e) = stream.record_event(&event) {
        return record_failed_submission(
            Some(stream),
            kind_of_device_error(&e),
            format!("gemm: Stream::record_event: {e}"),
        );
    }

    // One writeback: C back into its GpuArray's host bytes. A and B are
    // inputs and stay device-resident.
    let writebacks = vec![MarshalWriteback::ResidentF32 {
        handle: c_handle,
        buf: c_buf,
        len: c_elems,
    }];
    // The same guard every other dispatch site takes: it keeps the
    // collector off the marshalled arrays until the writeback has drained.
    // `Heap::enter_gpu_critical` returns a token borrowed from the heap,
    // which cannot be stored in a submission that outlives this frame.
    let gc_guard = GcCriticalGuard::acquire();

    let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let submission = std::sync::Arc::new(StreamSubmission {
        handle,
        stream: Some(stream),
        event: Some(event),
        status: parking_lot::Mutex::new(SubmissionStatus::Running),
        finalize: parking_lot::Mutex::new(Some(FinalizeState {
            writebacks,
            _gc_critical: gc_guard,
        })),
        device_done: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(submission);
    handle
}

/// Resolve a `GpuArray` handle to a device-resident `f32` buffer,
/// uploading its host bytes on a cache miss.
#[cfg(feature = "gpu-offload")]
fn resident_f32(
    ctx: &DeviceContext,
    handle: u64,
    elems: usize,
    role: &str,
) -> Result<std::sync::Arc<cuda_bridge::DeviceBuffer<f32>>, String> {
    if let Some(arc) = device_cache::get_f32(handle) {
        if arc.len() < elems {
            return Err(format!(
                "gemm: {role} (handle {handle}) holds {} f32 elements, the shape needs {elems}",
                arc.len()
            ));
        }
        return Ok(arc);
    }
    let (ty, count, bytes) = cratonvm_native_builtins::craton_gpu::array_snapshot(handle)
        .ok_or_else(|| format!("gemm: {role} (handle {handle}) is not a live GpuArray"))?;
    if ty != cratonvm_types::ArrayElementType::Float {
        return Err(format!(
            "gemm: {role} (handle {handle}) is a {ty:?} array; this kernel needs float"
        ));
    }
    if count < elems {
        return Err(format!(
            "gemm: {role} (handle {handle}) has {count} elements, the shape needs {elems}"
        ));
    }
    // Check the bytes, not just the reported count. `array_snapshot` reports
    // a length for every element type but only fills `bytes` for the ones it
    // knows, so an unhandled type arrives as "N elements, zero bytes" — which
    // uploads as an empty device buffer and is then read off the end by the
    // kernel. Failing here turns that into a message rather than a
    // CUDA_ERROR_ILLEGAL_ADDRESS attributed to some later, unrelated call.
    if bytes.len() < elems * 4 {
        return Err(format!(
            "gemm: {role} (handle {handle}) reports {count} elements but carries only {} \
             bytes; the shape needs {elems} ({} bytes). Does the VM snapshot {ty:?} arrays?",
            bytes.len(),
            elems * 4
        ));
    }
    // `pod_collect_to_vec`, not `cast_slice`: the snapshot is a `Vec<u8>`,
    // whose allocation carries alignment 1, and reinterpreting it as `&[f32]`
    // requires 4. `cast_slice` panics when the allocator happened not to
    // oblige — which it does often enough to look like it works. This copies
    // instead, which the upload was going to do anyway.
    let host: Vec<f32> = bytemuck::pod_collect_to_vec(&bytes);
    let buf = crate::runtime::gpu_marshal::upload(ctx, &host)
        .map_err(|e| format!("gemm: uploading {role} (len={count}): {e}"))?;
    let arc = std::sync::Arc::new(buf);
    device_cache::put_f32(handle, arc.clone());
    Ok(arc)
}

/// Resolve a `GpuArray` handle to a device-resident 16-bit buffer.
///
/// The elements are IEEE-754 binary16 bit patterns carried in a Java
/// `short[]`, because Java has no half type. Nothing here interprets
/// them; the kernel reads the same bytes as `__half`.
#[cfg(feature = "gpu-offload")]
fn resident_i16(
    ctx: &DeviceContext,
    handle: u64,
    elems: usize,
    role: &str,
) -> Result<std::sync::Arc<cuda_bridge::DeviceBuffer<i16>>, String> {
    if let Some(arc) = device_cache::get_i16(handle) {
        if arc.len() < elems {
            return Err(format!(
                "gemm: {role} (handle {handle}) holds {} f16 elements, the shape needs {elems}",
                arc.len()
            ));
        }
        return Ok(arc);
    }
    let (ty, count, bytes) = cratonvm_native_builtins::craton_gpu::array_snapshot(handle)
        .ok_or_else(|| format!("gemm: {role} (handle {handle}) is not a live GpuArray"))?;
    if ty != cratonvm_types::ArrayElementType::Short {
        return Err(format!(
            "gemm: {role} (handle {handle}) is a {ty:?} array; an f16 kernel needs short \
             (binary16 bit patterns)"
        ));
    }
    if count < elems {
        return Err(format!(
            "gemm: {role} (handle {handle}) has {count} elements, the shape needs {elems}"
        ));
    }
    // Check the bytes, not just the reported count. `array_snapshot` reports
    // a length for every element type but only fills `bytes` for the ones it
    // knows, so an unhandled type arrives as "N elements, zero bytes" — which
    // uploads as an empty device buffer and is then read off the end by the
    // kernel. Failing here turns that into a message rather than a
    // CUDA_ERROR_ILLEGAL_ADDRESS attributed to some later, unrelated call.
    if bytes.len() < elems * 2 {
        return Err(format!(
            "gemm: {role} (handle {handle}) reports {count} elements but carries only {} \
             bytes; the shape needs {elems} ({} bytes). Does the VM snapshot {ty:?} arrays?",
            bytes.len(),
            elems * 2
        ));
    }
    // Alignment-safe, for the reason spelled out in `resident_f32`. This is
    // the path that actually panicked on an RTX 2060 —
    // `TargetAlignmentGreaterAndInputNotAligned` out of `cast_slice` — so
    // the hazard is not theoretical.
    let host: Vec<i16> = bytemuck::pod_collect_to_vec(&bytes);
    let buf = crate::runtime::gpu_marshal::upload(ctx, &host)
        .map_err(|e| format!("gemm: uploading {role} (len={count}): {e}"))?;
    let arc = std::sync::Arc::new(buf);
    device_cache::put_i16(handle, arc.clone());
    Ok(arc)
}

/// Live-submission count past which [`register_submission`] starts
/// warning.
///
/// AUDIT 2026-08-28: a submission that has not been finalized owns its
/// host-side writeback buffers, its device buffers, and a GC-critical
/// token — and the collector deliberately steps aside while such a token
/// is alive (`Heap::gpu_blocked_gc_count` counts the bail-outs). A caller
/// that dispatches in a loop without draining therefore does not merely
/// leak: it holds collection off for the whole run while the leak grows,
/// and neither `-Xmx` nor the Java-side cleaner can recover anything,
/// because both need a collection to happen. A benchmark doing exactly
/// that — a thousand un-drained `dispatchNamedHandle` calls — exhausted
/// 64 GB of host RAM and hung the machine, with no diagnostic anywhere
/// on the way down.
///
/// A legitimate deep pipeline is nowhere near this: the fire-and-forget
/// pattern this supports is tens of kernels between waits, not thousands.
#[cfg(feature = "gpu-offload")]
const SUBMISSION_WARN_THRESHOLD: usize = 1024;

/// Register `sub` in the global submission table and return its
/// handle. The caller (typically the Java glue right after
/// `dispatch_async`) keeps the handle and hands it back when the Java
/// side polls for completion.
///
/// Warns — once per doubling past [`SUBMISSION_WARN_THRESHOLD`], so the
/// log cannot itself become the flood — when the number of live
/// submissions suggests the caller is not draining. Deliberately a
/// warning and not a hard cap: refusing a dispatch would turn a
/// recoverable leak into a failed kernel for a caller whose pipeline is
/// merely deep, and the VM has no way to tell those apart. The warning
/// names the obligation and the two calls that discharge it, which is
/// what was missing when this cost a machine.
#[cfg(feature = "gpu-offload")]
pub fn register_submission(sub: std::sync::Arc<StreamSubmission>) -> u64 {
    let h = sub.handle;
    let live = {
        let mut table = submissions().write();
        table.insert(h, sub);
        table.len()
    };
    if live >= SUBMISSION_WARN_THRESHOLD && live.is_power_of_two() {
        tracing::warn!(
            live_submissions = live,
            "gpu offload: {live} submissions are alive and un-finalized. Each one \
             pins host writeback buffers, device buffers and a GC-critical token, \
             and the collector does not run while any such token is alive — so \
             this grows until the host runs out of memory, outside the Java heap \
             and beyond what -Xmx bounds. Drain each handle with \
             GpuExecutor.awaitSubmission(h) then releaseSubmission(h), or call \
             GpuFuture.get(); a fire-and-forget chain must be bounded."
        );
    }
    h
}

/// Number of submissions currently registered and not yet released.
///
/// Exposed so a test can assert that a dispatch path drains what it
/// creates, rather than inferring it from memory use.
#[cfg(feature = "gpu-offload")]
pub fn live_submission_count() -> usize {
    submissions().read().len()
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

// ── Known-issues followups #3: spontaneous completion reaper ─────────
//
// The missing half of the async completion model: until now, nothing
// drove a submission to `Completed`/`Failed` except a Java thread
// calling `get()` / `isDone()` / `getNow()`. This background thread
// is what the `cuLaunchHostFunc` callback registered in
// `dispatch_async` wakes up — it runs `finalize_submission` itself,
// off any Java thread, as soon as the device reports a kernel done.
//
// Modelled directly on `jit::tiered::ensure_background_compiler` /
// `BACKGROUND_COMPILER` (`jit/src/tiered.rs`): a single process-wide
// worker, started at most once via `Once`, parked in a static so it
// isn't dropped. And on `interpreter.rs`'s
// `ensure_bg_compiler_started`/`background_compile_task`: the worker
// captures a `Weak<SharedVm>` (never an `Arc`, so it cannot keep the
// VM alive past teardown) and no-ops when `upgrade()` fails.
//
// Deliberately NOT registered with the thread registry / STW barrier
// — same reasoning as the background JIT compiler: this thread never
// holds a managed `ObjectRef` across a GC point on its own native
// stack. Its only heap touch is the bounded call into
// `finalize_submission`, which already uses the cross-thread-safe
// `GcCriticalGuard`/`GPU_CRITICAL_COUNT` gate built specifically so
// finalization "may run on a different thread from dispatch" (see
// `FinalizeState`'s doc comment above).

// Flat top-level statics rather than one struct behind a single
// `Mutex` — deliberately. An earlier version of this design put
// `queue`/`wake`/`shutdown` behind one outer `Mutex<Option<..>>` that
// `completion_reaper_loop` held for the *entire* condvar wait
// (`parking_lot::Condvar::wait` only releases the *inner* lock it's
// given, not any other lock the caller happens to be holding). That
// deadlocks `enqueue_completion` forever against a parked reaper: the
// reaper holds the outer lock while parked, so the enqueuer's own
// attempt to take that same outer lock never succeeds, so the
// `notify_one` that would wake the reaper never runs. Independent
// statics sidestep the issue entirely — `REAPER_QUEUE`'s mutex is the
// only lock the condvar wait ever touches.
//
// The queue holds a `(handle, weak_vm, submission)` triple per entry,
// NOT a bare handle — an earlier version captured a single
// `Weak<SharedVm>` once, at thread-spawn time, and reused it for every
// drained handle for the rest of the process's life. That is correct
// for a single long-lived production VM (matching `SUBMISSIONS`/
// `NEXT_SUBMISSION_HANDLE`, which really are process-global-for-life
// data), but it is WRONG the moment more than one `SharedVm` exists
// over the process's lifetime — e.g. an integration-test binary that
// constructs and drops a fresh `Vm::new()` per test. Real-hardware
// validation caught this: with several such tests running in the same
// process, the reaper's captured `weak_vm` belonged to whichever
// test's dispatch happened to start the reaper thread first; once
// *that* test's `Vm` was dropped, `upgrade()` failed forever after —
// silently stranding every later test's (otherwise perfectly valid)
// submissions in `Running`. Carrying the correct `Weak<SharedVm>`
// alongside each queued handle instead of pinning one to the thread's
// whole lifetime fixes this for any number of concurrent or
// sequential `SharedVm` instances.
//
// AUDIT 2026-08-03: the third element, `Arc<StreamSubmission>`, is the
// fix for a real hardware bug, not an optimization. The host callback
// in `dispatch_async` used to clone `submission` for its own use and
// let that clone drop locally at the end of the closure. For a
// synchronous (non-Future-API) dispatch — e.g. the transparent `--gpu`
// path for a reduction kernel like `GpuDotBench.dotReduce`, which never
// calls `register_submission` — that clone reliably ends up being the
// *last* strong reference by the time the driver fires the callback
// (the synchronous caller has already read its result and moved on).
// Dropping the last `Arc<StreamSubmission>` drops the `cuda_bridge`
// `Stream` inside it, whose `Drop` (via cudarc's `CudaStream::drop`)
// issues a real CUDA driver call — forbidden from inside a
// `cuLaunchHostFunc` callback, and confirmed on hardware (RTX 2060) to
// panic with `DriverError(CUDA_ERROR_NOT_PERMITTED, "operation not
// permitted")` on literally every such dispatch. Carrying the `Arc`
// through this queue instead moves that potential final drop onto the
// reaper thread — an ordinary thread, where CUDA driver calls are
// allowed — while still preserving the original "keep the submission
// (and any device buffers referenced by its pending `FinalizeState`)
// alive until finalization" intent.
#[cfg(feature = "gpu-offload")]
static REAPER_QUEUE: parking_lot::Mutex<
    std::collections::VecDeque<(
        u64,
        std::sync::Weak<crate::vm::SharedVm>,
        std::sync::Arc<StreamSubmission>,
    )>,
> = parking_lot::Mutex::new(std::collections::VecDeque::new());
#[cfg(feature = "gpu-offload")]
static REAPER_WAKE: parking_lot::Condvar = parking_lot::Condvar::new();
#[cfg(feature = "gpu-offload")]
static REAPER_SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "gpu-offload")]
static REAPER_HANDLE: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>> =
    parking_lot::Mutex::new(None);
#[cfg(feature = "gpu-offload")]
static COMPLETION_REAPER_INIT: std::sync::Once = std::sync::Once::new();

/// Idempotently start the GPU completion reaper thread. Safe to call
/// on every `dispatch_async` — the spawn happens at most once
/// (guarded by [`Once`](std::sync::Once)). Takes no `SharedVm`
/// reference: the worker is VM-agnostic, since each queued handle
/// carries its own `Weak<SharedVm>` (see the doc comment on
/// `REAPER_QUEUE` for why that matters).
#[cfg(feature = "gpu-offload")]
fn ensure_completion_reaper_started() {
    COMPLETION_REAPER_INIT.call_once(|| {
        match std::thread::Builder::new()
            .name("cratonvm-gpu-completion-reaper".to_string())
            .spawn(completion_reaper_loop)
        {
            Ok(h) => *REAPER_HANDLE.lock() = Some(h),
            Err(e) => {
                tracing::debug!(
                    "gpu offload: failed to spawn completion reaper thread ({e}); \
                     falling back to poll-only completion",
                );
            }
        }
    });
}

/// Body of the completion reaper thread. Blocks on `REAPER_WAKE` (no
/// busy-polling); for each drained `(handle, weak_vm, submission)`
/// triple, upgrades that entry's own `weak_vm` and runs the same
/// [`finalize_submission`] work `get()` would have — off the mutator,
/// with no Java thread involved. `finalize_submission`'s own errors are
/// already recorded on `StreamSubmission::status`; there is nothing
/// further to do with its `Result` here.
///
/// `submission` is kept bound (not `let _ = ...`'d away) for the
/// duration of `finalize_enqueued_handle` and only drops when the loop
/// moves on to the next iteration. If this is the last strong
/// `Arc<StreamSubmission>` in the process, that drop — and the
/// `cuda_bridge::Stream` / cudarc `CudaStream` teardown it can
/// transitively trigger — happens right here, on this ordinary thread.
/// That is the point: see `REAPER_QUEUE`'s doc comment for why it must
/// NOT happen back in the `cuLaunchHostFunc` callback that produced
/// this entry.
#[cfg(feature = "gpu-offload")]
fn completion_reaper_loop() {
    loop {
        let item = {
            let mut queue = REAPER_QUEUE.lock();
            loop {
                if let Some(item) = queue.pop_front() {
                    break Some(item);
                }
                if REAPER_SHUTDOWN.load(std::sync::atomic::Ordering::Acquire) {
                    break None;
                }
                // `Condvar::wait` atomically releases `queue` while
                // parked and re-acquires it on wake, so a
                // `notify_one` from `enqueue_completion` landing
                // between the empty-check above and this wait can
                // never be missed once we're inside `wait`.
                REAPER_WAKE.wait(&mut queue);
            }
        };
        let (handle, weak_vm, _submission_keepalive) = match item {
            Some(item) => item,
            None => return,
        };
        finalize_enqueued_handle(&weak_vm, handle);
        // `_submission_keepalive` drops here — safe on this thread even
        // if it is the last `Arc<StreamSubmission>`.
    }
}

/// Attempt to finalize one reaper-queued handle: upgrade `weak_vm`
/// and, if that succeeds and the handle is still registered, run
/// [`finalize_submission`]. A failed upgrade (VM torn down, or
/// `self_arc` was never populated) is a silent no-op — the
/// submission is simply left for the existing poll-based path
/// (`poll_submission_status`, `get()`) to finalize later, exactly as
/// it always has. Keeping the caller (`completion_reaper_loop`)
/// draining rather than exiting on a failed upgrade avoids leaking
/// the thread's role as the queue's sole consumer for as long as the
/// process runs multiple short-lived VMs (tests).
///
/// Factored out of `completion_reaper_loop`'s body so both the
/// VM-alive and VM-torn-down paths are unit-testable directly,
/// without spawning a thread or touching the process-global reaper
/// statics (which, being `Once`-guarded singletons, can only be
/// exercised by one test in the whole binary — see
/// `reaper_finalizes_submission_without_any_poll_call`).
#[cfg(feature = "gpu-offload")]
fn finalize_enqueued_handle(weak_vm: &std::sync::Weak<crate::vm::SharedVm>, handle: u64) {
    let Some(shared) = weak_vm.upgrade() else {
        return;
    };
    if let Some(sub) = lookup_submission(handle) {
        let _ = finalize_submission(&shared, &sub);
    }
}

/// Push `(handle, weak_vm, submission)` onto the reaper's work queue
/// and wake it. `weak_vm` travels with the handle rather than being
/// fixed once for the reaper thread's whole life — see the doc comment
/// on `REAPER_QUEUE` for why that distinction matters. Called from the
/// `cuLaunchHostFunc` callback registered in `dispatch_async`, so this
/// must stay cheap and must never call back into the CUDA driver: a
/// `parking_lot::Mutex` lock + `VecDeque` push + `Condvar::notify_one`
/// is the same class of "plain host memory operation" the callback
/// already performs for `device_done`. A push before the reaper thread
/// exists yet (a caller that races `ensure_completion_reaper_started`'s
/// spawn) is harmless — the entry just sits in the queue until the
/// worker starts draining it.
///
/// `submission` is *moved* in, not cloned — the caller (the host
/// callback) must give up its own `Arc<StreamSubmission>` here rather
/// than holding onto it and letting it drop locally. `VecDeque::push_back`
/// only moves bytes around; it never drops the value being inserted, so
/// transferring ownership this way adds no CUDA-driver-call risk to
/// this function itself. See `REAPER_QUEUE`'s doc comment for why the
/// eventual drop needs to happen on the reaper thread instead of here.
#[cfg(feature = "gpu-offload")]
fn enqueue_completion(
    handle: u64,
    weak_vm: std::sync::Weak<crate::vm::SharedVm>,
    submission: std::sync::Arc<StreamSubmission>,
) {
    REAPER_QUEUE.lock().push_back((handle, weak_vm, submission));
    REAPER_WAKE.notify_one();
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

/// Classify a `cuda_bridge::DeviceError` for the Java side.
///
/// The variant carries most of the answer: a module that would not load
/// or an entry point that is not in it are compilation problems, and
/// everything else happened against a live device. The one thing the
/// variant does not distinguish is exhaustion, so the driver's own
/// message is consulted for that — at the point the driver produced it,
/// where the wording is CUDA's canonical text, rather than on the Java
/// side after it has been reformatted into a sentence.
#[cfg(feature = "gpu-offload")]
fn kind_of_device_error(e: &cuda_bridge::DeviceError) -> GpuErrorKind {
    use cuda_bridge::DeviceError;
    match e {
        DeviceError::Load(_) | DeviceError::KernelNotFound(_) => GpuErrorKind::Compile,
        DeviceError::NoDriver => GpuErrorKind::Launch,
        DeviceError::Driver(m) | DeviceError::Launch(m) | DeviceError::Memcpy(m) => {
            kind_of_device_message(m)
        }
    }
}

/// Same classification for a device failure that has already been
/// flattened to a `String` — the marshaller's upload helpers return
/// `Result<_, String>`, so the `DeviceError` is gone by the time the
/// dispatch path sees it.
///
/// Anything that is not recognisably exhaustion is `Launch`: reaching
/// these call sites means the kernel resolved and compiled, so a
/// compilation category would be wrong.
#[cfg(feature = "gpu-offload")]
fn kind_of_device_message(m: &str) -> GpuErrorKind {
    let lower = m.to_ascii_lowercase();
    if lower.contains("out_of_memory")
        || lower.contains("out of memory")
        || lower.contains("outofmemory")
    {
        GpuErrorKind::OutOfMemory
    } else {
        GpuErrorKind::Launch
    }
}

#[cfg(feature = "gpu-offload")]
fn record_failed_submission(
    stream: Option<std::sync::Arc<Stream>>,
    kind: GpuErrorKind,
    message: String,
) -> u64 {
    // Every explicit-dispatch failure ends here, and until this line
    // existed none of them said anything: the reason was stored on the
    // submission and only ever surfaced if the Java side successfully
    // called back for it. `--print-gpu-decisions` turns this on along
    // with the analyzer verdicts.
    tracing::warn!("gpu offload: submission failed — {message}");
    let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sub = std::sync::Arc::new(StreamSubmission {
        handle,
        stream,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Failed { message, kind }),
        finalize: parking_lot::Mutex::new(None),
        device_done: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(sub);
    handle
}

/// Convenience wrapper around
/// [`dispatch_method_from_native_on_stream`] with `stream_handle =
/// None` — a fresh, private, one-shot stream for this dispatch
/// alone. Kept byte-for-byte source-compatible (same name, same
/// 5-arg signature) so callers that don't care about stream affinity
/// — the transparent `try_dispatch` interpreter hook, existing
/// integration tests — need no changes.
#[cfg(feature = "gpu-offload")]
pub fn dispatch_method_from_native(
    shared: &crate::vm::SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[cratonvm_types::Value],
) -> u64 {
    dispatch_method_from_native_on_stream(
        shared,
        class_name,
        method_name,
        descriptor,
        java_args,
        None,
    )
}

/// Dispatch `class_name`.`method_name`(`descriptor`) on the GPU with
/// `java_args`, optionally pinned to a previously-registered CUDA
/// stream. Returns the submission handle the Java layer wraps in
/// `GpuFutureImpl`. On any failure (method not eligible, missing
/// device, marshal error, launch error, unknown/released
/// `stream_handle`) the returned handle still resolves — it points
/// to a `StreamSubmission` whose status is `Failed { message }` so
/// the Java side surfaces it as `GpuException` via
/// `futureGetErrorMessage`.
///
/// `stream_handle`:
///   * `Some(h)` — `h` must be a live handle from a prior
///     [`OffloadCache::stream_create`] call (an executor's lazily
///     created default stream, or an explicit `GpuStream` — see
///     `native-builtins/src/craton_gpu.rs`). Resolved via
///     [`OffloadCache::resolve_stream`] and dispatched onto that
///     exact stream; an unknown or already-released handle is a hard
///     `Failed` submission, never a silent fresh-stream fallback —
///     see [`OffloadCache::stream_create`]'s doc comment for the
///     ordering guarantee this buys.
///   * `None` — unchanged pre-existing behavior: a fresh, private,
///     one-shot `cuda_bridge::Stream` is created for this dispatch
///     alone. This is what [`dispatch_method_from_native`] (this
///     function's 5-arg convenience wrapper) always passes, and what
///     the transparent `try_dispatch` interpreter hook uses — an
///     `invokestatic` call site never had a Java-visible stream
///     handle to give it in the first place, so its behavior is
///     intentionally unaffected by this parameter's addition.
#[cfg(feature = "gpu-offload")]
pub fn dispatch_method_from_native_on_stream(
    shared: &crate::vm::SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[cratonvm_types::Value],
    stream_handle: Option<u64>,
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
    //
    // Part E — also extract `return_kind` so the marshaller below
    // knows whether the compiled PTX declares a trailing `ret_ptr`
    // param (any non-void, non-array return) that needs a matching
    // accumulator buffer pushed ahead of `failure_flag`.
    let timed = cratonvm_native_builtins::craton_gpu::dispatch_timing::enabled();
    let mut mark = std::time::Instant::now();
    let (
        class_id,
        method_index,
        is_static,
        this_field_names,
        writes_param_mask,
        return_kind,
        work_bound,
    ): (
        crate::classloading::ClassId,
        u16,
        bool,
        Vec<String>,
        u64,
        ParamKind,
        jit_cuda::emitter::WorkBound,
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
                GpuErrorKind::Compile,
                format!("submitMethod: load class failed for {class_name}: {e:?}"),
            );
        }
        let cm = shared.classes.class_manager.read();
        let class_id = match cm.get_loaded_class_id(class_name) {
            Some(id) => id,
            None => {
                return record_failed_submission(
                    None,
                    GpuErrorKind::Compile,
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
                    GpuErrorKind::Compile,
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
                    GpuErrorKind::Compile,
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
                            GpuErrorKind::Compile,
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
                            GpuErrorKind::Compile,
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
                    compiled.signature.return_kind,
                    compiled.signature.work_bound,
                )
            }
            LookupOutcome::Skip => {
                return record_failed_submission(
                    None,
                GpuErrorKind::Compile,
                    format!(
                        "submitMethod: method not offloadable (Skip): {class_name}.{method_name}{descriptor}",
                    ),);
            }
            LookupOutcome::Blacklisted => {
                return record_failed_submission(
                    None,
                    GpuErrorKind::Compile,
                    format!(
                        "submitMethod: method blacklisted: {class_name}.{method_name}{descriptor}",
                    ),
                );
            }
        }
    };

    if timed {
        cratonvm_native_builtins::craton_gpu::dispatch_timing::add(
            3,
            mark.elapsed().as_nanos() as u64,
        );
        mark = std::time::Instant::now();
    }

    // 4. From here on we need a real device context. The Failed-fast
    //    path is identical to dispatch_async's no-device branch.
    let ctx = match cache.device() {
        Some(c) => c,
        None => {
            return record_failed_submission(
                None,
                GpuErrorKind::Launch,
                format!(
                    "submitMethod: no CUDA device available ({class_name}.{method_name}{descriptor})",
                ),);
        }
    };

    // 5. Resolve the stream to dispatch on.
    //
    //    `Some(h)` pins this dispatch onto a previously
    //    `OffloadCache::stream_create`-minted stream (today: an
    //    executor's lazily-created default stream, or an explicit
    //    `GpuStream` from `Native.newStream` — see
    //    `native-builtins/src/craton_gpu.rs`). We resolve it through
    //    the cache rather than trusting a raw handle the caller might
    //    supply, so a released handle (`stream_release` already ran)
    //    fails loud instead of silently dispatching onto a stream
    //    Java asked us to forget.
    //
    //    `None` keeps the original behavior: a fresh, private,
    //    one-shot stream per dispatch — still the fallback for
    //    handle-less callers, including the transparent interpreter
    //    (`try_dispatch`) path via `dispatch_method_from_native`.
    //
    //    Two dispatches resolved onto the SAME registered stream run
    //    in the order they were launched on it — a stock CUDA-stream
    //    property, not something this function implements itself —
    //    and it composes with (does not replace) the existing
    //    per-buffer last-write-event choreography the device-residency
    //    cache already does across streams; see `stream_create`'s doc
    //    comment for the full contract.
    let stream: Arc<Stream> = match stream_handle {
        Some(h) => match cache.resolve_stream(h) {
            Some(s) => s,
            None => {
                return record_failed_submission(
                    None,
                    GpuErrorKind::Launch,
                    format!(
                        "submitMethod: unknown or released stream handle {h} \
                         ({class_name}.{method_name}{descriptor})",
                    ),
                );
            }
        },
        None => match CudaStream::new(ctx) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                return record_failed_submission(
                    None,
                    kind_of_device_error(&e),
                    format!("submitMethod: Stream::new failed: {e}"),
                );
            }
        },
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
    let token = shared.mem.heap.enter_gpu_critical();

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
    // Per-declared-parameter array lengths, so `work_bound` can name
    // the one that is the loop trip count. Only the static path
    // populates this (the `pthis_*` prelude has no declared index),
    // which is also the only path `WorkBound::ParamLen` is derived on.
    let mut param_lens: Vec<Option<usize>> = Vec::new();
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
                GpuErrorKind::Compile,
                    format!(
                        "submitMethod: non-static receiver is null ({class_name}.{method_name}{descriptor})",
                    ),);
            }
            Some(other) => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    GpuErrorKind::Compile,
                    format!(
                        "submitMethod: non-static receiver is not an object reference: {other:?}",
                    ),
                );
            }
            None => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    GpuErrorKind::Compile,
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
        let receiver_class_id = shared.mem.heap.class_id_of(receiver);
        for (i, field_name) in this_field_names.iter().enumerate() {
            // Walk the class hierarchy starting at the receiver's
            // concrete class — the field may be declared on the
            // declared class (which equals receiver_class_id when the
            // receiver is exactly that class) or on a superclass.
            let slot: usize = {
                let cm = shared.classes.class_manager.read();
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
                            GpuErrorKind::Compile,
                            format!(
                                "submitMethod: this_field `{field_name}` not found on receiver's \
                                 class hierarchy (receiver class_id={receiver_class_id:?})",
                            ),
                        );
                    }
                }
            };
            let field_val = shared.mem.heap.get_field(receiver, slot);
            let field_obj = match field_val {
                Value::Object(Some(o)) => o,
                Value::Object(None) => {
                    drop(token);
                    return record_failed_submission(
                        Some(stream.clone()),
                GpuErrorKind::Compile,
                        format!(
                            "submitMethod: this_field `{field_name}` (pthis_{i}) is null on receiver",
                        ),);
                }
                other => {
                    drop(token);
                    return record_failed_submission(
                        Some(stream.clone()),
                        GpuErrorKind::Compile,
                        format!(
                            "submitMethod: this_field `{field_name}` (pthis_{i}) is not an \
                             object reference: {other:?}",
                        ),
                    );
                }
            };
            let Some(etype) = shared.mem.heap.array_element_type(field_obj) else {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    GpuErrorKind::Compile,
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
            let pthis_len = shared.mem.heap.array_length(field_obj);
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
                    return record_failed_submission(
                        Some(stream.clone()),
                        kind_of_device_message(&msg),
                        msg,
                    );
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
                if let Some(element_type) = shared.mem.heap.array_element_type(*obj_ref) {
                    // Phase 10 #2 — `i` is the index inside
                    // `java_args_to_marshal`, which lines up 1:1 with
                    // the analyzer's `param_kinds[i]` for static
                    // methods (the only path where `writes_param_mask`
                    // is precisely populated today). Consult the mask
                    // — a clear bit means the kernel never `*astore`s
                    // into this param, so we can safely skip the
                    // post-launch D→H copy.
                    let is_written = (writes_param_mask >> i) & 1 == 1;
                    let arr_len = shared.mem.heap.array_length(*obj_ref);
                    match marshal_array_arg(shared, ctx, *obj_ref, element_type, is_written, &token)
                    {
                        Ok((args_after, wb_opt, bytes)) => {
                            kernel_args = args_after(kernel_args);
                            h2d_bytes = h2d_bytes.saturating_add(bytes);
                            if arr_len > max_array_len {
                                max_array_len = arr_len;
                            }
                            record_param_len(&mut param_lens, i, arr_len);
                            if let Some(wb) = wb_opt {
                                writebacks.push(wb);
                            }
                        }
                        Err(msg) => {
                            drop(token);
                            return record_failed_submission(
                                Some(stream.clone()),
                                kind_of_device_message(&msg),
                                msg,
                            );
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
                if let Some((etype, len, arr_handle)) = try_gpu_array_shape(shared, *obj_ref) {
                    match marshal_resident_array_arg(ctx, etype, len, arr_handle) {
                        Ok((args_after, wb)) => {
                            kernel_args = args_after(kernel_args);
                            if let Some(l) = wb.array_len() {
                                if l > max_array_len {
                                    max_array_len = l;
                                }
                                record_param_len(&mut param_lens, i, l);
                            }
                            writebacks.push(wb);
                        }
                        Err(msg) => {
                            drop(token);
                            return record_failed_submission(
                                Some(stream.clone()),
                                kind_of_device_message(&msg),
                                msg,
                            );
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
                GpuErrorKind::Compile,
                            format!(
                                "submitMethod: arg #{i} is not a primitive array, GpuArray, or boxed primitive",
                            ),);
                    }
                }
                continue;
            }
            Value::Object(None) => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    GpuErrorKind::Compile,
                    format!("submitMethod: arg #{i} is null"),
                );
            }
            _ => {
                drop(token);
                return record_failed_submission(
                    Some(stream.clone()),
                    GpuErrorKind::Compile,
                    format!("submitMethod: arg #{i} type unsupported: {arg:?}"),
                );
            }
        }
    }

    // 7c. Part E — if the compiled kernel declares a scalar return,
    //     `build_param_list` (`jit-cuda/src/lowering.rs`) emits a
    //     trailing `ret_ptr: u64*` param ahead of `failure_flag` (see
    //     7d below) for any `ParamKind::I32/I64/F32/F64` return kind —
    //     regardless of whether the analyzer proved a reduction.
    //     Whether the kernel body treats it as an atomic accumulator
    //     (`atom.global.add`, proven reduction — see
    //     `KernelSignature::is_reduction`) or a plain racing overwrite
    //     (straight-line scalar return, every thread computes the same
    //     value) is baked into the compiled PTX by
    //     `jit_cuda::lowering::emit::scalar_return`; the host only
    //     needs to allocate+push the slot, so this arm is unconditional
    //     on `return_kind` and doesn't need to re-derive `is_reduction`
    //     here.
    //
    //     MUST be zero-initialized: for the reduction case the atomic
    //     add accumulates onto whatever is already in the cell, and
    //     `0` is the identity element that matches Java's implicit
    //     accumulator initializer (`int`/`long`/`float`/`double sum =
    //     0`) — `KernelSignature::is_reduction`'s doc comment states
    //     this contract explicitly ("The host marshaller MUST pre-zero
    //     `*ret_ptr` before launch"). For the straight-line case the
    //     zero is simply clobbered by the plain store, so pre-zeroing
    //     is harmless there too. Without a device pointer in this slot
    //     at all, `cuLaunchKernel` would fail exactly as the
    //     `failure_flag` omission described below does — one fewer arg
    //     than the compiled kernel declares.
    macro_rules! push_scalar_return_buf {
        ($ty:ty, $variant:ident, $tag:literal) => {{
            let buf = match cuda_bridge::DeviceBuffer::<$ty>::zeros(ctx, 1) {
                Ok(b) => std::sync::Arc::new(b),
                Err(e) => {
                    drop(token);
                    return record_failed_submission(
                        Some(stream.clone()),
                        kind_of_device_error(&e),
                        format!(
                            "submitMethod: failed to allocate scalar-return ({}) buffer: {e}",
                            $tag,
                        ),
                    );
                }
            };
            kernel_args = kernel_args.push_device_ptr(buf.as_ref());
            writebacks.push(MarshalWriteback::$variant { buf });
        }};
    }
    match return_kind {
        ParamKind::I32 => push_scalar_return_buf!(i32, ScalarI32, "i32"),
        ParamKind::I64 => push_scalar_return_buf!(i64, ScalarI64, "i64"),
        ParamKind::F32 => push_scalar_return_buf!(f32, ScalarF32, "f32"),
        ParamKind::F64 => push_scalar_return_buf!(f64, ScalarF64, "f64"),
        ParamKind::Void
        | ParamKind::I32Array
        | ParamKind::I64Array
        | ParamKind::F32Array
        | ParamKind::F64Array
        | ParamKind::I16Array
        | ParamKind::I8Array => {
            // Void: no ret_ptr param at all. Array returns: not yet
            // wired (see `SerializedResult`'s doc comment) — the
            // analyzer/lowering pairing for those shapes is a later
            // round's problem, unchanged by Part E.
        }
    }

    // 7d. Append the kernel's trailing `failure_flag` parameter.
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
    //     Pooled. A fresh `cuMemAlloc` per dispatch is the single
    //     most expensive thing on this path — measured at 117 us for a
    //     kernel that does nothing, against 5.5 ms of ACTUAL device
    //     work for a whole 453-kernel inference step. The buffer is
    //     one `u64`; allocating it fresh every time bought nothing.
    if timed {
        cratonvm_native_builtins::craton_gpu::dispatch_timing::add(
            4,
            mark.elapsed().as_nanos() as u64,
        );
        mark = std::time::Instant::now();
    }
    let pool_key = ctx as *const cuda_bridge::DeviceContext as usize;
    let failure_flag_buf = match flag_pool::take(pool_key, ctx) {
        Ok(b) => b,
        Err(e) => {
            drop(token);
            return record_failed_submission(
                Some(stream.clone()),
                GpuErrorKind::Compile,
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
        pool_key,
    });
    // `tid_base` — the index of the first element this launch covers.
    //
    // Every lowered kernel takes it (see `lowering::ptx_params`), and a
    // whole-array launch is base 0. A CHUNKED launch, which is what lets
    // one chunk's writeback overlap with the next chunk's kernel, passes
    // that chunk's first index instead. CUDA has no launch offset of its
    // own, so this parameter is the only way one kernel can cover disjoint
    // slices of an iteration space while every thread still computes its
    // global index.
    kernel_args = kernel_args.push_i32(0);

    // 8. Phase 7 #1 — dispatch on the stream. The launch grid's
    //    element count comes from `max_array_len` (0 for a scalar-only
    //    kernel with no array args, or a truncated 2^31-1 — an array
    //    bigger than u32::MAX is impossible in the JVM but defensive
    //    regardless); `dispatch_async` uses this `runtime_work` value
    //    outright when nonzero and only falls back to the analyzer's
    //    `estimated_work` placeholder for the `0` (scalar-only)
    //    sentinel — see the launch-config fix in `dispatch_async`'s
    //    doc comment.
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
    // Size the grid to the loop, not to the biggest array. For an
    // element-wise kernel these are the same number. For a kernel whose
    // inputs are larger than its iteration space — a matrix-vector
    // product is the canonical one — they differ by the matrix's inner
    // dimension, and the largest-array rule launches that many times too
    // many threads. Each extra thread does nothing but reach the guard
    // and exit, but the driver still creates it.
    //
    // `Unknown` (anything that is not a single counted loop) and a
    // parameter whose length we never recorded both fall back to the
    // old rule, which is over-provisioning and therefore always safe.
    let work_items: usize = match work_bound {
        jit_cuda::emitter::WorkBound::ParamLen(idx) => param_lens
            .get(idx as usize)
            .copied()
            .flatten()
            .unwrap_or(max_array_len),
        jit_cuda::emitter::WorkBound::Literal(v) if v > 0 => v as usize,
        _ => max_array_len,
    };
    let runtime_work: u32 = u32::try_from(work_items).unwrap_or(u32::MAX);
    // The thread-local SafepointToken's role is over (marshal is
    // done). The cross-thread GcCriticalGuard (moved into
    // `FinalizeState` below) takes over.
    drop(token);
    // 9. Known-issues followups #3 — the writebacks + GC-critical
    //    guard are handed to `dispatch_async` itself now, rather than
    //    attached by this caller after the call returns: attaching
    //    them here (post-return) raced the completion reaper's host
    //    callback, which can fire before this function resumes
    //    (guaranteed in stub mode, possible in principle on real
    //    hardware for a fast-completing kernel). `dispatch_async`
    //    attaches `finalize_state` to the submission before
    //    registering that callback, closing the race. If the dispatch
    //    itself fails (no device, kernel not in cache, launch error),
    //    `dispatch_async` simply drops the `FinalizeState` it was
    //    handed — no kernel ran, nothing to finalize — same as this
    //    caller used to do explicitly in the `needs_finalize == false`
    //    case.
    if timed {
        cratonvm_native_builtins::craton_gpu::dispatch_timing::add(
            5,
            mark.elapsed().as_nanos() as u64,
        );
        mark = std::time::Instant::now();
    }
    let submission = cache.dispatch_async(
        stream.clone(),
        class_id,
        method_index,
        kernel_args,
        runtime_work,
        Some(FinalizeState {
            writebacks,
            _gc_critical: gc_guard,
        }),
        shared.self_arc.read().as_ref().cloned().unwrap_or_default(),
    );

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
    // Take the FinalizeState, and HOLD ITS LOCK for the whole
    // finalization. If None, finalization has already run (or this
    // submission was Failed at dispatch) — fall through to read the
    // terminal status.
    //
    // The lock has to span the work, not just the take. Two callers
    // reach here for the same submission — `future.get()` on the Java
    // thread and the completion reaper woken by the device callback —
    // and taking-then-releasing let the second one observe the gap:
    // `FinalizeState` already gone, status not yet stamped, i.e.
    // `Running` with nothing left to finalize. That is not a logic
    // error, it is a race, and it surfaced as
    //
    //     GpuException: submission handle=35 is Running with no
    //     FinalizeState
    //
    // on roughly a third of a 5-kernel sequence — the kind of flake
    // that reads like a bad kernel. Holding the lock makes the second
    // caller wait for the first and then read a terminal status, which
    // is what it was always assumed to do.
    let mut finalize_guard = submission.finalize.lock();
    let pending = finalize_guard.take();

    if let Some(FinalizeState {
        writebacks,
        _gc_critical,
    }) = pending
    {
        // 1. Wait for the kernel to complete via the recorded event.
        //
        //    Skipped for a chunked submission: its writeback waits each
        //    chunk event in turn and copies that chunk out, so the host
        //    memcpy runs under the GPU work still outstanding behind it.
        //    Waiting here first would drain the whole pipeline and throw
        //    that overlap away -- the entire point of chunking.
        let is_chunked = writebacks
            .iter()
            .any(|wb| matches!(wb, MarshalWriteback::Chunked { .. }));
        if let Some(event) = submission.event.as_ref().filter(|_| !is_chunked) {
            if let Err(e) = event.synchronize() {
                let mut status = submission.status.lock();
                *status = SubmissionStatus::Failed {
                    message: format!("event.synchronize: {e}"),
                    kind: kind_of_device_error(&e),
                };
                // _gc_critical drops here (releases GC gate).
                drop(writebacks);
                drop(_gc_critical);
                return Err(match &*status {
                    SubmissionStatus::Failed { message, .. } => message.clone(),
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
        // Part E — a scalar-return kernel's `ScalarI32`/`ScalarI64`/
        // `ScalarF32`/`ScalarF64` writeback is the only kind that
        // produces a value; every other writeback below returns
        // `Ok(None)`. A submission has at most one `ret_ptr`
        // (`build_param_list` emits it once, right before
        // `failure_flag`), so at most one iteration of this loop ever
        // sets `scalar_result` — a plain "last write wins" is
        // sufficient and avoids an extra `is_some()` guard.
        let mut first_err: Option<String> = None;
        let mut scalar_result: Option<SerializedResult> = None;
        // Order: Chunked, then FailureFlag, then everything else.
        //
        // A Chunked writeback goes FIRST, ahead of the flag, and that is
        // deliberate -- its per-chunk waits are what overlap the host copy
        // with the GPU work still in flight, and draining the flag first
        // would require the whole pipeline to finish before any of it.
        // The consequence is that a chunked array can reach the Java heap
        // before a bounds failure is known, which is why chunking is only
        // ever planned for an array the kernel writes and never reads --
        // see `take_chunkable_writeback` for why that makes the partial
        // commit unobservable.
        //
        // Every other array writeback still drains AFTER the flag, so the
        // "interpreter observes no partial GPU state" guarantee holds
        // unchanged for them.
        let is_chunk = |wb: &&MarshalWriteback| matches!(wb, MarshalWriteback::Chunked { .. });
        let is_flag = |wb: &&MarshalWriteback| matches!(wb, MarshalWriteback::FailureFlag { .. });
        for wb in writebacks
            .iter()
            .filter(is_chunk)
            .chain(writebacks.iter().filter(is_flag))
            .chain(writebacks.iter().filter(|wb| !is_chunk(wb) && !is_flag(wb)))
        {
            match wb.writeback(shared, &local_token) {
                Ok(Some(result)) => scalar_result = Some(result),
                Ok(None) => {}
                Err(msg) => {
                    first_err = Some(msg);
                    break;
                }
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
                    result: scalar_result.unwrap_or(SerializedResult::Void),
                };
                Ok(())
            }
            (SubmissionStatus::Running, Some(msg)) => {
                *status = SubmissionStatus::Failed {
                    message: msg.clone(),
                    // Reached only from the writeback drain below, i.e.
                    // after the kernel itself launched: a device-side
                    // failure, not a compilation one.
                    kind: GpuErrorKind::Launch,
                };
                Err(msg)
            }
            // Submission was already terminal — keep whatever status
            // it had. (Shouldn't happen given we took the FinalizeState
            // under the same submission, but defensive.)
            (SubmissionStatus::Completed { .. }, _) => Ok(()),
            (SubmissionStatus::Failed { message, .. }, _) => Err(message.clone()),
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
            SubmissionStatus::Failed { message, .. } => Err(message.clone()),
        }
    }
}

/// Known-issues followups item 3 — the non-blocking half of
/// [`finalize_submission`]'s job.
///
/// Coarse terminal-vs-in-flight signal for a submission, without ever
/// calling `event.synchronize()`. This is what `Native.futureIsDone`
/// answers from: previously the only way to learn a submission's
/// status was `finalize_submission`, which blocks the calling thread
/// on the device until the kernel completes.
#[cfg(feature = "gpu-offload")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollOutcome {
    /// Dispatch accepted, kernel not observed complete yet. The
    /// caller should poll again later (or fall back to the blocking
    /// `get()` path via `finalize_submission` if it wants to wait).
    Running,
    /// The submission finished successfully. Callers still go through
    /// the usual `finalize_submission` / status-read path to pull the
    /// `SerializedResult` payload out — `PollOutcome` itself carries
    /// no data, only the coarse signal.
    Completed,
    /// The submission failed, either at dispatch time or during
    /// finalization (including a completion-query error surfaced by
    /// this very poll). The error text is on `StreamSubmission::status`.
    Failed,
}

/// Non-blocking poll of a submission's completion state.
///
/// Returns `None` for an unknown or already-`release_submission`d
/// handle — same "unknown handle" convention as [`lookup_submission`].
///
/// For a `Running` submission this never calls `event.synchronize()`.
/// It first checks `StreamSubmission::device_done` — set by the
/// best-effort host callback `dispatch_async` registers right after
/// the completion event — and only falls back to `Event::query` (also
/// non-blocking; it's the "has the event fired?" driver call, not the
/// "wait for it to fire" one) when that flag hasn't been observed set
/// yet. Either way, once the device side reports done, finalization
/// (writeback draining, GC-critical-guard release, status transition)
/// runs inline on the calling thread via [`finalize_submission`] — the
/// same work `get()` would have triggered, just invoked from a poll
/// instead of a blocking wait. That inline call does no device
/// waiting of its own (the event has already fired), so it's bounded
/// work, not a hidden block.
///
/// An `Event::query` error is treated the same way
/// `finalize_submission`'s own `event.synchronize()` failure is: the
/// submission is stamped `Failed` with the error text, and any
/// pending `FinalizeState` (writebacks, the GC-critical guard) is
/// dropped immediately rather than left to leak GC-critical count
/// forever with no future finalize call able to reach it.
#[cfg(feature = "gpu-offload")]
pub fn poll_submission_status(shared: &crate::vm::SharedVm, handle: u64) -> Option<PollOutcome> {
    let submission = lookup_submission(handle)?;

    // Already terminal — no need to touch the event or the driver.
    {
        let status = submission.status.lock();
        match &*status {
            SubmissionStatus::Completed { .. } => return Some(PollOutcome::Completed),
            SubmissionStatus::Failed { .. } => return Some(PollOutcome::Failed),
            SubmissionStatus::Running => {}
        }
    }

    // Fast path: the host callback registered at dispatch time may
    // have already flipped this without a driver round trip. Fall
    // back to `Event::query` when it hasn't (callback registration is
    // best-effort and can fail; the callback may also simply not have
    // run yet even though it will).
    let device_done = submission
        .device_done
        .load(std::sync::atomic::Ordering::Acquire);
    let query_result: cuda_bridge::Result<bool> = if device_done {
        Ok(true)
    } else {
        match submission.event.as_ref() {
            Some(event) => event.query(),
            // `Running` with no recorded event shouldn't happen — every
            // success path through `dispatch_async` records one before
            // returning a `Running` submission — but stay defensive
            // rather than panicking: no event to poll means we can't
            // claim more than "still running".
            None => Ok(false),
        }
    };

    match query_result {
        Ok(true) => {
            // Device-side work observed complete. Run the same
            // finalize path `get()` / `futureSynchronize` would have
            // run, inline on this (polling) thread.
            match finalize_submission(shared, &submission) {
                Ok(()) => Some(PollOutcome::Completed),
                Err(_) => Some(PollOutcome::Failed),
            }
        }
        Ok(false) => Some(PollOutcome::Running),
        Err(e) => {
            // Mirror `finalize_submission`'s `event.synchronize()`
            // error handling: drop the pending `FinalizeState` (which
            // releases the GC-critical guard and any device buffers)
            // and stamp the submission `Failed`, instead of leaving it
            // stuck `Running` with a `FinalizeState` nothing will ever
            // drain.
            let pending = submission.finalize.lock().take();
            {
                let mut status = submission.status.lock();
                if matches!(&*status, SubmissionStatus::Running) {
                    *status = SubmissionStatus::Failed {
                        message: format!("event.query: {e}"),
                        kind: kind_of_device_error(&e),
                    };
                }
            }
            drop(pending);
            Some(PollOutcome::Failed)
        }
    }
}

/// The recorded failure category for a submission, or `None` when the
/// handle is unknown or the submission did not fail.
///
/// Backs `NativeContext::gpu_future_error_kind`, and through it
/// `Native.futureErrorKind`. The category was stamped by whichever code
/// path actually failed, so this is a lookup rather than a guess.
#[cfg(feature = "gpu-offload")]
pub fn submission_error_kind(handle: u64) -> Option<GpuErrorKind> {
    let submission = lookup_submission(handle)?;
    let status = submission.status.lock();
    match &*status {
        SubmissionStatus::Failed { kind, .. } => Some(*kind),
        _ => None,
    }
}

/// How long [`await_submission`] sleeps between device queries once it
/// has stopped spinning.
///
/// The spin phase covers a kernel that is about to finish anyway; past
/// that, sleeping is cheaper than burning a core. 50 us is short enough
/// that the sleep is not what bounds a short kernel's observed latency
/// and long enough that a multi-millisecond kernel is not queried tens
/// of thousands of times.
#[cfg(feature = "gpu-offload")]
const AWAIT_SLEEP: std::time::Duration = std::time::Duration::from_micros(50);

/// How many times [`await_submission`] spins before it starts sleeping.
#[cfg(feature = "gpu-offload")]
const AWAIT_SPINS: u32 = 64;

/// Block until the submission completes or `timeout_nanos` elapses.
///
/// Returns [`GPU_AWAIT_COMPLETED`] (the submission reached a terminal
/// state — completed *or* failed; the caller reads which from the
/// status) or [`GPU_AWAIT_TIMED_OUT`].
///
/// # Why this is not just the Java loop moved down here
///
/// The Java fallback for a timed `get` polls `Native.futureStatus` on a
/// sleep loop. That costs a native crossing per poll and floors the
/// observed latency at the sleep interval — on a kernel that finishes in
/// 60 us, a 1 ms Java-side sleep reports it 16x late. Running the wait
/// here keeps the whole loop on one side of the boundary and lets the
/// spin phase observe completion at roughly device-callback latency.
///
/// A zero or negative `timeout_nanos` is a pure poll: the status is
/// checked once and the answer returned without sleeping.
#[cfg(feature = "gpu-offload")]
pub fn await_submission(shared: &crate::vm::SharedVm, handle: u64, timeout_nanos: u64) -> i32 {
    use cratonvm_native_api::registry::{GPU_AWAIT_COMPLETED, GPU_AWAIT_TIMED_OUT};

    let deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_nanos(timeout_nanos))
        // A timeout large enough to overflow `Instant` is, for every
        // practical purpose, "wait forever" — clamp rather than panic.
        .unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(86_400));

    let mut spins = 0u32;
    loop {
        match poll_submission_status(shared, handle) {
            // An unknown handle is terminal in the only sense that
            // matters here: no further waiting can change the answer.
            None => return GPU_AWAIT_COMPLETED,
            Some(PollOutcome::Completed) | Some(PollOutcome::Failed) => return GPU_AWAIT_COMPLETED,
            Some(PollOutcome::Running) => {}
        }

        if std::time::Instant::now() >= deadline {
            return GPU_AWAIT_TIMED_OUT;
        }

        if spins < AWAIT_SPINS {
            spins += 1;
            std::hint::spin_loop();
        } else {
            // Never overshoot the caller's deadline by a whole sleep
            // quantum: a 10 us timeout must not block for 50 us.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            std::thread::sleep(AWAIT_SLEEP.min(remaining));
        }
    }
}

/// One-`u64` failure-flag buffers, reused across dispatches.
///
/// Every launch needs one, and allocating it fresh meant a `cuMemAlloc`
/// on a path an inference step walks hundreds of times per token.
/// Measured on an RTX 2060: 117 us to submit a kernel that does
/// nothing, against 5.5 ms of real device work for a whole 453-kernel
/// forward pass — the dispatch floor, not the kernels, was the cost.
///
/// Buffers are keyed by the `DeviceContext`'s address so one device's
/// allocation can never be handed to another. A buffer goes back into
/// the pool only when it read ZERO, which is what keeps "came from the
/// pool" and "ready to be a failure flag" the same statement.
#[cfg(feature = "gpu-offload")]
pub(crate) mod flag_pool {
    use cuda_bridge::{DeviceBuffer, DeviceContext};
    use parking_lot::Mutex;
    use rustc_hash::FxHashMap;
    use std::sync::Arc;

    static POOLS: Mutex<Option<FxHashMap<usize, Vec<Arc<DeviceBuffer<u64>>>>>> = Mutex::new(None);

    /// Cap per context. A submission that is never finalized never
    /// returns its buffer, so this is a reuse cache and not a ledger;
    /// the cap keeps a pathological caller from growing it without
    /// bound.
    const MAX_POOLED: usize = 1024;

    pub(crate) fn take(key: usize, ctx: &DeviceContext) -> Result<Arc<DeviceBuffer<u64>>, String> {
        let pooled = {
            let mut guard = POOLS.lock();
            guard
                .as_mut()
                .and_then(|m| m.get_mut(&key))
                .and_then(|v| v.pop())
        };
        if let Some(buf) = pooled {
            return Ok(buf);
        }
        DeviceBuffer::<u64>::zeros(ctx, 1)
            .map(Arc::new)
            .map_err(|e| e.to_string())
    }

    /// Safe to call only from the finalize path: the submission's
    /// event has already been synchronized there, so no launch can
    /// still be writing through the pointer the launch closure pinned.
    pub(crate) fn give(key: usize, buf: &Arc<DeviceBuffer<u64>>) {
        let buf = Arc::clone(buf);
        let mut guard = POOLS.lock();
        let map = guard.get_or_insert_with(FxHashMap::default);
        let slot = map.entry(key).or_default();
        if slot.len() < MAX_POOLED {
            slot.push(buf);
        }
    }

    /// Drop every pooled buffer for one context, so the pool never
    /// outlives the memory it names.
    #[allow(dead_code)]
    pub(crate) fn clear(key: usize) {
        if let Some(map) = POOLS.lock().as_mut() {
            map.remove(&key);
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
        /// 16-bit elements: IEEE-754 binary16 bit patterns carried in a
        /// Java `short[]`, for the fp16 built-in kernels. Nothing in the
        /// cache interprets them; only the kernel does.
        I16(Arc<DeviceBuffer<i16>>),
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

    pub(crate) fn get_i16(handle: u64) -> Option<Arc<DeviceBuffer<i16>>> {
        match map().lock().get(&handle) {
            Some(Entry {
                buf: CachedBuffer::I16(arc),
                ..
            }) => Some(arc.clone()),
            _ => None,
        }
    }

    pub(crate) fn put_i16(handle: u64, buf: Arc<DeviceBuffer<i16>>) {
        map().lock().insert(
            handle,
            Entry {
                buf: CachedBuffer::I16(buf),
                dirty: false,
            },
        );
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
                // fp16 buffers are inputs to the built-in kernels and are
                // never written by one, so a dirty fp16 entry cannot arise.
                // If that changes, this needs a real download arm rather
                // than a silent None.
                CachedBuffer::I16(_) => return None,
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
            cratonvm_types::flags::runtime_var("CRATONVM_GPU_TRACE_BYTES")
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
// Host-write invalidation (Phase 10 #2 — closed):
//   - The interpreter's `Iastore`/`Lastore`/`Dastore` arms and the
//     `jit_iastore` / `jit_bastore` helpers call
//     `input_cache::invalidate(obj)` after the store lands, so a host
//     write always evicts the device mirror.
//   - `invalidate` is on the hottest path in the VM, so it is gated by
//     `ADDR_FILTER`: one relaxed load and a not-taken branch when
//     nothing is cached, which is every run that never submits a
//     kernel.
//   - The JIT's IR pipeline lowers `Op::ArrayStore` to a raw inline
//     `MOVSS`/`MOVSD` with no helper call, so there is nothing to hook
//     there. `offload_jit_gate` closes that hole from the other side:
//     while offload is live, a method containing `iastore`/`lastore`/
//     `fastore`/`dastore` is not admitted to the JIT at all.
//
// GC interaction (closed):
//   - Within a dispatch the GPU-critical guard keeps the GC paused, so
//     the marshalled `ObjectRef`s cannot move mid-submission.
//   - Across submits there is no guard, and the cache key is a bare
//     heap address. [`input_cache::remap_and_sweep`] is called once per
//     collection from `memory::gc::update_all_roots` to re-key moved
//     survivors and drop entries whose array died. Dropping is the half
//     that matters: a reclaimed address is reused by the allocator, and
//     the `element_type`/`len` guard in the getters does not separate a
//     recycled address from a genuine hit when the new array has the
//     same shape — which, for a kernel argument list, is the common
//     case rather than the exception.
//   - The cache is deliberately NOT a GC root. An array reachable only
//     from a cache entry can never be named by a future submit, so
//     rooting it would leak every array the GPU ever saw instead of
//     keeping anything useful alive. See `memory::addr_keyed`.
//   - `clear_all` from `releaseExecutor` remains as the coarse teardown
//     path; it is no longer load-bearing for GC correctness.
#[cfg(feature = "gpu-offload")]
pub(crate) mod input_cache {
    use cratonvm_types::{ArrayElementType, ObjectRef};
    use cuda_bridge::DeviceBuffer;
    use parking_lot::Mutex;
    use rustc_hash::FxHashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
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

    /// Per-VM tables, keyed by `SharedVm::vm_identity`.
    ///
    /// Process-global storage with a VM-scoped key, not a bare global
    /// map: one process can own several heaps at once, and an
    /// `ObjectRef` is only meaningful against the heap that allocated
    /// it. A single flat map would let VM A's address alias VM B's, and
    /// would hand VM A's post-GC sweep addresses belonging to a heap it
    /// does not own — the same reasoning that scoped the logmanager
    /// side-tables in `memory::native_roots`.
    static CACHE: OnceLock<Mutex<FxHashMap<usize, FxHashMap<ObjectRef, Entry>>>> = OnceLock::new();

    /// Cheap membership filter over the addresses currently cached.
    ///
    /// [`invalidate`] runs on **every primitive array store in the VM**,
    /// so it must not touch the mutex for the overwhelmingly common case
    /// of an array that was never marshalled to the device. Each cached
    /// address contributes one bit; a store whose bit is clear is
    /// definitely not cached and returns after a single relaxed load.
    ///
    /// False positives are possible and cost only a lock plus a failed
    /// lookup. False *negatives* are not, which is the property that
    /// makes skipping sound: every path that inserts an entry sets its
    /// bit before the entry becomes visible, and every path that removes
    /// entries rebuilds the whole filter. An empty cache leaves this at
    /// zero, so a build with the feature compiled in but no kernel ever
    /// submitted pays one load and a not-taken branch per array store.
    static ADDR_FILTER: AtomicU64 = AtomicU64::new(0);

    fn map() -> &'static Mutex<FxHashMap<usize, FxHashMap<ObjectRef, Entry>>> {
        CACHE.get_or_init(|| Mutex::new(FxHashMap::default()))
    }

    /// The filter bit for `obj`. Heap objects are 8-byte aligned, so the
    /// low three bits carry no information; index on the six bits above
    /// them.
    #[inline]
    fn addr_bit(obj: ObjectRef) -> u64 {
        1u64 << ((obj.as_ptr() as usize >> 3) & 63)
    }

    /// Recompute [`ADDR_FILTER`] from the live tables. Called by every
    /// path that removes entries; insertion just ORs its bit in.
    fn rebuild_filter(tables: &FxHashMap<usize, FxHashMap<ObjectRef, Entry>>) {
        let mut bits = 0u64;
        for table in tables.values() {
            for obj in table.keys() {
                bits |= addr_bit(*obj);
            }
        }
        ADDR_FILTER.store(bits, Ordering::Release);
    }

    /// Per-type lookup. Returns `None` on miss OR if the cached
    /// entry's element type / length doesn't match the requested
    /// shape (defensive: a stale `ObjectRef` could be reused for a
    /// different array kind across a GC; we treat that as a miss
    /// and the caller re-uploads).
    pub(crate) fn get_i32(vm: usize, obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<i32>>> {
        let g = map().lock();
        let e = g.get(&vm)?.get(&obj)?;
        if e.element_type != ArrayElementType::Int || e.len != len {
            return None;
        }
        if let CachedBuffer::I32(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_i32(vm: usize, obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<i32>>) {
        insert(
            vm,
            obj,
            Entry {
                buf: CachedBuffer::I32(buf),
                len,
                element_type: ArrayElementType::Int,
            },
        );
    }
    pub(crate) fn get_i64(vm: usize, obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<i64>>> {
        let g = map().lock();
        let e = g.get(&vm)?.get(&obj)?;
        if e.element_type != ArrayElementType::Long || e.len != len {
            return None;
        }
        if let CachedBuffer::I64(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_i64(vm: usize, obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<i64>>) {
        insert(
            vm,
            obj,
            Entry {
                buf: CachedBuffer::I64(buf),
                len,
                element_type: ArrayElementType::Long,
            },
        );
    }
    pub(crate) fn get_f32(vm: usize, obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<f32>>> {
        let g = map().lock();
        let e = g.get(&vm)?.get(&obj)?;
        if e.element_type != ArrayElementType::Float || e.len != len {
            return None;
        }
        if let CachedBuffer::F32(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_f32(vm: usize, obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<f32>>) {
        insert(
            vm,
            obj,
            Entry {
                buf: CachedBuffer::F32(buf),
                len,
                element_type: ArrayElementType::Float,
            },
        );
    }
    pub(crate) fn get_f64(vm: usize, obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<f64>>> {
        let g = map().lock();
        let e = g.get(&vm)?.get(&obj)?;
        if e.element_type != ArrayElementType::Double || e.len != len {
            return None;
        }
        if let CachedBuffer::F64(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_f64(vm: usize, obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<f64>>) {
        insert(
            vm,
            obj,
            Entry {
                buf: CachedBuffer::F64(buf),
                len,
                element_type: ArrayElementType::Double,
            },
        );
    }

    /// Install an entry, publishing its filter bit **first**.
    ///
    /// The bit must become visible no later than the entry itself: a
    /// concurrent [`invalidate`] that saw the entry but not the bit
    /// would skip a live entry and leave the device mirror stale. The
    /// reverse window (bit set, entry not yet inserted) is a harmless
    /// false positive.
    fn insert(vm: usize, obj: ObjectRef, entry: Entry) {
        ADDR_FILTER.fetch_or(addr_bit(obj), Ordering::AcqRel);
        map().lock().entry(vm).or_default().insert(obj, entry);
    }

    /// Drop the device-buffer cache entry for `obj` because the host
    /// just wrote to that array — the device copy no longer mirrors it.
    ///
    /// Wired to every primitive array store the VM can observe
    /// (Phase 10 #2): the interpreter's `*astore` arms and the JIT's
    /// `jit_iastore` / `jit_bastore` helpers. See the module comment for
    /// the one path that cannot call this and how it is handled instead.
    ///
    /// Address-keyed across **all** VMs rather than taking a `vm`
    /// argument: the store sites are the hottest paths in the
    /// interpreter, and threading an identity through them buys nothing.
    /// Removing a same-address entry belonging to another VM is at worst
    /// a spurious re-upload there, never a wrong answer.
    pub fn invalidate(obj: ObjectRef) {
        // Fast path: no cached address hashes to this bit, so `obj` is
        // definitely not cached. This is the case for essentially every
        // array store in a real program.
        if ADDR_FILTER.load(Ordering::Acquire) & addr_bit(obj) == 0 {
            return;
        }
        let mut tables = map().lock();
        let mut removed = false;
        for table in tables.values_mut() {
            removed |= table.remove(&obj).is_some();
        }
        if removed {
            rebuild_filter(&tables);
        }
    }

    /// Drop every entry belonging to `vm`. Called from
    /// `releaseExecutor` as a teardown path. GC correctness does not
    /// depend on it — see [`remap_and_sweep`].
    pub fn clear_all(vm: usize) {
        let mut tables = map().lock();
        tables.remove(&vm);
        rebuild_filter(&tables);
    }

    /// Post-collection fixup: re-key surviving entries through the
    /// collector's old→new map and drop entries whose Java array did
    /// not survive.
    ///
    /// Called once per collection from
    /// [`crate::memory::gc::update_all_roots`], **before** its
    /// empty-`pointer_map` early return, because the sweep half is
    /// needed on a non-moving collection too: objects still die there,
    /// and a stale key over a reclaimed address is what turns a later
    /// cache lookup into a false hit.
    ///
    /// The device buffer itself needs no fixup. It lives in device
    /// memory and never holds a JVM heap address — the marshal path
    /// copies the payload out of the heap at upload time — so
    /// relocating the source array leaves the mirror valid. Only the
    /// key is address-derived.
    ///
    /// # Locking
    ///
    /// Runs with the world stopped and takes the cache mutex, which is
    /// safe because every other holder of that mutex (the getters,
    /// `put_*`, `invalidate`) does a bounded map operation with no
    /// safepoint poll in between, so a stopped mutator can never be
    /// parked while holding it.
    pub fn remap_and_sweep(
        vm: usize,
        pointer_map: &cratonvm_types::PointerMap,
        heap: &crate::memory::vm_heap::VmHeap,
    ) {
        let mut tables = map().lock();
        // Only this VM's table: `heap` belongs to `vm`, and asking it
        // about another VM's addresses would report every one of them
        // dead and silently flush that VM's cache.
        let Some(table) = tables.get_mut(&vm) else {
            return;
        };
        let stats = crate::memory::addr_keyed::remap_and_sweep(table, pointer_map, &|addr| {
            heap.is_object_address(addr).is_some()
        });
        if stats.moved == 0 && stats.dropped == 0 {
            return;
        }
        // Re-keying moves addresses and sweeping removes them, so the
        // filter no longer describes the table. (`table`'s borrow ends
        // above, at its last use.)
        rebuild_filter(&tables);
        tracing::debug!(
            "gpu input_cache post-GC: {} re-keyed, {} retained, {} dropped",
            stats.moved,
            stats.retained,
            stats.dropped
        );
    }

    /// Diagnostic: entry count for `vm`.
    pub fn len(vm: usize) -> usize {
        map().lock().get(&vm).map_or(0, FxHashMap::len)
    }
}

/// One chunk of a chunked writeback: where it lives in the array, and the
/// event that fires when its device->staging copy has landed.
#[cfg(feature = "gpu-offload")]
pub struct WritebackChunk {
    /// First element index of this chunk within the array.
    pub lo: usize,
    /// Element count.
    pub len: usize,
    /// Recorded after this chunk's kernel AND its device->staging copy,
    /// on the stream both were issued on.
    pub done: std::sync::Arc<cuda_bridge::Event>,
}

/// The typed halves of a chunked writeback: the device buffer the kernel
/// wrote, and the page-locked host staging it is streamed into.
///
/// Page-locked is the point. An async device->host copy only overlaps with
/// kernel execution when its destination is page-locked; measured on an
/// RTX 2060, 11 MB, 12.9 GB/s into page-locked memory against 8.6 GB/s for
/// the async form into ordinary pageable memory. The Java heap arena is
/// pageable, so the chunk lands in staging and is memcpy'd on from there
/// (~26 GB/s) while later chunks are still on the GPU.
#[cfg(feature = "gpu-offload")]
pub enum ChunkedStage {
    I32 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i32>>,
        host: std::sync::Arc<cuda_bridge::PinnedHostBuffer<i32>>,
    },
    I64 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i64>>,
        host: std::sync::Arc<cuda_bridge::PinnedHostBuffer<i64>>,
    },
    F32 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f32>>,
        host: std::sync::Arc<cuda_bridge::PinnedHostBuffer<f32>>,
    },
    F64 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f64>>,
        host: std::sync::Arc<cuda_bridge::PinnedHostBuffer<f64>>,
    },
}

/// How many streams a chunked dispatch rotates its launches over, and how
/// many chunks it splits the iteration space into.
///
/// Both were swept in the VM on an RTX 2060 against the four-sphere ray
/// tracer at 2.76M elements, on a quiet host (HotSpot control within 2%
/// of its idle baseline), using `CRATONVM_GPU_CHUNKS=1` as a same-binary
/// kill switch for the off arm:
///
/// ```text
///   streams  chunks    1(off)      4       8      16      32
///        2              1.645    1.304   1.127   1.316   1.645
///        4              1.718    1.283   1.186   1.293   1.683
///        8              1.643    1.293   1.092   1.291   1.666
/// ```
///
/// Eight chunks is the floor in every row and 32 is no better than not
/// chunking at all -- per-launch cost grows linearly while the overlap it
/// buys does not. Stream count matters much less; 8 won a three-repeat
/// re-measure at 8 chunks (1.092 mean, against 1.149 for four and 1.163
/// for two) and is cheap because the streams are created once and cached.
///
/// 8x8 against the off arm is 1.643 -> 1.092 ms, a 1.51x speedup.
#[cfg(feature = "gpu-offload")]
const CHUNK_STREAMS_DEFAULT: usize = 8;
#[cfg(feature = "gpu-offload")]
const CHUNK_COUNT_DEFAULT: usize = 8;

/// Streams the chunked dispatch rotates launches over.
/// Override with `CRATONVM_GPU_CHUNK_STREAMS`.
#[cfg(feature = "gpu-offload")]
fn chunk_streams_wanted() -> usize {
    use std::sync::OnceLock;
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GPU_CHUNK_STREAMS")
            .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
            .filter(|n: &usize| *n >= 1 && *n <= 32)
            .unwrap_or(CHUNK_STREAMS_DEFAULT)
    })
}

/// Chunks the iteration space is split into.
/// Override with `CRATONVM_GPU_CHUNKS`; 1 disables chunking entirely,
/// which is the kill switch for A/B-ing this whole path on ONE binary.
#[cfg(feature = "gpu-offload")]
fn chunk_count_wanted() -> usize {
    use std::sync::OnceLock;
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GPU_CHUNKS")
            .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
            .filter(|n: &usize| *n >= 1 && *n <= 256)
            .unwrap_or(CHUNK_COUNT_DEFAULT)
    })
}

/// Below this many elements, chunking costs more in per-launch overhead
/// than it recovers in overlap.
///
/// The overlap can only hide `min(kernel, transfer)`, and both scale with
/// the element count, while the extra launches are a fixed cost per chunk.
/// At 307K elements the ray tracer's whole GPU-side cost is already only
/// ~0.28 ms, of which 16 extra launches would be a visible slice. Kept
/// deliberately conservative: a workload big enough to care is far above
/// this line.
#[cfg(feature = "gpu-offload")]
const CHUNK_MIN_ELEMS: usize = 1 << 19;

/// Decide whether this submission can use the overlapped chunked
/// writeback, and if so take the array writeback out of `writebacks` so
/// the caller can replace it.
///
/// # Why this is allowed to break the "no partial GPU state" rule
///
/// A chunked writeback commits each chunk into the Java array as that
/// chunk's event fires, which is BEFORE the bounds-failure flag has been
/// read. `finalize_submission` normally drains the flag first precisely so
/// a failed kernel leaves the heap untouched and the CPU re-run starts
/// clean (docs/book/src/gpu/overview.md, Exceptions).
///
/// That rule can be relaxed for an array the kernel writes and never
/// READS, and only for such an array:
///
///   * The values a committed chunk holds are the values the kernel
///     computed for those iterations, and its threads succeeded — the
///     flag is set by the failing thread, not by its neighbours, so a
///     committed chunk never contains garbage.
///   * On deopt the interpreter re-runs the whole method from iteration
///     0. It rewrites every element it would have written and throws at
///     the same index, so the committed elements are a subset of what
///     plain Java writes before the throw, with the same values. The
///     observable end state is identical.
///   * That argument fails the moment the kernel READS the array, because
///     then the partial commit is the re-run's own input. Hence the
///     `writes & !reads` gate below rather than `writes` alone.
///
/// The remaining conditions are about being able to identify the array at
/// all: exactly one written param and exactly one array writeback, so the
/// mask bit and the writeback provably refer to the same array.
#[cfg(feature = "gpu-offload")]
fn take_chunkable_writeback(
    sig: &jit_cuda::signature::KernelSignature,
    writebacks: &mut Vec<MarshalWriteback>,
    runtime_work: u32,
) -> Option<MarshalWriteback> {
    if (runtime_work as usize) < CHUNK_MIN_ELEMS || chunk_count_wanted() < 2 {
        return None;
    }
    // Exactly one param written, and it is not also read.
    let streamable = sig.writes_param_mask & !sig.reads_param_mask;
    if streamable.count_ones() != 1 || sig.writes_param_mask.count_ones() != 1 {
        return None;
    }
    // A reduction writes its accumulator through `ret_ptr` with an atomic;
    // chunking that would be a different (and racier) problem.
    if sig.is_reduction {
        return None;
    }
    // Exactly one plain-array writeback, so it must be the streamable one.
    let idx = {
        let mut found = None;
        for (i, wb) in writebacks.iter().enumerate() {
            let plain = matches!(
                wb,
                MarshalWriteback::I32 { .. }
                    | MarshalWriteback::I64 { .. }
                    | MarshalWriteback::F32 { .. }
                    | MarshalWriteback::F64 { .. }
            );
            let other_array = wb.array_len().is_some() && !plain;
            if other_array {
                // A resident/GpuArray writeback in the mix: bail rather
                // than reason about which one the mask names.
                return None;
            }
            if plain {
                if found.is_some() {
                    return None;
                }
                found = Some(i);
            }
        }
        found?
    };
    // The chunk tiling below assumes the array covers the whole launch.
    if writebacks[idx].array_len() != Some(runtime_work as usize) {
        return None;
    }
    Some(writebacks.remove(idx))
}

/// Issue a chunked, overlapped dispatch.
///
/// Splits `[0, work)` into `chunk_count_wanted()` chunks and, for each, launches
/// the kernel with that chunk's `tid_base` on one of `chunk_streams_wanted()`
/// rotating streams and issues the chunk's device->staging copy on the
/// SAME stream, then records an event. Because each chunk's copy is
/// ordered behind only its own kernel, chunk N's DMA runs while chunk
/// N+1's kernel does.
///
/// Returns the replacement writeback, or `Err` with a message. On `Err`
/// the caller must fall back to the whole-array path — nothing has been
/// committed to the Java heap either way, since that only happens at
/// finalize.
#[cfg(feature = "gpu-offload")]
#[allow(clippy::too_many_arguments)]
fn launch_chunked(
    cache: &OffloadCache,
    events: &[std::sync::Arc<cuda_bridge::Event>],
    ctx: &cuda_bridge::DeviceContext,
    kernel: &CompiledKernel,
    args: &cuda_bridge::KernelArgs,
    work: usize,
    streams: &[std::sync::Arc<Stream>],
    plain: MarshalWriteback,
) -> Result<MarshalWriteback, String> {
    // The typed device buffer and a page-locked staging slab the size of
    // the whole array. One slab, not one per chunk: chunks tile it, so no
    // chunk's DMA and no chunk's memcpy ever touch the same bytes, and
    // there is no slot to wait for before reusing.
    let (obj, stage) = match plain {
        MarshalWriteback::I32 { obj, buf, len } => (
            obj,
            ChunkedStage::I32 {
                buf,
                host: cache.staging_i32(ctx, len)?,
            },
        ),
        MarshalWriteback::I64 { obj, buf, len } => (
            obj,
            ChunkedStage::I64 {
                buf,
                host: cache.staging_i64(ctx, len)?,
            },
        ),
        MarshalWriteback::F32 { obj, buf, len } => (
            obj,
            ChunkedStage::F32 {
                buf,
                host: cache.staging_f32(ctx, len)?,
            },
        ),
        MarshalWriteback::F64 { obj, buf, len } => (
            obj,
            ChunkedStage::F64 {
                buf,
                host: cache.staging_f64(ctx, len)?,
            },
        ),
        other => {
            // `take_chunkable_writeback` only ever hands back the four
            // plain-array variants; anything else is a bug there.
            return Err(format!(
                "launch_chunked given a non-array writeback (len={:?})",
                other.array_len()
            ));
        }
    };

    let chunk = work.div_ceil(chunk_count_wanted()).max(1);
    // One occupancy query for the whole dispatch, not one per chunk: it
    // is a driver round-trip and every chunk but the last is the same
    // size anyway. The last, shorter chunk just under-fills its final
    // block, which the kernel guard already handles.
    // Cast: element count -> u32 grid sizing (JVM array length fits).
    let cfg = kernel
        .module
        .elementwise_for_kernel(ctx, &kernel.kernel_name, chunk as u32);
    let mut chunks: Vec<WritebackChunk> = Vec::with_capacity(chunk_count_wanted());
    let mut lo = 0usize;
    while lo < work {
        let len = chunk.min(work - lo);
        let stream = &streams[chunks.len() % streams.len()];
        // Cast: element index -> i32 launch base. `work` came from a JVM
        // array length, which is bounded by i32::MAX, so `lo` fits.
        let base = i32::try_from(lo).map_err(|_| format!("chunk base {lo} exceeds i32"))?;
        let chunk_args = args
            .with_tid_base(base)
            .ok_or_else(|| "kernel args do not end in a tid_base slot".to_string())?;
        kernel
            .module
            .launch_on_stream(ctx, &kernel.kernel_name, &cfg, chunk_args, stream)
            .map_err(|e| format!("chunked launch (lo={lo} len={len}): {e}"))?;

        // The copy goes on the SAME stream as its launch, so stream order
        // alone puts it after that chunk's kernel.
        macro_rules! copy_chunk {
            ($buf:expr, $host:expr) => {{
                // SAFETY: `[lo, lo+len)` is this chunk's exclusive slice of
                // the staging slab -- chunks tile it and none overlaps --
                // and nothing reads it until this chunk's event fires.
                let dst = unsafe { &mut $host.as_mut_slice()[lo..lo + len] };
                // SAFETY: `dst` is page-locked staging that outlives the
                // submission (it is moved into the returned writeback), and
                // the CPU does not touch the range until the event below.
                unsafe { $buf.to_host_async_range_unchecked(dst, lo, stream) }
                    .map_err(|e| format!("chunked copy (lo={lo} len={len}): {e}"))?;
            }};
        }
        match &stage {
            ChunkedStage::I32 { buf, host } => copy_chunk!(buf, host),
            ChunkedStage::I64 { buf, host } => copy_chunk!(buf, host),
            ChunkedStage::F32 { buf, host } => copy_chunk!(buf, host),
            ChunkedStage::F64 { buf, host } => copy_chunk!(buf, host),
        }

        // A pooled event when one is available, else a fresh one.
        let done = match events.get(chunks.len()) {
            Some(e) => std::sync::Arc::clone(e),
            None => std::sync::Arc::new(
                cuda_bridge::Event::new(ctx).map_err(|e| format!("chunk event (lo={lo}): {e}"))?,
            ),
        };
        stream
            .record_event(&done)
            .map_err(|e| format!("chunk record_event (lo={lo}): {e}"))?;
        chunks.push(WritebackChunk { lo, len, done });
        lo += len;
    }

    Ok(MarshalWriteback::Chunked { obj, stage, chunks })
}

/// Define an `OffloadCache` accessor for a reusable page-locked staging
/// slab of one element type.
///
/// `cuMemAllocHost` of a frame-sized slab is expensive enough to swamp
/// the overlap it enables: allocating 11 MB per dispatch made the chunked
/// writeback 2.8x SLOWER than the whole-array one it replaced. The slab is
/// therefore kept and reused across dispatches of the same size, which is
/// the normal case (one kernel called in a loop over one array).
///
/// Reuse is refused unless the cache holds the ONLY reference. A
/// submission that has not finalized yet still owns its slab through the
/// writeback, and handing that memory to a second concurrent dispatch
/// would let one chunk's DMA land in another submission's staging.
#[cfg(feature = "gpu-offload")]
macro_rules! staging_slot {
    ($name:ident, $ty:ty, $slot:ident) => {
        impl OffloadCache {
            fn $name(
                &self,
                ctx: &cuda_bridge::DeviceContext,
                len: usize,
            ) -> Result<std::sync::Arc<cuda_bridge::PinnedHostBuffer<$ty>>, String> {
                {
                    let held = self.$slot.read();
                    if let Some(buf) = held.as_ref() {
                        if buf.len() == len && std::sync::Arc::strong_count(buf) == 1 {
                            return Ok(std::sync::Arc::clone(buf));
                        }
                    }
                }
                let fresh = std::sync::Arc::new(
                    cuda_bridge::PinnedHostBuffer::<$ty>::new(ctx, len).map_err(|e| {
                        format!("pinned staging {} (len={len}): {e}", stringify!($ty))
                    })?,
                );
                *self.$slot.write() = Some(std::sync::Arc::clone(&fresh));
                Ok(fresh)
            }
        }
    };
}

#[cfg(feature = "gpu-offload")]
staging_slot!(staging_i32, i32, chunk_stage_i32);
#[cfg(feature = "gpu-offload")]
staging_slot!(staging_i64, i64, chunk_stage_i64);
#[cfg(feature = "gpu-offload")]
staging_slot!(staging_f32, f32, chunk_stage_f32);
#[cfg(feature = "gpu-offload")]
staging_slot!(staging_f64, f64, chunk_stage_f64);

// ── Per-type marshalling helpers ────────────────────────────────────

#[cfg(feature = "gpu-offload")]
pub enum MarshalWriteback {
    // (Chunked, below, is the overlapped writeback; see its comment.)
    /// A write-only array streamed back in chunks.
    ///
    /// Instead of one device->host copy after the whole kernel, the
    /// dispatch launched the kernel once per chunk and issued each
    /// chunk's copy on the same stream, so chunk N's copy runs while
    /// chunk N+1's kernel does. The drain below waits each chunk's event
    /// in turn and memcpys it into the Java array, which puts the host
    /// copy under the GPU work still outstanding behind it.
    ///
    /// This is the ONE writeback that may land in the Java heap before
    /// the bounds-failure flag has been read, and the dispatch only
    /// builds it for an array in `writes & !reads` — see
    /// `plan_chunked_writeback` for the full argument.
    Chunked {
        obj: cratonvm_types::ObjectRef,
        stage: ChunkedStage,
        chunks: Vec<WritebackChunk>,
    },
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
        /// Which device context the buffer came from, so finalize can
        /// return it to that context's pool and never another's. The
        /// key is the `DeviceContext`'s address, which is stable for
        /// the process because the context lives in the `OffloadCache`.
        pool_key: usize,
    },
    /// Part E — owns the 1-element device buffer a scalar-return
    /// kernel's `ret_ptr` param points at (see
    /// `jit_cuda::lowering::build_param_list`). For a proven reduction
    /// (`KernelSignature::is_reduction`) the kernel epilogue is
    /// `atom.global.add.u32 [ret_ptr], value` — the buffer MUST be
    /// zero-initialized before launch so the atomic add lands on the
    /// correct identity element (`0`, matching Java's implicit
    /// accumulator initializer; see `KernelSignature::is_reduction`'s
    /// doc comment). `dispatch_method_from_native` allocates this via
    /// `DeviceBuffer::<i32>::zeros`. The writeback downloads the
    /// accumulated value and surfaces it as `SerializedResult::ScalarI32`.
    ScalarI32 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i32>>,
    },
    /// `)J`-descriptor counterpart of `ScalarI32` — `atom.global.add.u64`.
    ScalarI64 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i64>>,
    },
    /// `)F`-descriptor counterpart of `ScalarI32` — `atom.global.add.f32`.
    /// Only reachable via the explicit `submitMethod` API; `try_dispatch`
    /// never requests a float reduction (see `SerializedResult::ScalarF32`).
    ScalarF32 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f32>>,
    },
    /// `)D`-descriptor counterpart of `ScalarI32` — `atom.global.add.f64`.
    ScalarF64 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<f64>>,
    },
}

#[cfg(feature = "gpu-offload")]
impl MarshalWriteback {
    /// Drain this writeback. Returns `Ok(Some(result))` when the
    /// writeback produced a value the submission's terminal
    /// `SerializedResult` must carry (Part E — the `Scalar*` variants);
    /// every other writeback returns `Ok(None)` on success (their
    /// output already landed in the JVM heap / resident store / cache
    /// dirty-bit, not in `SerializedResult`). `finalize_submission`
    /// keeps the *last* `Some` seen across the drain — today at most
    /// one writeback per submission ever returns `Some` (a kernel has
    /// at most one `ret_ptr`), so "last" is really "the only one".
    fn writeback(
        &self,
        shared: &crate::vm::SharedVm,
        token: &cratonvm_gc::safepoint::SafepointToken<'_>,
    ) -> Result<Option<SerializedResult>, String> {
        use crate::runtime::gpu_marshal;
        match self {
            // Download the kernel-written buffer straight into the JVM heap
            // arena (no staging Vec + write-back) when the array is contiguous.
            // Chunked: wait each chunk's event in turn and copy that
            // chunk's page-locked staging into the Java array. Waiting
            // per chunk rather than for the whole submission is the
            // point: while this memcpy runs, the chunks behind it are
            // still transferring.
            Self::Chunked { obj, stage, chunks } => {
                macro_rules! drain {
                    ($host:expr, $write:path, $what:literal) => {{
                        for chunk in chunks {
                            chunk.done.synchronize().map_err(|e| {
                                format!("chunked {} wait (lo={}): {e}", $what, chunk.lo)
                            })?;
                            // SAFETY: this chunk's event has fired, so the
                            // DMA that owned `[lo, lo+len)` in staging is
                            // complete and the range is ours to read. No
                            // other chunk covers it.
                            let src =
                                unsafe { &$host.as_mut_slice()[chunk.lo..chunk.lo + chunk.len] };
                            $write(*obj, &shared.mem.heap, src, chunk.lo, token).map_err(|e| {
                                format!("chunked {} write (lo={}): {e}", $what, chunk.lo)
                            })?;
                        }
                    }};
                }
                match stage {
                    ChunkedStage::I32 { host, .. } => {
                        drain!(host, gpu_marshal::write_back_range_i32, "i32")
                    }
                    ChunkedStage::I64 { host, .. } => {
                        drain!(host, gpu_marshal::write_back_range_i64, "i64")
                    }
                    ChunkedStage::F32 { host, .. } => {
                        drain!(host, gpu_marshal::write_back_range_f32, "f32")
                    }
                    ChunkedStage::F64 { host, .. } => {
                        drain!(host, gpu_marshal::write_back_range_f64, "f64")
                    }
                }
                Ok(None)
            }
            Self::I32 { obj, buf, .. } => {
                gpu_marshal::download_obj_i32(buf.as_ref(), *obj, &shared.mem.heap, token)
                    .map_err(|e| format!("download i32: {e}"))?;
                Ok(None)
            }
            Self::I64 { obj, buf, .. } => {
                gpu_marshal::download_obj_i64(buf.as_ref(), *obj, &shared.mem.heap, token)
                    .map_err(|e| format!("download i64: {e}"))?;
                Ok(None)
            }
            Self::F32 { obj, buf, .. } => {
                gpu_marshal::download_obj_f32(buf.as_ref(), *obj, &shared.mem.heap, token)
                    .map_err(|e| format!("download f32: {e}"))?;
                Ok(None)
            }
            Self::F64 { obj, buf, .. } => {
                gpu_marshal::download_obj_f64(buf.as_ref(), *obj, &shared.mem.heap, token)
                    .map_err(|e| format!("download f64: {e}"))?;
                Ok(None)
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
                Ok(None)
            }
            Self::ResidentI64 { handle, .. } => {
                device_cache::mark_dirty(*handle);
                Ok(None)
            }
            Self::ResidentF32 { handle, .. } => {
                device_cache::mark_dirty(*handle);
                Ok(None)
            }
            Self::ResidentF64 { handle, .. } => {
                device_cache::mark_dirty(*handle);
                Ok(None)
            }
            // Phase 9 #1 follow-up — read the failure flag back.
            // If non-zero, the kernel hit a bounds check; report as
            // a writeback error so `finalize_submission` flips the
            // submission to `Failed` and Java sees a `GpuException`.
            Self::FailureFlag { buf, pool_key } => {
                let mut cell = [0u64; 1];
                gpu_marshal::download_into(&buf, &mut cell)
                    .map_err(|e| format!("download_into failure_flag: {e}"))?;
                if cell[0] != 0 {
                    // Deliberately NOT returned to the pool: a buffer
                    // that read non-zero is not zero, and the pool's
                    // whole contract is that what comes out of it is
                    // ready to be a fresh failure flag.
                    return Err(format!(
                        "kernel failure flag set (value={}): out-of-range index inside kernel body",
                        cell[0]
                    ));
                }
                flag_pool::give(*pool_key, buf);
                Ok(None)
            }
            // Part E — scalar-return accumulator readback. The device
            // buffer was pre-zeroed by `dispatch_method_from_native`
            // before launch (see the `ScalarI32` field doc); download
            // the single cell and hand it back as the matching
            // `SerializedResult` so `finalize_submission` can stamp it
            // onto the submission's terminal status instead of `Void`.
            Self::ScalarI32 { buf } => {
                let mut cell = [0i32; 1];
                gpu_marshal::download_into(buf, &mut cell)
                    .map_err(|e| format!("download scalar i32 accumulator: {e}"))?;
                Ok(Some(SerializedResult::ScalarI32(cell[0])))
            }
            Self::ScalarI64 { buf } => {
                let mut cell = [0i64; 1];
                gpu_marshal::download_into(buf, &mut cell)
                    .map_err(|e| format!("download scalar i64 accumulator: {e}"))?;
                Ok(Some(SerializedResult::ScalarI64(cell[0])))
            }
            Self::ScalarF32 { buf } => {
                let mut cell = [0f32; 1];
                gpu_marshal::download_into(buf, &mut cell)
                    .map_err(|e| format!("download scalar f32 accumulator: {e}"))?;
                Ok(Some(SerializedResult::ScalarF32(cell[0])))
            }
            Self::ScalarF64 { buf } => {
                let mut cell = [0f64; 1];
                gpu_marshal::download_into(buf, &mut cell)
                    .map_err(|e| format!("download scalar f64 accumulator: {e}"))?;
                Ok(Some(SerializedResult::ScalarF64(cell[0])))
            }
        }
    }

    /// Element-count of the array this writeback owns. Used by
    /// `dispatch_method_from_native` to compute the launch grid:
    /// the kernel needs `>= max(array_len)` threads to cover every
    /// output index. Returns `None` for the `FailureFlag` and
    /// `Scalar*` variants (no array body — just a 1-cell signal).
    pub fn array_len(&self) -> Option<usize> {
        match self {
            // A chunked writeback covers the whole array; its length is
            // the sum of the chunks, which by construction tile it.
            Self::Chunked { chunks, .. } => Some(chunks.iter().map(|c| c.len).sum()),
            Self::I32 { len, .. }
            | Self::I64 { len, .. }
            | Self::F32 { len, .. }
            | Self::F64 { len, .. }
            | Self::ResidentI32 { len, .. }
            | Self::ResidentI64 { len, .. }
            | Self::ResidentF32 { len, .. }
            | Self::ResidentF64 { len, .. } => Some(*len),
            Self::FailureFlag { .. }
            | Self::ScalarI32 { .. }
            | Self::ScalarI64 { .. }
            | Self::ScalarF32 { .. }
            | Self::ScalarF64 { .. } => None,
        }
    }
}

/// Phase 6 #3: detect a `craton.gpu.GpuArray` Java object and read
/// its (element_type, length, native handle) tuple from the
/// resident-array store. Returns `None` for any non-GpuArray object so
/// callers can fall through to other arg shapes.
///
/// **Shape only, deliberately.** This used to return the host bytes
/// too, which meant every submit copied the whole array out of the
/// resident store — including the overwhelmingly common case where the
/// buffer was already on the device and the copy was thrown away
/// unread. For a weight tensor that is resident for the life of the
/// process (a language model's, say) that is the entire point of
/// residency undone: gigabytes of `memcpy` per kernel launch. The bytes
/// are now fetched by `marshal_resident_array_arg` on the cache-miss
/// path only.
///
/// The GpuArray Java layout (P3-2):
///   field 0: `long handle`
///   field 1: `Class<?> elementType`  (unused here; type comes from
///                                     the resident-store record)
#[cfg(feature = "gpu-offload")]
fn try_gpu_array_shape(
    shared: &crate::vm::SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<(cratonvm_types::ArrayElementType, usize, u64)> {
    let cid = shared.mem.heap.class_id_of(obj_ref);
    let cm = shared.classes.class_manager.read();
    let cls_name = cm.get_class(cid).map(|c| c.name.to_string())?;
    drop(cm);
    if cls_name != "craton/gpu/GpuArray" {
        return None;
    }
    // field 0 holds the long `handle`.
    let handle = match shared.mem.heap.get_field(obj_ref, 0) {
        cratonvm_types::Value::Long(h) => h as u64,
        _ => return None,
    };
    let (etype, len) = cratonvm_native_builtins::craton_gpu::array_shape(handle)?;
    Some((etype, len, handle))
}

/// Remember one declared parameter's array length, growing the vector
/// as needed. Indexed by the analyzer's declared-parameter index — the
/// same index `writes_param_mask` is bit-indexed by, and the one
/// `WorkBound::ParamLen` names.
#[cfg(feature = "gpu-offload")]
fn record_param_len(param_lens: &mut Vec<Option<usize>>, index: usize, len: usize) {
    if param_lens.len() <= index {
        param_lens.resize(index + 1, None);
    }
    param_lens[index] = Some(len);
}

/// Marshal a resident GpuArray as a kernel arg. The shape of the
/// closure + writeback record mirrors `marshal_array_arg` but the
/// source bytes are the resident-store snapshot rather than a JVM
/// array — and the writeback target is the resident store (so a
/// subsequent `GpuArray.toHost()` reads the post-kernel content).
///
/// The host bytes are read from the resident store only when the
/// device cache misses; a hit never touches them.
#[cfg(feature = "gpu-offload")]
fn marshal_resident_array_arg(
    ctx: &cuda_bridge::DeviceContext,
    element_type: cratonvm_types::ArrayElementType,
    len: usize,
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
                // (b) Miss — read the host bytes (the only path that
                // needs them at all), upload and install.
                let host_bytes = cratonvm_native_builtins::craton_gpu::array_snapshot(arr_handle)
                    .map(|(_, _, bytes)| bytes)
                    .ok_or_else(|| {
                        format!("GpuArray handle {arr_handle} released before its first upload")
                    })?;
                let host: &[$ty] = bytemuck::cast_slice(&host_bytes);
                let buf = crate::runtime::gpu_marshal::upload(ctx, host)
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
    let cid = shared.mem.heap.class_id_of(obj_ref);
    let cm = shared.classes.class_manager.read();
    let cls_name = cm.get_class(cid).map(|c| c.name.to_string())?;
    drop(cm);
    let inner = shared.mem.heap.get_field(obj_ref, 0);
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
    let len = shared.mem.heap.array_length(obj_ref);

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
                if let Some(arc) = $cache_get(shared.vm_identity, obj_ref, len) {
                    (arc, false)
                } else {
                    // (b) Miss — upload, reading the JVM heap arena directly
                    //     (no staging Vec) when the array is contiguous, then
                    //     install into the input cache.
                    let buf = $upload_obj(ctx, obj_ref, &shared.mem.heap, token)
                        .map_err(|e| format!("upload {} (len={len}): {e}", $tag))?;
                    let arc = Arc::new(buf);
                    $cache_put(shared.vm_identity, obj_ref, len, arc.clone());
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
