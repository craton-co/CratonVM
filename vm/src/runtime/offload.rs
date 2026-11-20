// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
pub(crate) fn decode_method_attrs(
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
    /// `true` when a `failure_flag` trap can fire after or concurrently with a global store.
    pub can_fail_after_store: bool,
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
    /// proper; we only keep `print_gpu_decisions` and `approx_math` here.
    print_decisions: bool,
    approx_math: bool,
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
    /// Round-robin pool of streams for dispatches that did not bring
    /// their own.
    ///
    /// AUDIT 2026-09-02: the handle-less path — which is every
    /// transparent interpreter dispatch, i.e. the common case — used to
    /// call `Stream::new(ctx)` per submission and drop it when the
    /// submission was released. `cuStreamCreate` is not free and
    /// `cuStreamDestroy` can synchronize, so a workload calling an
    /// offloaded method in a loop paid for both on every call, to get a
    /// stream it used exactly once. The chunked writeback already pools
    /// its streams for exactly this reason; this is the same pool
    /// discipline for the same cost.
    ///
    /// Two dispatches that land on the same pooled stream serialise
    /// against each other. That is not a regression: the transparent
    /// path marshals, launches, and then finalizes — a blocking wait —
    /// before returning to the interpreter, so it never had two launches
    /// in flight to overlap in the first place. Callers that DO want
    /// overlap register their own stream through `stream_create` and
    /// pass its handle, which bypasses this pool entirely.
    dispatch_streams: RwLock<Vec<std::sync::Arc<Stream>>>,
    /// Cursor into [`OffloadCache::dispatch_streams`].
    next_dispatch_stream: std::sync::atomic::AtomicUsize,
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
    chunk_stage_i16: RwLock<Option<std::sync::Arc<cuda_bridge::PinnedHostBuffer<i16>>>>,
    chunk_stage_i8: RwLock<Option<std::sync::Arc<cuda_bridge::PinnedHostBuffer<i8>>>>,
    chunk_stage_u16: RwLock<Option<std::sync::Arc<cuda_bridge::PinnedHostBuffer<u16>>>>,
    /// Per-call-site memo for everything `dispatch_method_from_native_on_stream`
    /// used to re-derive from the three name strings on EVERY dispatch.
    ///
    /// GPULlama3's forward pass makes 453 dispatches per token across 12
    /// distinct kernels, and each one arrived as
    /// `("org/.../CratonKernels", "matmulSplit", "([I[FI[F)V")` and paid, in
    /// order: a `load_class_concurrent` attempt, a class-manager read lock, a
    /// name hash lookup, a LINEAR scan of the class's method table comparing
    /// two strings per entry, and a `lookup_or_compile`. All of that is a
    /// function of the three names alone, and the three names are fixed per
    /// call site.
    ///
    /// Keyed on the name triple rather than on a class id because the names
    /// are what the caller supplies. That makes the memo stale if a class is
    /// ever redefined under the same name -- which is exactly the staleness
    /// `kernels` already has, since it is keyed on `(class_id, method_index)`
    /// and a redefinition would reuse neither.
    dispatch_memo: RwLock<FxHashMap<u64, ResolvedDispatch>>,
    /// Consecutive `below_min_work` refusals per CALL SITE, keyed by
    /// `(caller class id, bytecode pc)`. See [`min_work_giveup_after`].
    min_work_streak: RwLock<FxHashMap<u64, u32>>,
    /// The open graph capture, if any.
    ///
    /// While this is `Some`, `dispatch_method_from_native_on_stream` takes
    /// a LEAN path on the captured stream: resolve, marshal, launch, record
    /// the node. It skips the completion event, the submission-table entry
    /// and the host callback, because all three exist to tell a caller when
    /// a submission finished and during a capture nothing finishes -- the
    /// launches are recorded, not run. Asking the device any of those
    /// questions would also invalidate the capture outright.
    capture: RwLock<Option<GraphCapture>>,
    /// An open argument-update pass; see [`ReplayBind`].
    replay_bind: RwLock<Option<ReplayBind>>,
    /// Class id of `craton/gpu/GpuArray`, resolved once.
    ///
    /// `try_gpu_array_shape` used to answer "is this argument a GpuArray" by
    /// taking the class-manager read lock, cloning the class's name into a
    /// fresh `String`, and comparing it -- per ARGUMENT, per dispatch. At
    /// ~5 arguments across 453 dispatches that is 2,265 lock acquisitions and
    /// 2,265 allocations per token to answer a question that is one integer
    /// compare. `None` means "not looked up yet"; `Some(None)` means the
    /// class is not loaded in this process, which is the ordinary case for a
    /// program that never touches `craton.gpu`.
    gpu_array_class_id: RwLock<Option<Option<crate::classloading::ClassId>>>,
    /// Process/VM Phase 4b async DMA staging pool when arena span host-pinning
    /// is unavailable or `cuda` host registration fails.
    pinned_host_pool: cuda_bridge::pinned_pool::PinnedHostPool,
    /// Process/VM Phase 4b cache of `cuMemHostRegister_v2` arena spans for
    /// zero-copy async H2D/D2H transfers.
    host_pin_cache: cuda_bridge::pinned_pool::HostRegionPinCache,
    /// What every kernel on this cache is lowered for.
    ///
    /// Was `sm: (u32, u32)`, a CUDA compute capability. That is the
    /// right shape for one vendor and dangerously wrong for two: an AMD
    /// `gfx1100` reports major 11, which sails through the `>= (7, 0)`
    /// floor below and would be lowered as `sm_110` -- a target no
    /// driver knows, on a device that does not speak PTX at all.
    /// Probed once at construction; see [`OffloadTarget`].
    target: OffloadTarget,
}

/// Build an [`OffloadTarget`] from an AMD device's probed capabilities.
///
/// Every refusal here is loud and sends the whole cache to
/// [`OffloadTarget::None`], which means "run on the CPU". That is the
/// right answer for all three of them, and each is a thing that would
/// otherwise be silently wrong:
///
/// * a build without `gpu-target-hip` has no HIP renderer linked at
///   all, so there is nothing to lower with;
/// * an unrecognised `gfx` architecture has no known wavefront width,
///   and a reduction folded for the wrong one returns a plausible
///   number that is wrong by half;
/// * a device without 64-bit global atomics cannot run the `long`
///   reduction epilogue, and the emitter produces it unconditionally.
#[cfg(feature = "gpu-offload")]
fn hip_target_for(caps: &cuda_bridge::DeviceCaps) -> OffloadTarget {
    let cuda_bridge::DeviceArch::Hip(hip) = &caps.arch else {
        return OffloadTarget::None;
    };
    #[cfg(not(feature = "gpu-target-hip"))]
    {
        tracing::warn!(
            "gpu offload: device {} is {} ({}) but this binary was built \
             without `gpu-target-hip`, so no kernel can be lowered for it; \
             running on the CPU",
            caps.ordinal,
            caps.name,
            hip.gcn_arch_name,
        );
        OffloadTarget::None
    }
    #[cfg(feature = "gpu-target-hip")]
    {
        let Some(target) =
            jit_hip::HipTarget::from_arch(&hip.gcn_arch_name, hip.fp64, hip.global_int64_atomics)
        else {
            tracing::warn!(
                "gpu offload: device {} reports architecture `{}`, which this \
                 build does not know the wavefront width of; running on the \
                 CPU rather than folding a reduction for a guessed width",
                caps.ordinal,
                hip.gcn_arch_name,
            );
            return OffloadTarget::None;
        };
        if !hip.global_int64_atomics {
            tracing::warn!(
                "gpu offload: device {} ({}) has no 64-bit global atomics, \
                 which the reduction epilogue requires; running on the CPU",
                caps.ordinal,
                hip.gcn_arch_name,
            );
            return OffloadTarget::None;
        }
        tracing::info!(
            "gpu offload: device {} is {} ({}), wavefront {}, lowering HIP C++",
            caps.ordinal,
            caps.name,
            target.gcn_arch_name,
            target.wavefront_size,
        );
        OffloadTarget::Hip(target)
    }
}

/// Build an [`OffloadTarget`] from an OpenCL device's probed capabilities.
#[cfg(feature = "gpu-offload")]
fn opencl_target_for(caps: &cuda_bridge::DeviceCaps) -> OffloadTarget {
    let cuda_bridge::DeviceArch::OpenCl(opencl) = &caps.arch else {
        return OffloadTarget::None;
    };
    #[cfg(not(feature = "gpu-target-opencl"))]
    {
        tracing::warn!(
            "gpu offload: device {} is {} (OpenCL {}.{}) but this binary was built \
             without `gpu-target-opencl`, so no kernel can be lowered for it; \
             running on the CPU",
            caps.ordinal,
            caps.name,
            opencl.opencl_version.0,
            opencl.opencl_version.1,
        );
        OffloadTarget::None
    }
    #[cfg(feature = "gpu-target-opencl")]
    {
        let target = jit_opencl::OpenClTarget {
            opencl_version: opencl.opencl_version,
            fp64: opencl.fp64,
            global_int64_atomics: opencl.global_int64_atomics,
            warp_size: opencl.warp_size,
            device_name: caps.name.clone(),
        };
        tracing::info!(
            "gpu offload: device {} is {} (OpenCL {}.{}), warp {}, lowering OpenCL C",
            caps.ordinal,
            caps.name,
            target.opencl_version.0,
            target.opencl_version.1,
            target.warp_size,
        );
        OffloadTarget::OpenCl(target)
    }
}

/// The architecture this cache lowers for, chosen once from the probed
/// device.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Debug)]
enum OffloadTarget {
    /// PTX for `sm_{0}{1}`, after the driver-ISA clamp.
    ///
    /// `(7, 0)` is the floor: the lowering emits Volta-era PTX
    /// unconditionally, so nothing older can run it.
    Ptx((u32, u32)),
    /// HIP C++ for a `gfx` architecture.
    #[cfg(feature = "gpu-target-hip")]
    Hip(jit_hip::HipTarget),
    /// OpenCL C for an OpenCL device.
    #[cfg(feature = "gpu-target-opencl")]
    OpenCl(jit_opencl::OpenClTarget),
    /// No device, or one this build cannot lower for. Every lookup
    /// returns `Skip` and the method runs on the CPU.
    None,
}

/// Everything a dispatch needs to know about its target method, resolved
/// once per call site instead of once per call.
///
/// See [`OffloadCache::dispatch_memo`] for what this replaces and why the
/// key is the name triple.
#[cfg(feature = "gpu-offload")]
#[derive(Clone)]
struct ResolvedDispatch {
    /// The three names this entry was resolved from, kept so a hash
    /// collision on the memo key is caught rather than dispatched.
    class_name: String,
    method_name: String,
    descriptor: String,
    class_id: crate::classloading::ClassId,
    method_index: u16,
    is_static: bool,
    this_field_names: Vec<String>,
    writes_param_mask: u64,
    return_kind: ParamKind,
    work_bound: jit_cuda::emitter::WorkBound,
    can_fail_after_store: bool,
}

#[cfg(feature = "gpu-offload")]
impl ResolvedDispatch {
    /// The shape the dispatch site destructures. A struct rather than a
    /// bare tuple in the map because a seven-field tuple with two `u16`-ish
    /// members is exactly the kind of thing that gets silently reordered.
    fn into_tuple(
        self,
    ) -> (
        crate::classloading::ClassId,
        u16,
        bool,
        Vec<String>,
        u64,
        ParamKind,
        jit_cuda::emitter::WorkBound,
        bool,
    ) {
        (
            self.class_id,
            self.method_index,
            self.is_static,
            self.this_field_names,
            self.writes_param_mask,
            self.return_kind,
            self.work_bound,
            self.can_fail_after_store,
        )
    }
}

/// An open graph capture on one stream.
#[cfg(feature = "gpu-offload")]
struct GraphCapture {
    /// The stream being captured, by identity. A second `begin` while
    /// this is open is refused rather than interleaved: capture is a
    /// property of a stream, and two of them at once is a caller error that
    /// would otherwise produce two half-graphs.
    ///
    /// The stream itself rather than its Java-visible handle, so the
    /// dispatch path can ask "am I on the captured stream" with an
    /// `Arc::ptr_eq` and nothing has to thread a handle down to it.
    stream: Arc<Stream>,
    /// Dispatches recorded so far, and how many of them the driver
    /// confirmed as nodes. The two differing is the signal that something
    /// was swallowed, which is worth failing on -- a graph with fewer nodes
    /// than the loop had launches replays successfully and does less.
    dispatches: usize,
    confirmed: usize,
    /// A dispatch the capture had to refuse, kept so `end_capture` can say
    /// which one rather than just failing.
    refused: Option<String>,
    /// Every device buffer a captured launch was given, held so it stays
    /// allocated.
    ///
    /// This is the VM's half of the rule the Java API states to callers:
    /// device memory must outlive the graph. A captured dispatch never
    /// finalizes, so its `FinalizeState` -- which owns the failure-flag
    /// and scalar-accumulator buffers, and which carries a GC-critical
    /// guard that must NOT be held for the life of a graph -- is taken
    /// apart here: the guard is dropped and the writebacks are kept, so
    /// the buffers the graph's nodes point at stay alive and the
    /// collector is not blocked.
    pins: Vec<MarshalWriteback>,
    /// Resident-store handles the captured launches write.
    ///
    /// A replay runs the kernels and no writebacks, so these are what
    /// `graph_replay` marks dirty; without it `GpuArray.toHost` answers
    /// with the host mirror from before the replay.
    writes: Vec<u64>,
    /// The one failure-flag cell every launch in this graph shares.
    /// Allocated on the first captured dispatch. See the long note at
    /// its allocation site.
    flag: Option<Arc<cuda_bridge::DeviceBuffer<u64>>>,
    /// The node each captured dispatch became, in dispatch order, with
    /// the kernel it was.
    ///
    /// The kernel identity is carried so a later argument update can
    /// refuse a caller whose sequence has drifted. Updating node `i`
    /// with the arguments of a DIFFERENT kernel is accepted by the
    /// driver and produces a wrong answer, which is the one failure this
    /// whole mechanism must not have.
    nodes: Vec<(cuda_bridge::graph::GraphNode, ClassId, u16)>,
}

/// An open "re-supply the arguments" pass over an already-captured graph.
///
/// The counterpart of [`GraphCapture`] for callers who cannot move their
/// changing values into device memory. The caller re-runs the SAME
/// dispatch sequence; each dispatch, instead of launching, rewrites the
/// arguments of the node it corresponds to, and `end_replay` submits the
/// graph once.
///
/// It is strictly more expensive than replaying a graph that does not
/// change — a driver call per updated node, plus the caller's own
/// per-dispatch work — and strictly cheaper than issuing the launches.
/// It exists so that a caller who cannot change their kernels is not
/// shut out.
#[cfg(feature = "gpu-offload")]
struct ReplayBind {
    /// Stream the graph will be submitted on, by identity, as in
    /// [`GraphCapture::stream`].
    stream: Arc<Stream>,
    graph_handle: u64,
    /// How many dispatches have been consumed. Indexes the graph's node
    /// list, so it must end equal to that list's length.
    next: usize,
    /// The first thing that went wrong, kept so `end_replay` can say
    /// which dispatch rather than just refusing.
    refused: Option<String>,
}

/// Instantiated graphs, by Java-visible handle.
///
/// A separate handle space from submissions and streams, reached through
/// its own `Native.*` entry points, so nothing confuses the three.
#[cfg(feature = "gpu-offload")]
fn graphs() -> &'static RwLock<FxHashMap<u64, InstalledGraph>> {
    static GRAPHS: std::sync::OnceLock<RwLock<FxHashMap<u64, InstalledGraph>>> =
        std::sync::OnceLock::new();
    GRAPHS.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// An instantiated graph and the node count it was built from.
///
/// The count is carried rather than queried because `node_count` lives on
/// the `Graph`, which `instantiate` consumes, and the driver offers no way
/// to ask a `CUgraphExec` how many nodes it holds. A caller that wants to
/// check the graph against its own launch count needs the number after
/// instantiation, so it is kept here.
#[cfg(feature = "gpu-offload")]
struct InstalledGraph {
    exec: Arc<cuda_bridge::graph::GraphExec>,
    node_count: usize,
    /// The device buffers the graph's nodes point at, kept alive for
    /// exactly as long as the graph. Dropped by `graph_release`.
    _pins: Vec<MarshalWriteback>,
    /// Resident handles a replay makes stale, marked dirty per replay.
    writes: Vec<u64>,
    /// The shared failure flag, read once per replay.
    flag: Option<Arc<cuda_bridge::DeviceBuffer<u64>>>,
    /// Per-dispatch node identity; see [`GraphCapture::nodes`].
    nodes: Vec<(cuda_bridge::graph::GraphNode, ClassId, u16)>,
}

#[cfg(feature = "gpu-offload")]
static NEXT_GRAPH_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Hash of a call site's name triple, the memo's key.
#[cfg(feature = "gpu-offload")]
/// Consecutive `below_min_work` refusals after which a CALL SITE is allowed
/// into the invoke cache. `0` never gives up.
///
/// # Why this needs a pc-keyed invoke cache
///
/// A site whose target is offload-eligible is deliberately kept out of the
/// per-call-site invoke cache so a later call with bigger arrays can still
/// offload. The hook that does the keeping is cheap -- 0.48 us per refusal,
/// caller guard included -- but the site then pays the UNCACHED
/// `invokestatic` path on every call, 10.3 us against 0.35 us cached. 96% of
/// the measured "hook overhead" is that, not the hook.
///
/// Giving up on a site therefore pays, but only if a SITE is a thing the
/// cache can name. Under the old `(class, cp index, is_special)` key it was
/// not: two call sites invoking the same method share a constant-pool entry
/// and one cache entry, so promoting the small-array site also promoted the
/// big-array one and it silently stopped offloading -- measured at 1.47 ms
/// against 81.7 us on `GpuHookOverheadBench`, an 18x regression, which is
/// why the first attempt at this was reverted.
///
/// So this is gated on `CRATONVM_INVOKE_CACHE_PC_KEY`. With pc-keying off
/// the promotion never fires, because it would be the same defect again.
#[inline(always)]
fn min_work_giveup_after() -> u32 {
    256
}

fn dispatch_memo_key(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    class_name.hash(&mut h);
    method_name.hash(&mut h);
    descriptor.hash(&mut h);
    h.finish()
}

/// `CRATONVM_GPU_DISPATCH_MEMO=0` restores the per-call re-derivation.
///
/// Default ON. It exists as an A/B lever rather than a supported
/// configuration: the memo has no observable semantics, so the only honest
/// way to price it is to run one binary both ways in the same minutes on a
/// host that will not hold a clock still between two builds.
#[cfg(feature = "gpu-offload")]
#[inline(always)]
fn dispatch_memo_enabled() -> bool {
    true
}

impl OffloadCache {
    /// Construct the cache. Lazily probes the CUDA driver if
    /// `gpu_offload_enabled` is on. A missing driver is non-fatal: the
    /// cache is created with `ctx = None` and every lookup returns
    /// [`LookupOutcome::Skip`].
    pub fn new(config: &VmConfig) -> Self {
        Self::new_for_vm(config, config.gpu_device_ordinal, 0)
    }

    /// Construct the cache for a specific `(device_ordinal, vm_identity)`
    /// pair so per-VM / per-GPU VRAM quotas are enforced.
    pub fn new_for_vm(config: &VmConfig, device_ordinal: u32, vm_identity: u64) -> Self {
        ensure_process_gpu_gc_hooks_bound();
        if config.gpu_vram_soft_quota_bytes > 0 || config.gpu_vram_hard_quota_bytes > 0 {
            cuda_bridge::allocator::set_vm_device_vram_quotas(
                vm_identity,
                device_ordinal,
                config.gpu_vram_soft_quota_bytes,
                config.gpu_vram_hard_quota_bytes,
            );
        }
        if config.gpu_pin_soft_quota_bytes > 0
            || config.gpu_pin_hard_quota_bytes > 0
            || config.gpu_pin_max_spans > 0
        {
            cuda_bridge::pinned_pool::set_vm_host_pin_quota(
                vm_identity,
                cuda_bridge::pinned_pool::HostPinQuota {
                    soft_pinned_bytes: config.gpu_pin_soft_quota_bytes,
                    hard_pinned_bytes: config.gpu_pin_hard_quota_bytes,
                    max_spans: config.gpu_pin_max_spans,
                },
            );
        }
        let ctx = if config.gpu_offload_enabled {
            match DeviceContext::new_for_vm(device_ordinal, vm_identity) {
                Ok(c) => {
                    tracing::info!(
                        "gpu offload: device {} context acquired (vm={})",
                        device_ordinal,
                        vm_identity
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
        // instead of the Volta floor. Floored at sm_70: the lowering
        // emits Volta-era PTX, so a device older than that (or a failed
        // probe) keeps the floor and the driver rejects the module later
        // if it truly cannot run it.
        //
        // AUDIT 2026-09-02: the probe used to be clamped UPWARD ONLY,
        // and `PtxModule::render` wrote a literal `.version 7.5` beside
        // whatever it produced. PTX ISA 7.5 tops out at `sm_87`, so on
        // Ada (`sm_89`), Hopper (`sm_90`) and Blackwell (`sm_100`/
        // `sm_120`) every lowered module named a target its own declared
        // ISA version does not know, `cuModuleLoadData` refused it,
        // `lookup_or_compile` blacklisted the method, and the VM ran
        // every kernel on the CPU — right answers, one `info` line, and
        // a `--gpu` flag that bought a CUDA context and nothing else.
        // `render` now derives `.version` from the target
        // (`jit_cuda::target::isa_for_target`), and the downward clamp
        // below closes the other direction: a driver older than its own
        // GPU cannot parse the ISA that GPU's target requires, and the
        // honest answer there is to run the device as the newest
        // architecture the driver does know rather than to emit a header
        // nothing can load.
        let target = if ctx.is_some() {
            match cuda_bridge::probe_device(config.gpu_device_ordinal) {
                // An AMD device. Nothing below this arm applies: there
                // is no compute capability, no PTX ISA, and nothing to
                // clamp -- a `gfx90a` code object does not run on a
                // `gfx1100` the way an `sm_80` module runs on an
                // `sm_90` device.
                Ok(caps) if matches!(caps.arch, cuda_bridge::DeviceArch::Hip(_)) => {
                    hip_target_for(&caps)
                }
                Ok(caps) if matches!(caps.arch, cuda_bridge::DeviceArch::OpenCl(_)) => {
                    opencl_target_for(&caps)
                }
                Ok(caps) if (caps.compute_major, caps.compute_minor) >= (7, 0) => {
                    let probed = (caps.compute_major, caps.compute_minor);
                    // An unreadable driver version means "do not clamp":
                    // the pre-audit behaviour, which is right whenever we
                    // cannot prove the driver is behind.
                    let target = match cuda_bridge::driver_cuda_version() {
                        Ok(v) => {
                            let driver_isa = jit_cuda::target::max_isa_for_cuda_version(v);
                            let clamped = jit_cuda::target::clamp_target_to_isa(probed, driver_isa);
                            if clamped != probed {
                                tracing::warn!(
                                    "gpu offload: device {} is sm_{}{} but the installed \
                                     driver (CUDA {}.{}) only parses PTX ISA {}.{}; lowering \
                                     for sm_{}{} instead",
                                    caps.ordinal,
                                    probed.0,
                                    probed.1,
                                    v / 1000,
                                    (v % 1000) / 10,
                                    driver_isa.0,
                                    driver_isa.1,
                                    clamped.0,
                                    clamped.1,
                                );
                            }
                            clamped
                        }
                        Err(_) => probed,
                    };
                    let isa = jit_cuda::target::isa_for_target(target.0, target.1);
                    #[allow(clippy::let_and_return)]
                    tracing::info!(
                        "gpu offload: device {} is {} (sm_{}{}), lowering for sm_{}{} \
                         with PTX ISA {}.{}",
                        caps.ordinal,
                        caps.name,
                        probed.0,
                        probed.1,
                        target.0,
                        target.1,
                        isa.0,
                        isa.1,
                    );
                    OffloadTarget::Ptx(target)
                }
                _ => OffloadTarget::Ptx((7, 0)),
            }
        } else {
            OffloadTarget::Ptx((7, 0))
        };
        let pinned_host_pool = match ctx.as_ref() {
            Some(c) => cuda_bridge::pinned_pool::PinnedHostPool::with_context(
                c.clone(),
                cuda_bridge::pinned_pool::DEFAULT_PINNED_POOL_CAP_BYTES,
            ),
            None => cuda_bridge::pinned_pool::PinnedHostPool::with_cap(
                cuda_bridge::pinned_pool::DEFAULT_PINNED_POOL_CAP_BYTES,
            ),
        };
        let host_pin_cache = match ctx.as_ref() {
            Some(c) => cuda_bridge::pinned_pool::HostRegionPinCache::with_context_for_vm(
                c.clone(),
                vm_identity,
            ),
            None => cuda_bridge::pinned_pool::HostRegionPinCache::new_for_vm(vm_identity),
        };
        Self {
            ctx,
            kernels: RwLock::new(FxHashMap::default()),
            blacklist: RwLock::new(FxHashSet::default()),
            print_decisions: config.print_gpu_decisions,
            approx_math: config.gpu_approx_math,
            streams: RwLock::new(FxHashMap::default()),
            next_stream_handle: std::sync::atomic::AtomicU64::new(1),
            dispatch_streams: RwLock::new(Vec::new()),
            next_dispatch_stream: std::sync::atomic::AtomicUsize::new(0),
            chunk_streams: RwLock::new(Vec::new()),
            chunk_events: RwLock::new(Vec::new()),
            builtin_module: RwLock::new(None),
            chunk_stage_i32: RwLock::new(None),
            chunk_stage_i64: RwLock::new(None),
            chunk_stage_f32: RwLock::new(None),
            chunk_stage_f64: RwLock::new(None),
            chunk_stage_i16: RwLock::new(None),
            chunk_stage_i8: RwLock::new(None),
            chunk_stage_u16: RwLock::new(None),
            dispatch_memo: RwLock::new(FxHashMap::default()),
            min_work_streak: RwLock::new(FxHashMap::default()),
            gpu_array_class_id: RwLock::new(None),
            pinned_host_pool,
            host_pin_cache,
            capture: RwLock::new(None),
            replay_bind: RwLock::new(None),
            target,
        }
    }

    /// The memoised resolution for one call site, if it has been resolved
    /// before and the memo is enabled.
    #[cfg(feature = "gpu-offload")]
    fn dispatch_memo_get(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<ResolvedDispatch> {
        if !dispatch_memo_enabled() {
            return None;
        }
        // Keyed by a hash of the triple, not by the triple itself: a
        // `HashMap<(String, String, String), _>` cannot be probed with
        // `(&str, &str, &str)` -- `Borrow` has no tuple impl -- so every
        // LOOKUP would allocate the three strings it is trying to avoid
        // resolving. The names are kept in the value and compared on hit,
        // so a hash collision resolves the slow way rather than dispatching
        // the wrong method.
        let key = dispatch_memo_key(class_name, method_name, descriptor);
        let held = self.dispatch_memo.read();
        match held.get(&key) {
            Some(resolved)
                if resolved.class_name == class_name
                    && resolved.method_name == method_name
                    && resolved.descriptor == descriptor =>
            {
                cratonvm_types::gpu_dispatch_memo_census::note_resolve_hit();
                Some(resolved.clone())
            }
            _ => {
                cratonvm_types::gpu_dispatch_memo_census::note_resolve_miss();
                None
            }
        }
    }

    /// Remember one call site's resolution.
    #[cfg(feature = "gpu-offload")]
    /// Count one `below_min_work` refusal at `site` and say whether the site
    /// should be PROMOTED into the invoke cache.
    ///
    /// Always `false` unless the invoke cache is pc-keyed: without that a
    /// "site" is a method reference, and promoting one caller silently
    /// deoptimises every other caller of the same kernel. See
    /// [`min_work_giveup_after`].
    pub(crate) fn note_site_below_min_work(&self, site: u64) -> bool {
        let limit = min_work_giveup_after();
        if limit == 0 || !cratonvm_classloading::resolution::pc_key_enabled() {
            return false;
        }
        let mut streaks = self.min_work_streak.write();
        let n = streaks.entry(site).or_insert(0);
        *n = n.saturating_add(1);
        *n > limit
    }

    /// Clear `site`'s streak: it just offloaded, so a later small call must
    /// not inherit a count from before.
    pub(crate) fn clear_site_below_min_work(&self, site: u64) {
        if min_work_giveup_after() == 0 || !cratonvm_classloading::resolution::pc_key_enabled() {
            return;
        }
        // A read first: the common case is an empty streak, and taking the
        // write lock on every offloaded dispatch would be a cost on the path
        // this whole feature exists to make fast.
        if self
            .min_work_streak
            .read()
            .get(&site)
            .is_none_or(|n| *n == 0)
        {
            return;
        }
        self.min_work_streak.write().insert(site, 0);
    }

    fn dispatch_memo_put(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        resolved: &ResolvedDispatch,
    ) {
        if !dispatch_memo_enabled() {
            return;
        }
        let key = dispatch_memo_key(class_name, method_name, descriptor);
        self.dispatch_memo.write().insert(key, resolved.clone());
    }

    /// Class id of `craton/gpu/GpuArray` in this process, resolved once.
    ///
    /// `Some(None)` is a real answer -- the class is not loaded -- and is
    /// cached like any other, because a program that never touches
    /// `craton.gpu` would otherwise re-ask on every argument of every
    /// dispatch.
    #[cfg(feature = "gpu-offload")]
    fn gpu_array_class_id(
        &self,
        shared: &crate::vm::SharedVm,
    ) -> Option<crate::classloading::ClassId> {
        if let Some(Some(cached)) = *self.gpu_array_class_id.read() {
            return Some(cached);
        }
        let resolved = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("craton/gpu/GpuArray");
        if let Some(id) = resolved {
            *self.gpu_array_class_id.write() = Some(Some(id));
        }
        resolved
    }

    /// Start recording dispatches on `stream_handle` into a graph.
    #[cfg(feature = "gpu-offload")]
    pub fn graph_begin_capture(&self, stream_handle: u64) -> bool {
        let Some(stream) = self.resolve_stream(stream_handle) else {
            return false;
        };
        let mut held = self.capture.write();
        if held.is_some() {
            tracing::warn!(
                "gpu graph: a capture is already open; refusing to start a second one. \
                 Capture is a property of one stream and two at once would produce \
                 two half-graphs."
            );
            return false;
        }
        // Allocate the shared failure flag BEFORE the capture opens.
        //
        // `cuMemAlloc` is a synchronous driver call, and a synchronous
        // driver call made by the capturing thread while a thread-local
        // capture is open invalidates it. Allocating this lazily on the
        // first captured dispatch -- which is where it is USED -- would
        // therefore fail every capture, and fail it in the way that is
        // hardest to read: `cuStreamEndCapture` returning a null graph
        // with nothing to say about which call did it.
        let flag = match self.device() {
            Some(ctx) => match cuda_bridge::DeviceBuffer::<u64>::zeros(ctx, 1) {
                Ok(buf) => Some(Arc::new(buf)),
                Err(e) => {
                    tracing::warn!("gpu graph: could not allocate the shared failure flag: {e}");
                    return false;
                }
            },
            None => None,
        };
        if let Err(e) = stream.begin_capture(cuda_bridge::graph::CaptureMode::ThreadLocal) {
            tracing::warn!("gpu graph: begin_capture failed: {e}");
            return false;
        }
        *held = Some(GraphCapture {
            stream: stream.clone(),
            dispatches: 0,
            confirmed: 0,
            refused: None,
            pins: Vec::new(),
            writes: Vec::new(),
            flag,
            nodes: Vec::new(),
        });
        true
    }

    /// The failure-flag cell a graph on `stream` owns, if any.
    ///
    /// `None` when this stream is neither being captured nor having a
    /// captured graph's arguments re-supplied, which is the ordinary
    /// case and tells the caller to take a pooled flag as before.
    ///
    /// Both graph paths must answer, and for the same reason. A capture
    /// bakes this pointer into every node. An argument UPDATE rewrites
    /// those same nodes, so if it handed them a pooled flag instead, the
    /// nodes would point at a buffer that goes back to the pool the
    /// moment the pass ends -- handed to an unrelated dispatch, and
    /// written through by the graph on its next replay. The graph's own
    /// flag is the only correct answer in both cases.
    #[cfg(feature = "gpu-offload")]
    pub(crate) fn capture_shared_flag(
        &self,
        stream: &Arc<Stream>,
    ) -> Option<Arc<cuda_bridge::DeviceBuffer<u64>>> {
        if let Some(capture) = self.capture.read().as_ref() {
            if Arc::ptr_eq(&capture.stream, stream) {
                return capture.flag.clone();
            }
        }
        let graph_handle = self.replay_binding_on(stream)?;
        graphs()
            .read()
            .get(&graph_handle)
            .and_then(|g| g.flag.clone())
    }

    /// Stop recording, instantiate, and register the result.
    ///
    /// Answers `0` for every failure, which the Java side turns into a
    /// thrown `GpuException`. An empty or short graph is a failure and not
    /// a small graph: it would instantiate, replay, and quietly do less
    /// than the loop that was captured.
    #[cfg(feature = "gpu-offload")]
    pub fn graph_end_capture(&self, stream_handle: u64) -> u64 {
        let taken = self.capture.write().take();
        let Some(state) = taken else {
            tracing::warn!("gpu graph: end_capture with no capture open");
            return 0;
        };
        let Some(stream) = self.resolve_stream(stream_handle) else {
            return 0;
        };
        if !Arc::ptr_eq(&state.stream, &stream) {
            tracing::warn!(
                "gpu graph: end_capture on stream {stream_handle}, but the capture was \
                 opened on a different one"
            );
            return 0;
        }
        let Some(ctx) = self.device() else { return 0 };
        if let Some(reason) = &state.refused {
            // End the capture anyway so the stream is usable again, then
            // report. Leaving a stream in capture mode would make every
            // later dispatch on it fail in a way that has nothing to do
            // with the caller's next action.
            let _ = stream.end_capture(ctx);
            tracing::warn!("gpu graph: capture refused a dispatch: {reason}");
            return 0;
        }
        let graph = match stream.end_capture(ctx) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("gpu graph: end_capture failed: {e}");
                return 0;
            }
        };
        let nodes = graph.node_count().unwrap_or(0);
        if nodes != state.dispatches || state.confirmed != state.dispatches {
            tracing::warn!(
                "gpu graph: captured {} dispatches but the graph holds {nodes} nodes \
                 ({} confirmed during capture). Refusing rather than handing back \
                 a graph that replays less work than was recorded.",
                state.dispatches,
                state.confirmed,
            );
            return 0;
        }
        let exec = match graph.instantiate() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("gpu graph: instantiate failed: {e}");
                return 0;
            }
        };
        let handle = NEXT_GRAPH_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        graphs().write().insert(
            handle,
            InstalledGraph {
                exec: Arc::new(exec),
                node_count: nodes,
                _pins: state.pins,
                writes: state.writes,
                flag: state.flag,
                nodes: state.nodes,
            },
        );
        tracing::debug!("gpu graph: captured {nodes} nodes as handle {handle}");
        handle
    }

    /// Submit a graph's launches onto `stream_handle`, as one submission.
    ///
    /// Answers a submission handle so the caller awaits a replay with the
    /// same `awaitSubmission` / `releaseSubmission` pair it already uses
    /// for a dispatch. That is not decoration: a replay is asynchronous,
    /// and a caller who reads a result array without waiting reads
    /// whatever was there before — the failure this returns a handle to
    /// make impossible. `0` if the replay was refused.
    ///
    /// The submission carries no `FinalizeState`. Every buffer a captured
    /// graph touches is a resident `GpuArray` — capture refuses anything
    /// else — so there is no host writeback to run and nothing to pin.
    /// The completion event is the whole payload.
    #[cfg(feature = "gpu-offload")]
    pub fn graph_replay(&self, stream_handle: u64, graph_handle: u64) -> u64 {
        let Some(stream) = self.resolve_stream(stream_handle) else {
            return 0;
        };
        let installed = graphs()
            .read()
            .get(&graph_handle)
            .map(|g| (g.exec.clone(), g.writes.clone(), g.flag.clone()));
        let Some((exec, writes, flag)) = installed else {
            tracing::warn!("gpu graph: replay of unknown handle {graph_handle}");
            return 0;
        };
        let Some(ctx) = self.device() else { return 0 };
        // `launch` records the completion event itself, because it also
        // stamps it into the `last_write` slot of every buffer the graph
        // writes -- see its doc comment. Using that same event as the
        // submission's is not a shortcut: two events would mean the
        // submission and the buffers were waiting on different points in
        // the same stream, which is one more thing to get wrong for no
        // benefit.
        let event = match exec.launch(ctx, &stream) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!("gpu graph: replay of handle {graph_handle} failed: {e}");
                return 0;
            }
        };
        // Every array the captured launches write is now ahead of its
        // host mirror. Marking is a bit each; the download happens only
        // if Java asks for one of them.
        for h in &writes {
            device_cache::mark_dirty(*h);
        }

        // The one writeback a replay has: read the shared failure flag
        // once the work completes. `pool_key: None` because the graph
        // still owns the buffer -- returning it would hand a live graph
        // node's target to an unrelated dispatch.
        let finalize = flag.map(|buf| {
            FinalizeState::new(
                vec![MarshalWriteback::FailureFlag {
                    buf,
                    pool_key: None,
                }],
                GcCriticalGuard::acquire(),
            )
        });

        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        register_submission(std::sync::Arc::new(StreamSubmission {
            handle,
            stream: Some(stream),
            stream_handle: Some(stream_handle),
            event: Some(event),
            status: parking_lot::Mutex::new(SubmissionStatus::Running),
            finalize: parking_lot::Mutex::new(finalize),
            ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
            device_done: std::sync::atomic::AtomicBool::new(false),
            active_stream_armed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Open an argument-update pass over `graph_handle`.
    ///
    /// Between this and [`Self::graph_end_replay`] the caller re-issues
    /// the dispatch sequence it captured. Nothing runs: each dispatch
    /// rewrites the arguments of the node it corresponds to.
    #[cfg(feature = "gpu-offload")]
    pub fn graph_begin_replay(&self, stream_handle: u64, graph_handle: u64) -> bool {
        let Some(stream) = self.resolve_stream(stream_handle) else {
            return false;
        };
        if graphs().read().get(&graph_handle).is_none() {
            tracing::warn!("gpu graph: begin_replay on unknown handle {graph_handle}");
            return false;
        }
        if self.capture.read().is_some() {
            tracing::warn!(
                "gpu graph: refusing to open an argument-update pass while a capture is \
                 open; a dispatch cannot be recorded and re-supplied at the same time"
            );
            return false;
        }
        let mut held = self.replay_bind.write();
        if held.is_some() {
            tracing::warn!("gpu graph: an argument-update pass is already open");
            return false;
        }
        *held = Some(ReplayBind {
            stream,
            graph_handle,
            next: 0,
            refused: None,
        });
        true
    }

    /// Close the pass and submit the graph once.
    ///
    /// Refuses — and submits nothing — if the caller's sequence did not
    /// match the captured one. A short sequence would replay some nodes
    /// with this iteration's arguments and the rest with the previous
    /// iteration's, which is a wrong answer rather than a failure.
    #[cfg(feature = "gpu-offload")]
    pub fn graph_end_replay(&self, stream_handle: u64) -> u64 {
        let Some(bind) = self.replay_bind.write().take() else {
            tracing::warn!("gpu graph: end_replay with no argument-update pass open");
            return 0;
        };
        if let Some(reason) = bind.refused {
            tracing::warn!("gpu graph: argument-update pass refused a dispatch: {reason}");
            return 0;
        }
        let expected = graphs()
            .read()
            .get(&bind.graph_handle)
            .map_or(0, |g| g.nodes.len());
        if bind.next != expected {
            tracing::warn!(
                "gpu graph: the argument-update pass supplied {} dispatches but the graph \
                 holds {expected} nodes. Refusing: replaying now would run some nodes with \
                 this pass's arguments and the rest with the previous pass's.",
                bind.next,
            );
            return 0;
        }
        self.graph_replay(stream_handle, bind.graph_handle)
    }

    /// Whether an argument-update pass is open on `stream`, and if so
    /// the graph it is updating.
    #[cfg(feature = "gpu-offload")]
    fn replay_binding_on(&self, stream: &Arc<Stream>) -> Option<u64> {
        let held = self.replay_bind.read();
        let bind = held.as_ref()?;
        Arc::ptr_eq(&bind.stream, stream).then_some(bind.graph_handle)
    }

    /// Rewrite the next node's arguments from this dispatch.
    ///
    /// Answers `Err` with a reason the caller can report. Every failure
    /// is recorded on the bind as well, so `end_replay` refuses even if
    /// the caller ignores the individual dispatch's status.
    #[cfg(feature = "gpu-offload")]
    fn update_next_node(
        &self,
        graph_handle: u64,
        class_id: ClassId,
        method_index: u16,
        args: &cuda_bridge::KernelArgs,
    ) -> Result<(), String> {
        let mut held = self.replay_bind.write();
        let Some(bind) = held.as_mut() else {
            return Err("no argument-update pass is open".to_string());
        };
        let entry = graphs()
            .read()
            .get(&graph_handle)
            .and_then(|g| g.nodes.get(bind.next).copied().map(|n| (g.exec.clone(), n)));
        let Some((exec, (node, want_class, want_method))) = entry else {
            let reason = format!(
                "the update pass reached dispatch {} but the graph holds fewer nodes",
                bind.next
            );
            bind.refused.get_or_insert(reason.clone());
            return Err(reason);
        };
        // The kernel at this position must be the one captured here.
        // Updating a node with another kernel's arguments is something
        // the driver accepts, and it produces a wrong answer.
        if (want_class, want_method) != (class_id, method_index) {
            let reason = format!(
                "dispatch {} is class_id={class_id:?} method={method_index}, but the graph \
                 captured class_id={want_class:?} method={want_method} at that position; \
                 the sequence has drifted from the one that was captured",
                bind.next
            );
            bind.refused.get_or_insert(reason.clone());
            return Err(reason);
        }
        bind.next += 1;
        // Drop the lock before the driver call: nothing below touches
        // the bind, and holding a write lock across FFI serialises
        // nothing useful.
        let at = bind.next - 1;
        drop(held);
        exec.set_kernel_node_args(node, args).map_err(|e| {
            let reason = format!("updating node {at}: {e}");
            if let Some(b) = self.replay_bind.write().as_mut() {
                b.refused.get_or_insert(reason.clone());
            }
            reason
        })
    }

    /// How many nodes a graph holds, or `-1` for an unknown handle.
    ///
    /// A caller compares this with the number of dispatches it issued
    /// between begin and end. They can only differ if something was
    /// dropped, and a graph that replays less work than was recorded is
    /// the one failure mode of this whole mechanism that produces wrong
    /// answers instead of an error, so it is worth making checkable.
    #[cfg(feature = "gpu-offload")]
    pub fn graph_node_count(&self, graph_handle: u64) -> i32 {
        graphs()
            .read()
            .get(&graph_handle)
            .map_or(-1, |g| i32::try_from(g.node_count).unwrap_or(i32::MAX))
    }

    /// Free a graph. Idempotent, and a no-op for an unknown handle --
    /// the same release convention as streams, arrays and submissions.
    ///
    /// Dropping the `GraphExec` is what calls `cuGraphExecDestroy`; the
    /// device memory the graph's nodes point at is not owned here and is
    /// not touched.
    #[cfg(feature = "gpu-offload")]
    pub fn graph_release(&self, graph_handle: u64) {
        let _ = graphs().write().remove(&graph_handle);
    }

    /// Whether a capture is open on `stream_handle`, and the recorder for
    /// it. `None` on every other stream, so an unrelated dispatch during a
    /// capture takes its ordinary path.
    #[cfg(feature = "gpu-offload")]
    fn capturing_on(&self, stream: &Arc<Stream>) -> bool {
        self.capture
            .read()
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(&c.stream, stream))
    }

    /// Record that one dispatch was captured, and take custody of the
    /// device buffers it was given.
    ///
    /// `writebacks` come out of the dispatch's `FinalizeState`, whose
    /// GC-critical guard the caller has already dropped -- see the
    /// `pins` field. Nothing here runs a writeback; they are held only
    /// so the memory the graph's nodes point at stays allocated, and
    /// the resident handles among them are remembered so a replay can
    /// mark them dirty.
    #[cfg(feature = "gpu-offload")]
    fn note_captured(
        &self,
        node: Option<cuda_bridge::graph::GraphNode>,
        kernel: Option<(ClassId, u16)>,
        writebacks: Vec<MarshalWriteback>,
    ) {
        if let Some(c) = self.capture.write().as_mut() {
            c.dispatches += 1;
            if node.is_some() {
                c.confirmed += 1;
            }
            if let (Some(node), Some((class_id, method_index))) = (node, kernel) {
                c.nodes.push((node, class_id, method_index));
            }
            for w in &writebacks {
                if let Some(h) = w.resident_handle() {
                    if !c.writes.contains(&h) {
                        c.writes.push(h);
                    }
                }
            }
            c.pins.extend(writebacks);
        }
    }

    /// Record that a dispatch could not be captured, with the reason.
    /// Keeps only the first: it is the one that explains the rest.
    #[cfg(feature = "gpu-offload")]
    fn note_capture_refused(&self, reason: String) {
        if let Some(c) = self.capture.write().as_mut() {
            if c.refused.is_none() {
                c.refused = Some(reason);
            }
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
        let hip_arch = match &self.target {
            #[cfg(feature = "gpu-target-hip")]
            OffloadTarget::Hip(hip) => Some(hip.gcn_arch_name.as_str()),
            _ => None,
        };
        let module = crate::runtime::kernels::load_for_target(ctx, hip_arch)
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
            // `info!`, not `debug!`: docs/gpu/annotations.md tells users to
            // read this line with `RUST_LOG=gpu.offload=...`, and under
            // `release_max_level_info` a `debug!` is compiled out of every
            // release build, so no RUST_LOG value could ever surface it.
            // Bounded by the number of @GpuExclude-annotated methods, and
            // still below the default WARN filter.
            tracing::info!(
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
        let verdict = analyzer::analyze_with_annotations_pool_and_approx(
            method,
            &method_annotations,
            constant_pool,
            self.approx_math,
        );
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

        // The compute capability the front end lowers for. On a HIP
        // target this is only the PTX side's `sm_XX`, which the HIP
        // renderer ignores -- it reads the instruction list, not the
        // PTX -- but the front end's signature still takes it, so the
        // floor is passed and the architecture comes from
        // `self.target`.
        let (sm_major, sm_minor) = match &self.target {
            OffloadTarget::Ptx(sm) => *sm,
            #[cfg(feature = "gpu-target-hip")]
            OffloadTarget::Hip(_) => (7, 0),
            #[cfg(feature = "gpu-target-opencl")]
            OffloadTarget::OpenCl(_) => (7, 0),
            OffloadTarget::None => {
                // Probed as unusable at construction, and said so then.
                // Blacklist rather than retry: the answer cannot change
                // for the life of this cache.
                self.blacklist.write().insert(key);
                return LookupOutcome::Blacklisted;
            }
        };
        let ptx_module: PtxModule = match jit_cuda::lowering::lower_method_with_pool_and_approx(
            class_name,
            method,
            constant_pool,
            &sig,
            sm_major,
            sm_minor,
            self.approx_math,
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
        let can_fail_after_store = ptx_module.can_fail_after_store();
        // Render for whichever target this cache chose, and hand the
        // bridge the matching image. The two arms are the only place
        // the vendor is visible on this path.
        let ctx = self.ctx.as_ref().expect("ctx presence checked above");
        let module = match &self.target {
            OffloadTarget::Ptx(_) => {
                let ptx_text = ptx_module.render();
                dump_kernel_if_requested(class_name, &method.name, &ptx_text);
                DeviceModule::from_kernel_image(
                    ctx,
                    &cuda_bridge::KernelImage::Ptx(&ptx_text),
                    &[kernel_name.as_str()],
                )
            }
            #[cfg(feature = "gpu-target-hip")]
            OffloadTarget::Hip(hip) => match jit_hip::render_module(&ptx_module, hip) {
                Ok(rendered) => {
                    dump_kernel_if_requested(class_name, &method.name, &rendered.source);
                    DeviceModule::from_kernel_image(
                        ctx,
                        &cuda_bridge::KernelImage::HipSource {
                            source: &rendered.source,
                            offload_arch: &hip.gcn_arch_name,
                        },
                        &[kernel_name.as_str()],
                    )
                }
                Err(e) => {
                    // The front end lowered this method, but not every
                    // instruction it produced has a HIP form yet. That
                    // is a coverage gap, not a defect, and the message
                    // names the instruction so it can be closed --
                    // see `jit-hip`'s golden manifest.
                    tracing::info!(
                        "gpu offload: HIP rendering refused {}.{} ({e}); blacklisting",
                        class_name,
                        &*method.name
                    );
                    self.blacklist.write().insert(key);
                    return LookupOutcome::Blacklisted;
                }
            },
            #[cfg(feature = "gpu-target-opencl")]
            OffloadTarget::OpenCl(opencl) => match jit_opencl::render_module(&ptx_module, opencl) {
                Ok(rendered) => {
                    dump_kernel_if_requested(class_name, &method.name, &rendered.source);
                    DeviceModule::from_kernel_image(
                        ctx,
                        &cuda_bridge::KernelImage::OpenClSource(&rendered.source),
                        &[kernel_name.as_str()],
                    )
                }
                Err(e) => {
                    tracing::info!(
                        "gpu offload: OpenCL rendering refused {}.{} ({e}); blacklisting",
                        class_name,
                        &*method.name
                    );
                    self.blacklist.write().insert(key);
                    return LookupOutcome::Blacklisted;
                }
            },
            OffloadTarget::None => {
                self.blacklist.write().insert(key);
                return LookupOutcome::Blacklisted;
            }
        };
        let module = match module {
            Ok(m) => m,
            Err(e) => {
                tracing::info!(
                    "gpu offload: kernel module load failed for {}.{} ({e}); blacklisting",
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
            can_fail_after_store,
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
///
/// Renamed from `dump_ptx_if_requested` when a second target arrived:
/// on a HIP build the text it writes is HIP C++, and a function called
/// `dump_ptx` writing C++ into a `.ptx` file is a small lie that costs
/// somebody an afternoon. The flag keeps its declared name --
/// `CRATONVM_GPU_DUMP_PTX` is on the four-file flag surface and
/// renaming it is a separate change -- and the extension follows the
/// content.
fn dump_kernel_if_requested(class_name: &str, method_name: &str, kernel_text: &str) {
    // Through the config boundary, not `std::env` directly: this IS a declared
    // flag (`CRATONVM_DBG=gpu-dump-ptx`), so it must be served by the immutable
    // snapshot like every other one. A raw read also made the grouped spelling
    // silently do nothing here.
    let Ok(dir) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) else {
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
    // The extension follows the content: a HIP build writes C++.
    let ext = if kernel_text.starts_with(".version") {
        "ptx"
    } else {
        "hip"
    };
    let path = std::path::Path::new(&dir).join(format!("{safe}.{ext}"));
    if let Err(e) = std::fs::write(&path, kernel_text) {
        tracing::warn!("gpu offload: CRATONVM_GPU_DUMP_PTX write to {path:?} failed: {e}");
    } else {
        tracing::info!(
            "gpu offload: wrote kernel source for {class_name}.{method_name} to {path:?}"
        );
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
fn unpin_freed_native_span(base: usize, len: usize) {
    cratonvm_gc::vm_heap::gpu_coordination::run_matching_span_unpin_hooks(&[(base, len)]);
}

pub(crate) fn ensure_native_span_free_hook_bound() {
    static BOUND: std::sync::Once = std::sync::Once::new();
    BOUND.call_once(|| {
        cratonvm_native_builtins::unsafe_natives_ext::register_native_span_free_hook(
            unpin_freed_native_span,
        );
    });
}

fn vm_heaps_registry() -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<usize, std::sync::Weak<crate::vm::SharedVm>>> {
    static MAP: std::sync::OnceLock<
        parking_lot::RwLock<rustc_hash::FxHashMap<usize, std::sync::Weak<crate::vm::SharedVm>>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

fn release_dropped_array_vram(vm_identity: usize, bytes: usize) {
    let weak = vm_heaps_registry().read().get(&vm_identity).cloned();
    if let Some(shared) = weak.and_then(|w| w.upgrade()) {
        shared
            .mem
            .heap
            .note_external_gpu_retained(-(i64::try_from(bytes).unwrap_or(i64::MAX)), -1);
    }
}

/// Bind `cratonvm_gc`'s VRAM telemetry mirrors and pressure-GC callback into
/// `cuda_bridge::allocator` idempotently.
pub fn ensure_process_gpu_gc_hooks_bound() {
    ensure_native_span_free_hook_bound();
    static CACHE_HOOK_BOUND: std::sync::Once = std::sync::Once::new();
    CACHE_HOOK_BOUND.call_once(|| {
        cratonvm_native_builtins::craton_gpu::register_array_cache_release_hook(
            device_cache::release,
        );
        cratonvm_native_builtins::craton_gpu::register_array_cache_vram_release_hook(
            release_dropped_array_vram,
        );
    });
    let (used, limit) = cratonvm_gc::vm_heap::gpu_coordination::vram_mirror_atomics();
    cuda_bridge::allocator::bind_vram_mirrors(used, limit);
    cuda_bridge::allocator::register_vram_pressure_gc_hook(
        cratonvm_gc::vm_heap::gpu_coordination::collect_garbage_for_gpu_pressure,
    );
    cuda_bridge::allocator::register_vram_soft_watermark_gc_hook(
        cratonvm_gc::vm_heap::gpu_coordination::collect_garbage_for_gpu_pressure,
    );
}

fn trigger_vm_pressure_gc(
    weak_vm: &std::sync::Weak<crate::vm::SharedVm>,
    shutdown: &std::sync::atomic::AtomicBool,
    in_flight: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    use std::sync::atomic::Ordering;
    if shutdown.load(Ordering::Acquire) || REAPER_SHUTDOWN.load(Ordering::Acquire) {
        return false;
    }
    let Some(shared) = weak_vm.upgrade() else {
        return false;
    };
    if in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    struct InFlightGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let guard = InFlightGuard(in_flight.clone());
    let can_join = !crate::native::jni::is_foreign_attached()
        && shared.threads.thread_registry.current_os_tid_claimed() == Some(false);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let (job_tx, job_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
    let job: crate::native::jni::AttachedServiceJob = Box::new(move |vm_shared, thread| {
        crate::runtime::interpreter::gc_and_alloc::force_gc_for_vm(vm_shared, thread);
        let _ = cuda_bridge::allocator::reclaim_all_completed_pools();
        cuda_bridge::allocator::notify_vram_freed();
        let _ = done_tx.send(true);
    });
    if job_tx.send(job).is_err() {
        return false;
    }
    drop(job_tx);
    let shared_for_wait = shared.clone();
    let Ok(handle) = std::thread::Builder::new()
        .name("GpuPressureGc".to_string())
        .spawn(move || {
            let _guard = guard;
            crate::native::jni::run_attached_service_as(
                shared,
                "GpuPressureGc",
                false,
                started_tx,
                job_rx,
            );
        })
    else {
        return false;
    };
    let join_and_wait = move || {
        let _ = handle.join();
        drop(started_rx);
        done_rx.recv().unwrap_or(false)
    };
    if can_join {
        return join_and_wait();
    }
    // Mutator or foreign-attached thread (`can_join == false`): settle this
    // calling thread into a GC-blocked safepoint region before joining
    // `GpuPressureGc` so STW completes cleanly and callers never retry
    // allocation before `force_gc_for_vm` + `reclaim_all_completed_pools()`
    // finish.
    if crate::native::jni::is_foreign_attached() {
        return crate::native::jni::with_foreign_idle_safepoint(&shared_for_wait, join_and_wait);
    }
    crate::vm::vm_exec::with_mutator_pressure_gc_safepoint(&shared_for_wait, join_and_wait)
        .unwrap_or(true)
}

/// Bind process-wide GPU/GC allocator mirrors and register this `SharedVm`'s
/// weak pressure-collection callback.
pub fn register_vm_offload_hooks(shared: &crate::vm::SharedVm) {
    ensure_process_gpu_gc_hooks_bound();
    shared.offload_registry.ensure_vm_hooks(shared);
}

pub struct OffloadCacheRegistry {
    per_device: parking_lot::RwLock<rustc_hash::FxHashMap<u32, std::sync::Arc<OffloadCache>>>,
    vm_identity: std::sync::atomic::AtomicU64,
    registered_vm_identity: std::sync::atomic::AtomicU64,
    pressure_gc_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl OffloadCacheRegistry {
    pub fn new() -> Self {
        Self {
            per_device: parking_lot::RwLock::new(rustc_hash::FxHashMap::default()),
            vm_identity: std::sync::atomic::AtomicU64::new(u64::MAX),
            registered_vm_identity: std::sync::atomic::AtomicU64::new(u64::MAX),
            pressure_gc_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Record the owning VM's `vm_identity` so teardown (`Drop`) can evict
    /// per-VM caches (`device_cache`, `offload_jit_gate::GATE_CACHE`) even if
    /// `ensure_vm_hooks` never ran (or `shared.self_arc` was not yet populated).
    pub(crate) fn note_vm_identity(&self, vm_identity: usize) {
        let vm_id = vm_identity as u64;
        let _ = self.vm_identity.compare_exchange(
            u64::MAX,
            vm_id,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }

    /// Ensure process-wide VRAM mirrors / pressure hook and this VM's weak
    /// pressure collector are registered. Safe to call repeatedly.
    pub fn ensure_vm_hooks(&self, shared: &crate::vm::SharedVm) {
        use std::sync::atomic::Ordering;
        ensure_process_gpu_gc_hooks_bound();
        self.note_vm_identity(shared.vm_identity);
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let vm_id = shared.vm_identity as u64;
        let vm_id = shared.vm_identity as u64;
        shared.mem.heap.gpu_coord().set_owner_vm_id(vm_id);
        if self.registered_vm_identity.load(Ordering::Acquire) != u64::MAX {
            return;
        }
        let Some(weak_vm) = shared.self_arc.read().as_ref().cloned() else {
            return;
        };
        if self
            .registered_vm_identity
            .compare_exchange(u64::MAX, vm_id, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            vm_heaps_registry()
                .write()
                .insert(shared.vm_identity, weak_vm.clone());
            let shutdown = self.shutdown.clone();
            let in_flight = self.pressure_gc_in_flight.clone();
            cratonvm_gc::vm_heap::gpu_coordination::register_vm_pressure_collector(
                vm_id,
                std::sync::Arc::new(move || {
                    trigger_vm_pressure_gc(&weak_vm, &shutdown, &in_flight)
                }),
            );
        }
    }

    /// Return the sorted active CUDA device ordinals initialized in this VM's
    /// cache registry.
    pub fn active_device_ordinals(&self) -> Vec<u32> {
        let mut ordinals: Vec<u32> = self.per_device.read().keys().copied().collect();
        ordinals.sort_unstable();
        ordinals
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
        let vm_id = match self.vm_identity.load(std::sync::atomic::Ordering::Acquire) {
            u64::MAX => 0,
            id => id,
        };
        let cache = std::sync::Arc::new(OffloadCache::new_for_vm(config, device_ordinal, vm_id));
        write.insert(device_ordinal, cache.clone());
        cache
    }

    /// Lookup without constructing. Returns `None` if no cache has
    /// been created for `device_ordinal` yet.
    pub fn get(&self, device_ordinal: u32) -> Option<std::sync::Arc<OffloadCache>> {
        self.per_device.read().get(&device_ordinal).cloned()
    }
}

impl Drop for OffloadCacheRegistry {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.shutdown.store(true, Ordering::Release);
        let registered_vm_id = self.registered_vm_identity.swap(u64::MAX, Ordering::AcqRel);
        if registered_vm_id != u64::MAX {
            cratonvm_gc::vm_heap::gpu_coordination::unregister_vm_pressure_collector(
                registered_vm_id,
            );
        }
        let noted_vm_id = self.vm_identity.swap(u64::MAX, Ordering::AcqRel);
        let evict_id = if registered_vm_id != u64::MAX {
            registered_vm_id
        } else {
            noted_vm_id
        };
        if evict_id != u64::MAX {
            vm_heaps_registry().write().remove(&(evict_id as usize));
            device_cache::clear_vm(evict_id as usize);
            cratonvm_native_builtins::craton_gpu::clear_vm_state(evict_id as usize);
            input_cache::clear_vm_disabled(evict_id as usize);
            crate::runtime::offload_jit_gate::evict_vm(evict_id as usize);
            clear_vm_gpu_sandbox_quotas(evict_id);
            cuda_bridge::allocator::unregister_vm_vram_pressure_gc_hooks(evict_id);
        }
        let caches = self.per_device.read();
        for cache in caches.values() {
            if let Some(ref ctx) = cache.ctx {
                flag_pool::clear(ctx as *const DeviceContext as usize);
            }
            cache.host_pin_cache.clear();
            cache.pinned_host_pool.clear_ready();
        }
    }
}

/// MBean-style combined VRAM, spill-over, and host-pin quota snapshot for one
/// `(vm_identity, device_ordinal)` tenant.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmGpuSandboxStats {
    pub vram: cuda_bridge::allocator::VmGpuMemoryReport,
    pub host_pin: cuda_bridge::pinned_pool::HostPinCacheStats,
}

/// Clear per-VM VRAM and host-pinning quotas for `vm_identity`.
#[cfg(feature = "gpu-offload")]
pub fn clear_vm_gpu_sandbox_quotas(vm_identity: u64) {
    cuda_bridge::allocator::clear_vm_vram_quotas(vm_identity);
    cuda_bridge::pinned_pool::clear_vm_host_pin_quota(vm_identity);
}

/// Snapshot per-VM GPU VRAM, spill-over, and host-pinning stats for
/// `(vm_identity, device_ordinal)`.
#[cfg(feature = "gpu-offload")]
pub fn vm_gpu_sandbox_stats(vm_identity: u64, device_ordinal: u32) -> VmGpuSandboxStats {
    VmGpuSandboxStats {
        vram: cuda_bridge::allocator::vm_gpu_memory_report(vm_identity, device_ordinal),
        host_pin: cuda_bridge::pinned_pool::vm_host_pin_stats(vm_identity),
    }
}

/// Snapshot MBean-style GPU sandbox stats for all known tenant partitions.
#[cfg(feature = "gpu-offload")]
pub fn all_vm_gpu_sandbox_stats() -> Vec<VmGpuSandboxStats> {
    cuda_bridge::allocator::all_vm_gpu_memory_reports()
        .into_iter()
        .map(|vram| VmGpuSandboxStats {
            host_pin: cuda_bridge::pinned_pool::vm_host_pin_stats(vram.vm_identity),
            vram,
        })
        .collect()
}

/// Emit a `craton.GpuMemoryStats` JFR sample for `(vm_identity, device_ordinal)`.
#[cfg(feature = "gpu-offload")]
pub fn emit_gpu_memory_stats_jfr(
    recorder: &mut cratonvm_jfr::FlightRecorder,
    vm_identity: u64,
    device_ordinal: u32,
    timestamp_ns: u64,
) {
    let stats = vm_gpu_sandbox_stats(vm_identity, device_ordinal);
    cratonvm_jfr::builtin::emit_gpu_memory_stats_event(
        recorder,
        &cratonvm_jfr::builtin::GpuMemoryStatsSample {
            vm_identity,
            device_ordinal,
            vram_used_bytes: stats.vram.vram_used_bytes,
            vram_peak_bytes: stats.vram.vram_peak_bytes,
            vram_soft_limit_bytes: stats.vram.vram_soft_limit_bytes,
            vram_hard_limit_bytes: stats.vram.vram_hard_limit_bytes,
            spill_used_bytes: stats.vram.spill_used_bytes,
            pinned_host_bytes: stats.host_pin.pinned_bytes,
        },
        timestamp_ns,
    );
}

/// Emit `craton.GpuMemoryStats` JFR samples for every active GPU device ordinal
/// initialized on `shared` (falling back to `shared.config.gpu_device_ordinal`
/// if no per-device cache has been initialized yet).
#[cfg(feature = "gpu-offload")]
pub fn emit_vm_gpu_memory_stats_jfr(
    recorder: &mut cratonvm_jfr::FlightRecorder,
    shared: &crate::vm::SharedVm,
    timestamp_ns: u64,
) {
    let mut ordinals = shared.offload_registry.active_device_ordinals();
    if ordinals.is_empty() {
        ordinals.push(shared.config.gpu_device_ordinal);
    }
    for ordinal in ordinals {
        emit_gpu_memory_stats_jfr(recorder, shared.vm_identity as u64, ordinal, timestamp_ns);
    }
}

impl Default for OffloadCacheRegistry {
    fn default() -> Self {
        Self::new()
    }
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
    // Phase timing for the transparent door. `timed` is read once; every
    // `mark` below is an `Instant::now()` pair only when it is on. See
    // `cratonvm_types::gpu_offload_phase_census`.
    let timed = cratonvm_types::gpu_offload_phase_census::enabled();
    let entered = std::time::Instant::now();
    let mut mark = entered;
    // A refusal is charged to `gpu_refusal_census`, which is the only
    // instrument that can see it: the dispatch table's `note_call` sits at
    // the point of no return, so every fall-through below is invisible
    // there. `refuse!` stamps the reason and the elapsed total.
    macro_rules! refuse {
        ($reason:expr, $outcome:expr) => {{
            if timed {
                cratonvm_types::gpu_refusal_census::note_refusal($reason);
                cratonvm_types::gpu_refusal_census::add(
                    cratonvm_types::gpu_refusal_census::TOTAL,
                    entered.elapsed().as_nanos() as u64,
                );
            }
            return Ok($outcome);
        }};
    }
    let cm = shared.classes.class_manager.read();
    let class_id = match cm.get_loaded_class_id(class_name) {
        Some(id) => id,
        None => refuse!(0, DispatchOutcome::FallThrough),
    };
    let class = match cm.get_class(class_id) {
        Some(c) => c,
        None => refuse!(0, DispatchOutcome::FallThrough),
    };
    let method_index = match class
        .methods
        .iter()
        .position(|m| &*m.name == method_name && &*m.descriptor == method_descriptor)
    {
        Some(i) => i as u16,
        None => refuse!(1, DispatchOutcome::FallThrough),
    };
    if timed {
        let n = mark.elapsed().as_nanos() as u64;
        cratonvm_types::gpu_offload_phase_census::add(0, n);
        cratonvm_types::gpu_refusal_census::add(0, n);
        mark = std::time::Instant::now();
    }
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
    shared.offload_registry.ensure_vm_hooks(shared);
    let outcome = cache.lookup_or_compile(
        class_id,
        class_name,
        method_index,
        &class.methods[method_index as usize],
        &class.constant_pool,
    );
    drop(cm);
    if timed {
        let n = mark.elapsed().as_nanos() as u64;
        cratonvm_types::gpu_offload_phase_census::add(1, n);
        cratonvm_types::gpu_refusal_census::add(1, n);
        mark = std::time::Instant::now();
    }

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
                if timed {
                    cratonvm_types::gpu_refusal_census::add(2, mark.elapsed().as_nanos() as u64);
                }
                refuse!(3, DispatchOutcome::FallThrough);
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
            if !admission_model_admits(&kernel.signature, runtime_work)
                || (runtime_work as u32) < shared.config.gpu_min_work
            {
                if timed {
                    cratonvm_types::gpu_refusal_census::add(2, mark.elapsed().as_nanos() as u64);
                }
                // Per-call gate, not a property of the method: the next
                // call at this site may pass a larger array, so the site
                // must stay on the slow path where this hook can see it.
                refuse!(4, DispatchOutcome::FallThroughKeepHooked);
            }
            // Marshal args → device, launch the kernel, synchronize, and
            // write kernel-written arrays back into the Java heap (void)
            // or download the accumulator (reduction). This reuses the
            // explicit path's marshal and launch and finalizes here.
            // Any failure leaves the operand stack + locals untouched, so
            // falling through to the CPU body is always safe.
            // Synchronous: the submission is never registered and never
            // watched by the reaper; this call finalizes it and reads the
            // result off it directly. See `Completion::Caller`.
            if timed {
                cratonvm_types::gpu_offload_phase_census::note_call();
                cratonvm_types::gpu_offload_phase_census::add(2, mark.elapsed().as_nanos() as u64);
                mark = std::time::Instant::now();
            }
            let submission =
                dispatch_method_sync(shared, class_name, method_name, method_descriptor, args);
            let dispatch_ns = if timed {
                let n = mark.elapsed().as_nanos() as u64;
                mark = std::time::Instant::now();
                n
            } else {
                0
            };
            let result = finalize_submission(shared, &submission);
            if timed {
                // `finalize_submission` reports its own two phases; what
                // it spent outside them is negligible and lands in the
                // unaccounted row.
                let _ = mark;
                // What `dispatch_method_sync` spent outside the phases it
                // reported from the inside, split prologue/epilogue by
                // the marks it left.
                let inner = DISPATCH_INNER_NS.with(|c| c.replace(0));
                let prologue = DISPATCH_PROLOGUE_NS.with(|c| c.replace(0));
                cratonvm_types::gpu_offload_phase_census::add(6, prologue);
                cratonvm_types::gpu_offload_phase_census::add(
                    7,
                    dispatch_ns.saturating_sub(inner).saturating_sub(prologue),
                );
                cratonvm_types::gpu_offload_phase_census::add(
                    cratonvm_types::gpu_offload_phase_census::TOTAL,
                    entered.elapsed().as_nanos() as u64,
                );
            }
            let submission = Some(submission);
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
        LookupOutcome::Skip | LookupOutcome::Blacklisted => {
            refuse!(2, DispatchOutcome::FallThrough)
        }
    }
}

/// Largest primitive-array argument length among `args`, or 0 if none.
/// Used to gate the transparent offload on real per-element work (the
/// analyzer's `estimated_work` is a fixed placeholder and cannot).
#[cfg(feature = "gpu-offload")]
/// The fitted admission cost model, default on behind
/// `CRATONVM_GPU_ADMIT_MODEL=1`.
///
/// `docs/feature-designs/gpu-offload.md` measured an 80-cell
/// grid (4 element types x 3 intensities x 6-8 sizes) and fitted
///
/// ```text
/// admit  iff   n * (L + c*ops)   >   overhead + k * bytes_moved
///              \____ CPU ____/       \______ GPU _______/
/// ```
///
/// with `bytes_moved = 2*w*n`. On that grid it is right 77/80 (96%)
/// against 47/80 (59%) for the shipped `n >= 4096`, and it never admits
/// a LOSS — all three misses are conservative refusals on `double[]`,
/// whose CPU cost the fit underestimates at high intensity.
///
/// # Why this is off by default
///
/// The constants below were fitted on ONE device (RTX 2060, sm_75). That
/// page's own conclusion is that they "want re-fitting on any device this
/// ships to, which is an argument for measuring them at startup rather
/// than baking them in". Baking them in and defaulting them on would ship
/// this box's numbers to every other box. So this is the predicate, gated,
/// with the measurement work it implies left visible rather than hidden
/// behind a default.
///
/// Returns `true` (admit) when the model is off, so the caller's
/// `--gpu-min-work` check is unchanged in the default configuration.
#[cfg(feature = "gpu-offload")]
fn admission_model_admits(sig: &jit_cuda::KernelSignature, n: usize) -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .as_deref()
            == Some("1")
    });
    if !on || n == 0 {
        return true;
    }
    // Fitted on this box; see the doc comment.
    const OVERHEAD_NS: f64 = 64_152.0;
    const K_NS_PER_BYTE: f64 = 0.136;
    const L_NS: f64 = 1.0;
    /// Per ARITHMETIC BYTECODE, which is not the unit the page's table
    /// prints. Its `ops` axis is the fixture's parameter, and each of
    /// those "ops" is `v = v*a +/- b` — **two** arithmetic bytecodes
    /// (`javap` on `GpuIntensitySweep`: ops=1/4/16 -> 2/8/32). So the
    /// fitted 0.9 ns/op is 0.45 ns per bytecode, which is what
    /// `body_ops` counts.
    ///
    /// Using 0.9 against a raw bytecode count doubles the CPU estimate
    /// and makes the predicate ADMIT LOSSES: replayed over the published
    /// grid it takes 4 of the 32 cells the wrong way, including
    /// `int[]` at n=65536 (measured 0.90x) — reproduced on device before
    /// this constant was corrected.
    const C_NS_PER_ARITH_BYTECODE: f64 = 0.45;

    // Widest element the kernel moves. `bytes_moved` is in + out, which
    // is what the fit used.
    let w = sig
        .param_kinds
        .iter()
        .filter_map(|k| match k {
            jit_cuda::ParamKind::I64Array | jit_cuda::ParamKind::F64Array => Some(8usize),
            jit_cuda::ParamKind::I32Array | jit_cuda::ParamKind::F32Array => Some(4),
            jit_cuda::ParamKind::I16Array => Some(2),
            jit_cuda::ParamKind::I8Array | jit_cuda::ParamKind::BoolArray => Some(1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if w == 0 {
        // No array parameter: `bytes_moved` is not what this model
        // describes. Defer to the scalar threshold.
        return true;
    }
    let n_f = n as f64;
    let ops = f64::from(sig.body_ops);
    let cpu = n_f * (L_NS + C_NS_PER_ARITH_BYTECODE * ops);
    let gpu = OVERHEAD_NS + K_NS_PER_BYTE * (2.0 * w as f64 * n_f);
    cpu > gpu
}

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
mod tests;

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
    /// Device-resident primitive array kept in `device_cache` until
    /// materialized lazily by `futureGetResult` / `GpuFuture.get()` or consumed
    /// directly by a chained GPU stage.
    DeviceArray {
        array_handle: u64,
        elem: cratonvm_native_api::registry::DeviceArrayElem,
        len: usize,
    },
    /// Raw little-endian `i32` bytes copied back from device memory.
    PrimitiveArrayI32 { bytes: Vec<u8> },
    /// Raw little-endian `i64` bytes copied back from device memory.
    PrimitiveArrayI64 { bytes: Vec<u8> },
    /// Raw little-endian `f32` bytes copied back from device memory.
    PrimitiveArrayF32 { bytes: Vec<u8> },
    /// Raw little-endian `f64` bytes copied back from device memory.
    PrimitiveArrayF64 { bytes: Vec<u8> },
}

#[cfg(feature = "gpu-offload")]
impl SerializedResult {
    /// Materialize array bytes from either an eager `PrimitiveArray*` payload
    /// or a device-resident [`Self::DeviceArray`] handle.
    pub fn materialize_array_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::DeviceArray {
                array_handle, len, ..
            } => {
                if *len == 0 {
                    Some(Vec::new())
                } else {
                    device_cache::download_into_bytes(*array_handle)
                }
            }
            Self::PrimitiveArrayI32 { bytes }
            | Self::PrimitiveArrayI64 { bytes }
            | Self::PrimitiveArrayF32 { bytes }
            | Self::PrimitiveArrayF64 { bytes } => Some(bytes.clone()),
            _ => None,
        }
    }
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
    /// Managed Java-visible stream handle from [`OffloadCache::stream_create`]
    /// when this submission was pinned to a managed stream (`None` for
    /// one-shot/pooled streams or pre-launch failures).
    pub stream_handle: Option<u64>,
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
    /// Ephemeral `device_cache` array output handle owned by this submission
    /// (`0` when this submission produces no ephemeral `DeviceArrayOutput`).
    /// Retained until `StreamSubmission::drop` so repeated `future.get()` and
    /// fan-out `thenApplyGpu()` calls from the same `GpuFutureImpl` cannot
    /// prematurely evict the intermediate buffer, while `StreamCleaner` /
    /// `release_submission` deterministically releases it on future GC.
    pub ephemeral_output_handle: std::sync::atomic::AtomicU64,
    /// `true` while this submission is counted in `stream.active_submission_count()`.
    pub active_stream_armed: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "gpu-offload")]
impl StreamSubmission {
    pub(crate) fn arm_active_stream(&self) {
        if let Some(stream) = &self.stream {
            if !self
                .active_stream_armed
                .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                stream.note_submission_start();
            }
        }
    }

    pub(crate) fn disarm_active_stream(&self) {
        if self
            .active_stream_armed
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            if let Some(stream) = &self.stream {
                stream.note_submission_end();
            }
        }
    }
}

#[cfg(feature = "gpu-offload")]
impl Drop for StreamSubmission {
    fn drop(&mut self) {
        self.disarm_active_stream();
        let h = self
            .ephemeral_output_handle
            .swap(0, std::sync::atomic::Ordering::AcqRel);
        if h != 0 {
            device_cache::release_ephemeral_submission_owner(h);
        }
    }
}

#[cfg(feature = "gpu-offload")]
fn release_ephemeral_writebacks(writebacks: &[MarshalWriteback]) {
    for wb in writebacks {
        if let MarshalWriteback::DeviceArrayOutput { array_handle, .. } = wb {
            device_cache::release_ephemeral_submission_owner(*array_handle);
        }
    }
}

/// Phase 7 #1 — payload of work that must run on the first
/// `future.get()` call.
#[cfg(feature = "gpu-offload")]
pub struct FinalizeState {
    /// Writebacks to drain after the event fires (download device
    /// buffers into source Java arrays / resident-store entries).
    pub writebacks: Vec<MarshalWriteback>,
    /// Device buffers read-only inputs / chained args must keep alive across
    /// the asynchronous kernel execution even if `input_cache` or `device_cache`
    /// is invalidated concurrently before the kernel completes.
    pub retained_buffers: Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    /// Phase 4b pinned-span / pooled DMA lifetimes kept alive alongside
    /// `gc_critical` until the kernel + DMA event retires.
    pub dma_guards: Vec<crate::runtime::gpu_marshal::AsyncDmaLifetime>,
    /// The submission's GPU critical-section token, held from dispatch
    /// to writeback. See [`GcCriticalGuard`] for what it declares and
    /// what it no longer does.
    gc_critical: GcCriticalGuard,
}

#[cfg(feature = "gpu-offload")]
impl FinalizeState {
    fn new(writebacks: Vec<MarshalWriteback>, gc_critical: GcCriticalGuard) -> Self {
        Self {
            writebacks,
            retained_buffers: Vec::new(),
            dma_guards: Vec::new(),
            gc_critical,
        }
    }
}

/// A registered, leased, attributed GPU critical-section token.
///
/// AUDIT 2026-09-02. Until this date this was a bare increment of
/// `cratonvm_gc::vm_heap::GPU_CRITICAL_COUNT`, held from dispatch until
/// the writeback drained, and every collector spun on that counter
/// without bound. So: no collection at all for the length of any
/// kernel, a hang for the life of the process if one submission was
/// never finalized, and ZGC — the default collector — not consulting the
/// counter at all, which let its compacting slide run under a
/// device-to-host copy landing in the arena.
///
/// Now it wraps a [`cuda_bridge::critical::CriticalToken`], which the
/// collectors consult through `cratonvm_gc::vm_heap::gpu_coordination`
/// before every cycle. What the token declares is the whole contract:
///
/// * [`Relocation::Forbidden`](cuda_bridge::critical::Relocation::Forbidden)
///   for the two SHORT windows in which the device reads or writes the
///   heap arena in place — the zero-copy upload during marshalling and
///   the download during writeback. The collector waits for these,
///   bounded, and runs non-moving if they outlast its budget.
/// * [`Relocation::KeepAliveOnly`](cuda_bridge::critical::Relocation::KeepAliveOnly)
///   with the writeback targets as keep-alive roots for the LONG window,
///   dispatch to finalize. The collector does not wait for this at all:
///   it keeps the targets alive, moves them if it likes, and the
///   writeback reads their new addresses back through
///   [`GcCriticalGuard::keepalive_addrs`] before it writes. Collection
///   runs during a kernel, which it never could before.
///
/// Every token carries the VM, thread, site and (once known) submission
/// handle, so a collector stall names its holder; and a lease, so an
/// abandoned submission is reaped and its writeback refused
/// ([`GcCriticalGuard::is_revoked`]) instead of wedging the collector.
#[cfg(feature = "gpu-offload")]
pub struct GcCriticalGuard {
    token: cuda_bridge::critical::CriticalToken,
}

#[cfg(feature = "gpu-offload")]
impl GcCriticalGuard {
    /// A keep-alive-only token with no declared roots and no VM
    /// attribution. For paths whose device buffers are all resident
    /// `GpuArray`s (nothing to keep alive in the heap) and for tests.
    pub fn acquire() -> Self {
        Self::acquire_for(
            0,
            None,
            "unattributed",
            cuda_bridge::critical::Relocation::KeepAliveOnly,
            &[],
        )
    }

    /// Acquire with full attribution. `keepalive` are the raw addresses
    /// of the heap objects the holder will write to after the device
    /// finishes; the collector keeps them alive and remaps them.
    pub fn acquire_for(
        vm: u64,
        submission: Option<u64>,
        site: &'static str,
        relocation: cuda_bridge::critical::Relocation,
        keepalive: &[usize],
    ) -> Self {
        if !cratonvm_gc::vm_heap::gpu_coordination::has_gpu_critical_bridge() {
            struct CudaCriticalBridge;
            impl cratonvm_gc::vm_heap::gpu_coordination::GpuCriticalBridge for CudaCriticalBridge {
                fn collector_wait_budget(&self) -> std::time::Duration {
                    cuda_bridge::critical::collector_wait_budget()
                }
                fn record_forced_non_moving_collection(&self) {
                    cuda_bridge::critical::global().record_forced_non_moving_collection();
                }
                fn relocation_forbidden(&self) -> bool {
                    cuda_bridge::critical::global().relocation_forbidden()
                }
                fn relocation_forbidden_for_vm(&self, scope_vm: Option<u64>) -> bool {
                    cuda_bridge::critical::global().relocation_forbidden_for_vm(scope_vm)
                }
                fn outstanding(&self) -> usize {
                    cuda_bridge::critical::global().outstanding() as usize
                }
                fn outstanding_for_vm(&self, scope_vm: Option<u64>) -> usize {
                    cuda_bridge::critical::global().outstanding_for_vm(scope_vm) as usize
                }
                fn wait_for_relocation_clearance(&self, budget: std::time::Duration) -> bool {
                    matches!(
                        cuda_bridge::critical::global().wait_for_relocation_clearance(budget),
                        cuda_bridge::critical::WaitOutcome::Drained { .. }
                    )
                }
                fn wait_for_relocation_clearance_for_vm(
                    &self,
                    scope_vm: Option<u64>,
                    budget: std::time::Duration,
                ) -> bool {
                    matches!(
                        cuda_bridge::critical::global()
                            .wait_for_relocation_clearance_for_vm(scope_vm, budget),
                        cuda_bridge::critical::WaitOutcome::Drained { .. }
                    )
                }
                fn begin_moving_cycle(&self) {
                    cuda_bridge::critical::global().begin_moving_cycle();
                }
                fn try_begin_moving_cycle(&self) -> bool {
                    cuda_bridge::critical::global().try_begin_moving_cycle()
                }
                fn try_begin_moving_cycle_for_vm(&self, scope_vm: Option<u64>) -> bool {
                    cuda_bridge::critical::global().try_begin_moving_cycle_for_vm(scope_vm)
                }
                fn end_moving_cycle(&self) {
                    cuda_bridge::critical::global().end_moving_cycle();
                }
                fn end_moving_cycle_for_vm(&self, scope_vm: Option<u64>) {
                    cuda_bridge::critical::global().end_moving_cycle_for_vm(scope_vm);
                }
                fn outstanding_keepalive_addrs(&self) -> Vec<usize> {
                    cuda_bridge::critical::global().outstanding_keepalive_addrs()
                }
                fn outstanding_keepalive_addrs_for_vm(&self, scope_vm: Option<u64>) -> Vec<usize> {
                    cuda_bridge::critical::global().outstanding_keepalive_addrs_for_vm(scope_vm)
                }
                fn remap_keepalive(&self, remap: &dyn Fn(usize) -> Option<usize>) {
                    cuda_bridge::critical::global().remap_keepalive(remap);
                }
            }
            cratonvm_gc::vm_heap::gpu_coordination::register_gpu_critical_bridge(
                std::sync::Arc::new(CudaCriticalBridge),
            );
        }
        use cuda_bridge::critical::{default_token_lease, global, OwnerId, Registry};
        let owner = OwnerId::current(vm, submission, site);
        let token = Registry::acquire_with(
            global(),
            owner,
            default_token_lease(),
            relocation,
            keepalive,
        );
        if relocation == cuda_bridge::critical::Relocation::Forbidden {
            // Drain any already-published ZGC concurrent relocation plan now that
            // our Forbidden token blocks new concurrent relocation from starting.
            let scoped_drained = if vm != 0 {
                vm_heaps_registry()
                    .read()
                    .get(&(vm as usize))
                    .and_then(|w| w.upgrade())
                    .map(|s| {
                        let _ = s.mem.heap.gpu_coord().enter_relocation_forbidden();
                    })
                    .is_some()
            } else {
                false
            };
            if !scoped_drained {
                let _ = cratonvm_gc::vm_heap::gpu_coordination::enter_relocation_forbidden();
            }
        }
        Self { token }
    }

    /// Whether the registry revoked this token — lease expired, or the VM
    /// shut down. A revoked holder must not write to the heap: its
    /// keep-alive roots were dropped and the objects may be gone.
    pub fn is_revoked(&self) -> bool {
        self.token.is_revoked()
    }

    /// The declared keep-alive addresses as the collector last left them,
    /// in declaration order. Empty once revoked.
    pub fn keepalive_addrs(&self) -> Vec<usize> {
        self.token.keepalive_addrs()
    }
}

/// Who observes a submission's completion.
///
/// AUDIT 2026-09-02. Every dispatch used to register a `cuLaunchHostFunc`
/// callback to learn when its kernel finished. A host function on a
/// stream is not free: CUDA runs it after the work ahead of it and
/// **blocks every launch enqueued behind it on that stream until it
/// returns** — so a chain of kernels on one stream paid a driver-thread
/// round trip between every pair. GPULlama3's 453-launch decode step
/// measured 14 ms of host time against 24 ms of device time, and the
/// graph-capture path, which skips the callback, measured 2.05x. This is
/// the other half of that number. The reaper now polls `cuEventQuery`
/// instead (see [`completion_reaper_loop`]), which asks the device a
/// question without inserting anything into its queue.
///
/// `CRATONVM_GPU_HOST_CALLBACK=1` restores the callback, as the A/B
/// lever for one binary.
#[cfg(feature = "gpu-offload")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// The completion reaper watches the submission and finalizes it on
    /// its own thread when the device reports the event fired. The
    /// explicit async API (`GpuFuture`) wants this: nothing on the Java
    /// side has to call back for the writeback to happen.
    Reaper,
    /// The caller will finalize the submission itself, synchronously,
    /// as the transparent interpreter hook does. Nothing is queued for
    /// the reaper, no callback is registered and the submission is not
    /// entered in the process-wide registry: three lock acquisitions
    /// and a driver call per call site that bought nothing on a path
    /// that blocks on the event before returning.
    Caller,
}

/// `CRATONVM_GPU_HOST_CALLBACK=1` restores the per-launch
/// `cuLaunchHostFunc`. See [`Completion`].
#[cfg(feature = "gpu-offload")]
fn host_callback_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("CRATONVM_GPU_HOST_CALLBACK")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false)
    })
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
        if let Some(stream) = self.streams.write().remove(&handle) {
            if std::sync::Arc::strong_count(&stream) > 1 || stream_has_active_work(&stream, &[]) {
                enqueue_deferred_stream_close(stream);
            }
        }
    }

    /// Block until all work previously enqueued on `handle` finishes on the device.
    pub fn stream_synchronize(&self, handle: u64) -> Result<(), String> {
        let stream = self
            .resolve_stream(handle)
            .ok_or_else(|| format!("unknown or released stream handle {handle}"))?;
        stream
            .synchronize()
            .map_err(|e| format!("streamSynchronize(handle={handle}): {e}"))
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
        let ctx = self.device().ok_or(cuda_bridge::DeviceError::NoDriver)?;
        if let Some(s) = self.chunk_stream_pool(ctx).first() {
            return Ok(std::sync::Arc::clone(s));
        }
        // The pool declines to build itself when stream creation fails;
        // try once directly so the caller gets the driver's own error
        // rather than a bare "no streams".
        Stream::new(ctx).map(std::sync::Arc::new)
    }

    /// A stream for a dispatch that did not name one.
    ///
    /// Built once per device and handed out round-robin. See
    /// [`OffloadCache::dispatch_streams`] for why pooling is safe here
    /// and what it replaces.
    ///
    /// Falls back to a fresh `Stream::new` if the pool cannot be built,
    /// so a driver that refuses to create streams up front still gets
    /// the driver's own error at the point of use rather than a bare
    /// "no streams".
    fn dispatch_stream(
        &self,
        ctx: &cuda_bridge::DeviceContext,
    ) -> Result<std::sync::Arc<Stream>, cuda_bridge::DeviceError> {
        {
            let have = self.dispatch_streams.read();
            if !have.is_empty() {
                let i = self
                    .next_dispatch_stream
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(std::sync::Arc::clone(&have[i % have.len()]));
            }
        }
        {
            let mut slot = self.dispatch_streams.write();
            if slot.is_empty() {
                let mut made = Vec::with_capacity(dispatch_stream_pool_size());
                for _ in 0..dispatch_stream_pool_size() {
                    match Stream::new(ctx) {
                        Ok(s) => made.push(std::sync::Arc::new(s)),
                        Err(e) => {
                            tracing::debug!(
                                "gpu offload: dispatch stream pool unavailable ({e}); \
                                 falling back to a per-dispatch stream"
                            );
                            made.clear();
                            break;
                        }
                    }
                }
                *slot = made;
            }
            if !slot.is_empty() {
                let i = self
                    .next_dispatch_stream
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(std::sync::Arc::clone(&slot[i % slot.len()]));
            }
        }
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
        completion: Completion,
    ) -> std::sync::Arc<StreamSubmission> {
        self.dispatch_async_on_handle(
            stream,
            None,
            class_id,
            method_index,
            args,
            runtime_work,
            finalize_state,
            weak_vm,
            completion,
        )
    }

    pub(crate) fn dispatch_async_on_handle(
        &self,
        stream: std::sync::Arc<Stream>,
        stream_handle: Option<u64>,
        class_id: ClassId,
        method_index: u16,
        args: cuda_bridge::KernelArgs,
        runtime_work: u32,
        mut finalize_state: Option<FinalizeState>,
        weak_vm: std::sync::Weak<crate::vm::SharedVm>,
        completion: Completion,
    ) -> std::sync::Arc<StreamSubmission> {
        let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // `event` is populated only on the success path below, after
        // the launch is queued. Failure paths leave it None.
        let make_with_event =
            |status: SubmissionStatus, event: Option<std::sync::Arc<cuda_bridge::Event>>| {
                std::sync::Arc::new(StreamSubmission {
                    handle,
                    stream: Some(stream.clone()),
                    stream_handle,
                    event,
                    status: parking_lot::Mutex::new(status),
                    finalize: parking_lot::Mutex::new(None),
                    ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
                    device_done: std::sync::atomic::AtomicBool::new(false),
            active_stream_armed: std::sync::atomic::AtomicBool::new(false),
                })
            };
        macro_rules! fail_dispatch {
            ($status:expr) => {{
                if let Some(fs) = finalize_state.take() {
                    if !fs.dma_guards.is_empty() {
                        let _ = stream.synchronize();
                    }
                    release_ephemeral_writebacks(&fs.writebacks);
                    drop(fs);
                }
                make_with_event($status, None)
            }};
        }

        // 1. No-device fast path. The Java layer surfaces this as
        //    `GpuException("no CUDA device …")`.
        let ctx = match self.device() {
            Some(c) => c,
            None => {
                return fail_dispatch!(SubmissionStatus::Failed {
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
                return fail_dispatch!(SubmissionStatus::Failed {
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

        // 3a-update. An argument-update pass rewrites the node this
        //     dispatch corresponds to and runs nothing. Placed before
        //     the capture arm because the two are mutually exclusive and
        //     `graph_begin_replay` refuses to open while a capture is
        //     open, so reaching both is impossible by construction.
        //
        //     Everything above this point still happens — resolving the
        //     kernel, marshalling the arguments — because the arguments
        //     are the whole point. What is skipped is the launch, the
        //     completion event, the submission-table entry and the host
        //     callback, exactly as in a capture.
        if let Some(graph_handle) = self.replay_binding_on(&stream) {
            if let Err(reason) = self.update_next_node(graph_handle, class_id, method_index, &args)
            {
                return fail_dispatch!(SubmissionStatus::Failed {
                    message: format!("graph argument update refused: {reason}"),
                    kind: GpuErrorKind::Launch,
                });
            }
            // As in a capture: nothing ran, so there is nothing to
            // await. `end_replay` hands back the one handle that does
            // name real work.
            if let Some(fs) = finalize_state.take() {
                if !fs.dma_guards.is_empty() {
                    let _ = stream.synchronize();
                }
                release_ephemeral_writebacks(&fs.writebacks);
                drop(fs);
            }
            return make_with_event(
                SubmissionStatus::Completed {
                    result: SerializedResult::Void,
                },
                None,
            );
        }

        // 3a-capture. A capture records launches instead of running them,
        //     so everything below that exists to observe a running
        //     submission is skipped: the chunked writeback (it commits host
        //     memory as chunk events fire, and nothing fires during a
        //     capture), the completion event, the submission-table entry
        //     and the host callback. Two of those would also invalidate the
        //     capture by asking the device a question.
        //
        //     What is refused, and why, is the whole safety story: a replay
        //     re-runs the recorded launches against the device addresses
        //     they were captured with. A resident `GpuArray` keeps its
        //     buffer for the life of the process, so those addresses stay
        //     good. A plain Java array is marshalled into a FRESH device
        //     buffer per dispatch and written back afterwards, so capturing
        //     one would bake in a pointer that is freed before the first
        //     replay, and bake in a writeback into whichever host buffer
        //     the capture happened to use. Both are silent wrong answers,
        //     so a dispatch carrying either is refused and the whole
        //     capture fails.
        if self.capturing_on(&stream) {
            let has_host_writeback = finalize_state.as_ref().is_some_and(|fs| {
                fs.writebacks
                    .iter()
                    .any(MarshalWriteback::is_per_dispatch_host_target)
            });
            if has_host_writeback {
                self.note_capture_refused(format!(
                    "{} marshals a plain Java array; its device buffer is allocated \
                     per dispatch and written back afterwards, so a replay would use a \
                     freed pointer and write into a stale host buffer. Pass a resident \
                     GpuArray instead.",
                    kernel.kernel_name,
                ));
                return fail_dispatch!(SubmissionStatus::Failed {
                    message: "dispatch refused during graph capture: non-resident array \
                              argument"
                        .to_string(),
                    kind: GpuErrorKind::Launch,
                });
            }
            if let Err(e) =
                kernel
                    .module
                    .launch_on_stream(ctx, &kernel.kernel_name, &cfg, args, &stream)
            {
                self.note_capture_refused(format!(
                    "captured launch of {} failed: {e}",
                    kernel.kernel_name
                ));
                return fail_dispatch!(SubmissionStatus::Failed {
                    message: format!("captured launch failed: {e}"),
                    kind: kind_of_device_error(&e),
                });
            }
            // Ask the driver which node that launch became. Counting the
            // confirmations is what lets `end_capture` refuse a graph that
            // holds fewer nodes than the loop had launches -- a graph like
            // that replays successfully and does less, which is the worst
            // way for this to fail.
            let node = stream.capturing_node().ok().flatten();
            // Take the writebacks out of the finalize state and drop the
            // rest of it. The rest is a GC-critical guard, and holding
            // one per captured launch for the life of the graph would
            // stop the collector for the life of the process. The
            // writebacks are what own the device buffers this launch was
            // given, and those must outlive the graph.
            let pins = if let Some(mut fs) = finalize_state.take() {
                if !fs.dma_guards.is_empty() {
                    let _ = stream.synchronize();
                }
                std::mem::take(&mut fs.writebacks)
            } else {
                Vec::new()
            };
            self.note_captured(node, Some((class_id, method_index)), pins);
            // No handle: nothing ran, so there is nothing to await. The
            // Java side documents that a dispatch made during a capture
            // returns no usable submission.
            return make_with_event(
                SubmissionStatus::Completed {
                    result: SerializedResult::Void,
                },
                None,
            );
        }

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
        let mut chunked = false;
        if let Some(fs) = finalize_state.as_mut() {
            if let Some(plain) =
                take_chunkable_writeback(&kernel.signature, &mut fs.writebacks, work)
            {
                let pool = self.chunk_stream_pool(ctx);
                if pool.is_empty() {
                    cratonvm_types::gpu_chunk_census::note_refused();
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
                                cratonvm_types::gpu_chunk_census::note_taken(chunks.len() as u64);
                                for c in chunks {
                                    if let Err(e) = stream.wait_event(&c.done) {
                                        return fail_dispatch!(SubmissionStatus::Failed {
                                            message: format!(
                                                "chunked join wait (lo={}): {e}",
                                                c.lo
                                            ),
                                            kind: GpuErrorKind::Launch,
                                        });
                                    }
                                }
                            }
                            // The chunked writeback's STAGING WAITS run before
                            // the failure flag, so they overlap with the GPU
                            // work still outstanding — that is the whole
                            // point. Its commit into the Java array runs
                            // after the flag, like every other array
                            // writeback. `finalize_submission` splits the two
                            // (`MarshalWriteback::sync_chunks`); the position
                            // in this vector no longer decides the order, and
                            // the `insert(0, ..)` is kept only so the chunked
                            // entry stays easy to find while debugging.
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
                            return fail_dispatch!(SubmissionStatus::Failed {
                                message: format!("chunked dispatch failed: {msg}"),
                                kind: GpuErrorKind::Launch,
                            });
                        }
                    }
                }
            }
        }

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
                return fail_dispatch!(SubmissionStatus::Failed {
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
                return fail_dispatch!(SubmissionStatus::Failed {
                    message: format!("Event::new after launch: {e}"),
                    kind: kind_of_device_error(&e),
                });
            }
        };
        if let Err(e) = stream.record_event(&event) {
            return fail_dispatch!(SubmissionStatus::Failed {
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
        let taken_finalize = finalize_state.take();
        if let Some(ref fs) = taken_finalize {
            for wb in &fs.writebacks {
                if let MarshalWriteback::DeviceArrayOutput { array_handle, .. } = wb {
                    submission
                        .ephemeral_output_handle
                        .store(*array_handle, std::sync::atomic::Ordering::Release);
                    break;
                }
            }
        }
        *submission.finalize.lock() = taken_finalize;

        // 7. Hand the submission to whoever observes its completion.
        //
        //    A synchronous caller finalizes it itself, right after this
        //    returns, and needs nothing else — see `Completion::Caller`.
        //
        //    For the async API the completion reaper watches it. By
        //    default the reaper POLLS the completion event: `cuEventQuery`
        //    asks the device a question without enqueueing anything, so
        //    the next launch on this stream is not held behind a host
        //    round trip the way a `cuLaunchHostFunc` holds it (see
        //    `Completion`). The callback is kept behind
        //    `CRATONVM_GPU_HOST_CALLBACK=1` as the A/B lever.
        //
        //    Either way the submission `Arc` moves onto the reaper queue
        //    rather than being dropped on a driver thread: the last
        //    `Arc<StreamSubmission>` to go drops a `cuda_bridge::Stream`,
        //    whose destructor is a driver call, and a `cuLaunchHostFunc`
        //    callback is the one place that is forbidden — verified on an
        //    RTX 2060 (2026-08-03) as `CUDA_ERROR_NOT_PERMITTED` out of
        //    `CudaStream::drop` on every synchronous dispatch.
        if completion == Completion::Caller {
            return submission;
        }
        ensure_completion_reaper_started();
        if host_callback_enabled() {
            let cb_submission = submission.clone();
            let cb_weak_vm = weak_vm;
            if let Err(e) = stream.add_host_callback(Box::new(move || {
                cb_submission
                    .device_done
                    .store(true, std::sync::atomic::Ordering::Release);
                let cb_handle = cb_submission.handle;
                // Transfers ownership of `cb_submission` into the reaper
                // queue — see above. Do not reintroduce a local
                // `Arc<StreamSubmission>`/`Arc<Stream>` drop in this
                // closure; that is precisely what re-enters the CUDA
                // driver from a host callback.
                enqueue_completion(cb_handle, cb_weak_vm, cb_submission);
            })) {
                tracing::debug!(
                    "gpu offload: add_host_callback registration failed ({e}); \
                     the reaper will poll the event for handle={handle}",
                );
                enqueue_completion(handle, std::sync::Weak::default(), submission.clone());
            }
        } else {
            enqueue_completion(handle, weak_vm, submission.clone());
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
    shared.offload_registry.ensure_vm_hooks(shared);

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
    let c_buf = match resident_f32(ctx, c_handle, c_elems, "C", shared.vm_identity) {
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
            let a = match resident_f32(ctx, a_handle, a_elems, "A", shared.vm_identity) {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(Some(stream), kind_of_device_message(&e), e)
                }
            };
            let b = match resident_f32(ctx, b_handle, b_elems, "B", shared.vm_identity) {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(Some(stream), kind_of_device_message(&e), e)
                }
            };
            let args = kernels::gemm_args(&*a, &*b, &*c_buf, m, n, k, a_strides, b_strides);
            // The Arcs must outlive the launch; the writeback below holds
            // one for C, and these two keep A and B alive across it.
            let _keep = (a, b);
            args
        }
        kernels::GemmKind::F16 => {
            let a = match resident_i16(ctx, a_handle, a_elems, "A", shared.vm_identity) {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(Some(stream), kind_of_device_message(&e), e)
                }
            };
            let b = match resident_i16(ctx, b_handle, b_elems, "B", shared.vm_identity) {
                Ok(b) => b,
                Err(e) => {
                    return record_failed_submission(Some(stream), kind_of_device_message(&e), e)
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
    // A keep-alive token for the submission's life. Every buffer here is
    // a resident `GpuArray`, so there is no heap object to keep alive and
    // nothing to remap; the token exists so the submission is attributed
    // and leased like every other.
    let gc_guard = GcCriticalGuard::acquire_for(
        shared.vm_identity as u64,
        None,
        "dispatch_gemm",
        cuda_bridge::critical::Relocation::KeepAliveOnly,
        &[],
    );

    let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let submission = std::sync::Arc::new(StreamSubmission {
        handle,
        stream: Some(stream),
        stream_handle,
        event: Some(event),
        status: parking_lot::Mutex::new(SubmissionStatus::Running),
        finalize: parking_lot::Mutex::new(Some(FinalizeState::new(writebacks, gc_guard))),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
            active_stream_armed: std::sync::atomic::AtomicBool::new(false),
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
    vm_identity: usize,
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
    device_cache::put_f32(handle, arc.clone(), vm_identity);
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
    vm_identity: usize,
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
    device_cache::put_i16(handle, arc.clone(), vm_identity);
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
    if sub.stream.is_some() && matches!(&*sub.status.lock(), SubmissionStatus::Running) {
        sub.arm_active_stream();
    }
    let h = sub.handle;
    let live = {
        let mut table = submissions().write();
        table.insert(h, sub);
        table.len()
    };
    cratonvm_types::gpu_submission_census::note_register(live as u64);
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
///
/// # NO PRODUCTION CALLER since 2026-09-02 — and do not "fix" that by
/// deleting this
///
/// The `submissions().write().remove(&handle)` below is the **only** remove
/// from the registry, against exactly one insert in
/// [`register_submission`]. `b6133b92d` moved the synchronous JIT-caller
/// path onto `dispatch_method_sync`, which never registers a submission and
/// therefore has nothing to release — correct in itself, and it happened to
/// delete the last call to this function. So today **nothing drains the map**
/// and every async submission leaks its entry, its CUDA stream and its event
/// for the life of the process.
///
/// Putting the call back in the reaper or at the end of
/// [`finalize_submission`] does not work: both run before or independently of
/// the Java side reading the result, and `GpuFuture.get()` resolves through
/// [`lookup_submission`], which answers `None` for a released handle. The old
/// synchronous caller only got away with it by holding its own `Arc` across
/// the release, which does not generalise to a handle Java still owns.
///
/// The drain has to belong to whoever owns the handle's lifetime —
/// a `GpuExecutor.releaseSubmission` native (the API the overflow warning in
/// [`register_submission`] already tells callers to use, and which
/// `bench-gpu/GpuAsyncChainBench.java` already calls, and which is NOT
/// registered in `native-builtins/src/craton_gpu.rs`), or an executor-close
/// path. See
/// `docs/feature-designs/gpu-offload.md`, and
/// `vm/tests/no_test_only_public_api.rs`'s baseline note, which carries the
/// resulting +1 rather than hiding it.
#[cfg(feature = "gpu-offload")]
pub fn release_submission(handle: u64) {
    let removed = submissions().write().remove(&handle);
    if let Some(sub) = removed {
        cratonvm_types::gpu_submission_census::note_release();
        // Drop `sub` outside the `submissions()` write lock so
        // `StreamSubmission::drop` (releasing `ephemeral_output_handle` in
        // `device_cache` and potentially freeing device/stream resources)
        // never runs under the registry lock.
        drop(sub);
    }
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
enum ReaperWork {
    Submission(
        u64,
        std::sync::Weak<crate::vm::SharedVm>,
        std::sync::Arc<StreamSubmission>,
    ),
    DeferredStreamClose(std::sync::Arc<Stream>),
}

#[cfg(feature = "gpu-offload")]
static REAPER_QUEUE: parking_lot::Mutex<std::collections::VecDeque<ReaperWork>> =
    parking_lot::Mutex::new(std::collections::VecDeque::new());
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

/// Body of the completion reaper thread.
///
/// Parks on `REAPER_WAKE` while it has nothing to watch. Once a
/// submission is queued it POLLS: each pass asks every watched
/// submission whether its device work has been observed complete —
/// `device_done` (set by the optional host callback), or the completion
/// event's `cuEventQuery` — and finalizes the ones that have, running
/// the same [`finalize_submission`] work `get()` would have, off the
/// mutator and with no Java thread involved.
///
/// AUDIT 2026-09-02: polling replaced the per-launch `cuLaunchHostFunc`
/// as the default because a host function blocks the launches queued
/// behind it on its stream; see [`Completion`]. A `cuEventQuery` costs a
/// microsecond and touches no queue. Between passes with nothing
/// finished the thread yields a few times and then sleeps, capped at
/// 200 us, so a kernel that takes milliseconds is queried a few thousand
/// times rather than continuously and a kernel that takes microseconds
/// is noticed within a few of them.
///
/// The submission `Arc` stays bound to the entry for as long as it is
/// watched. If it is the last strong `Arc<StreamSubmission>` in the
/// process, the drop — and the `cuda_bridge::Stream` teardown it can
/// transitively trigger — happens here, on an ordinary thread, which is
/// the point: see `REAPER_QUEUE`'s doc comment.
#[cfg(feature = "gpu-offload")]
fn stream_has_active_work(
    stream: &std::sync::Arc<Stream>,
    watched: &[(
        u64,
        std::sync::Weak<crate::vm::SharedVm>,
        std::sync::Arc<StreamSubmission>,
    )],
) -> bool {
    stream.active_submission_count() > 0
        || watched.iter().any(|(_, _, sub)| {
            sub.stream
                .as_ref()
                .is_some_and(|s| std::sync::Arc::ptr_eq(s, stream))
        })
}

#[cfg(feature = "gpu-offload")]
fn drain_ready_deferred_streams(
    deferred_streams: &mut Vec<std::sync::Arc<Stream>>,
    watched: &[(
        u64,
        std::sync::Weak<crate::vm::SharedVm>,
        std::sync::Arc<StreamSubmission>,
    )],
) -> bool {
    let before = deferred_streams.len();
    deferred_streams.retain(|stream| stream_has_active_work(stream, watched));
    deferred_streams.len() != before
}

#[cfg(feature = "gpu-offload")]
fn enqueue_deferred_stream_close(stream: std::sync::Arc<Stream>) {
    ensure_completion_reaper_started();
    REAPER_QUEUE
        .lock()
        .push_back(ReaperWork::DeferredStreamClose(stream));
    REAPER_WAKE.notify_one();
}

#[cfg(feature = "gpu-offload")]
fn completion_reaper_loop() {
    type Watched = (
        u64,
        std::sync::Weak<crate::vm::SharedVm>,
        std::sync::Arc<StreamSubmission>,
    );
    let mut watched: Vec<Watched> = Vec::new();
    let mut deferred_streams: Vec<std::sync::Arc<Stream>> = Vec::new();
    let mut quiet_passes: u32 = 0;
    loop {
        {
            let mut queue = REAPER_QUEUE.lock();
            if watched.is_empty() && deferred_streams.is_empty() {
                while queue.is_empty() {
                    if REAPER_SHUTDOWN.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }
                    // `Condvar::wait` atomically releases `queue` while
                    // parked and re-acquires it on wake, so a `notify_one`
                    // from `enqueue_completion` landing between the
                    // empty-check above and this wait is never missed.
                    REAPER_WAKE.wait(&mut queue);
                }
            }
            for item in queue.drain(..) {
                match item {
                    ReaperWork::Submission(h, w, sub) => watched.push((h, w, sub)),
                    ReaperWork::DeferredStreamClose(stream) => deferred_streams.push(stream),
                }
            }
        }
        if REAPER_SHUTDOWN.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let mut progressed = false;
        watched.retain(|(_, weak_vm, sub)| {
            if !submission_observed_complete(sub) {
                return true;
            }
            finalize_enqueued_handle(weak_vm, sub);
            progressed = true;
            false
        });
        if cuda_bridge::allocator::reclaim_all_completed_pools() > 0 {
            progressed = true;
        }
        if drain_ready_deferred_streams(&mut deferred_streams, &watched) {
            progressed = true;
        }
        if watched.is_empty() && deferred_streams.is_empty() {
            quiet_passes = 0;
            continue;
        }
        quiet_passes = if progressed {
            0
        } else {
            quiet_passes.saturating_add(1)
        };
        if quiet_passes < 8 {
            std::thread::yield_now();
        } else {
            let us = (20 * u64::from(quiet_passes)).min(200);
            std::thread::sleep(std::time::Duration::from_micros(us));
        }
    }
}

/// Whether the reaper may finalize `sub` now without blocking.
///
/// True for a submission already terminal (finalize is then a no-op
/// that drops the entry), for one whose host callback flagged
/// `device_done`, for one whose completion event has fired, and for one
/// that has no event at all — a pre-launch failure, or a test fixture —
/// where there is nothing on the device to wait for.
#[cfg(feature = "gpu-offload")]
fn submission_observed_complete(sub: &StreamSubmission) -> bool {
    if !matches!(&*sub.status.lock(), SubmissionStatus::Running) {
        return true;
    }
    if sub.device_done.load(std::sync::atomic::Ordering::Acquire) {
        return true;
    }
    match sub.event.as_ref() {
        None => true,
        Some(ev) => match ev.query() {
            Ok(done) => done,
            // A query that errors will error again from
            // `finalize_submission`, which records the failure on the
            // status; letting it through is how that gets reported.
            Err(_) => true,
        },
    }
}

/// Finalize one reaper-queued submission: upgrade `weak_vm` and, if that
/// succeeds, run [`finalize_submission`]. A failed upgrade (VM torn
/// down, or `self_arc` was never populated) is a silent no-op — the
/// submission is left for the poll-based path (`poll_submission_status`,
/// `get()`) to finalize later, exactly as it always has. Keeping the
/// caller (`completion_reaper_loop`) draining rather than exiting on a
/// failed upgrade avoids leaking the thread's role as the queue's sole
/// consumer for as long as the process runs multiple short-lived VMs
/// (tests).
///
/// Takes the submission itself rather than looking its handle up: the
/// reaper holds the `Arc`, and a `Completion::Caller` submission is
/// never in the registry at all.
///
/// Factored out of `completion_reaper_loop`'s body so both the
/// VM-alive and VM-torn-down paths are unit-testable directly,
/// without spawning a thread or touching the process-global reaper
/// statics (which, being `Once`-guarded singletons, can only be
/// exercised by one test in the whole binary — see
/// `reaper_finalizes_submission_without_any_poll_call`).
#[cfg(feature = "gpu-offload")]
fn finalize_enqueued_handle(
    weak_vm: &std::sync::Weak<crate::vm::SharedVm>,
    sub: &std::sync::Arc<StreamSubmission>,
) {
    let Some(shared) = weak_vm.upgrade() else {
        return;
    };
    let _ = finalize_submission(&shared, sub);}

/// Push `(handle, weak_vm, submission)` onto the reaper's work queue
/// and wake it. `weak_vm` travels with the handle rather than being
/// fixed once for the reaper thread's whole life — see the doc comment
/// on `REAPER_QUEUE` for why that distinction matters. Called from
/// `dispatch_async` directly, or from the `cuLaunchHostFunc` callback
/// when `CRATONVM_GPU_HOST_CALLBACK=1` restores one, so this
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
    REAPER_QUEUE
        .lock()
        .push_back(ReaperWork::Submission(handle, weak_vm, submission));
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
        // A kernel lowered for the other vendor is a compilation-stage
        // fault, not a launch one: the image never reached a device.
        // Deliberately NOT folded into `NoDriver` — that would report a
        // target-selection bug as an absent GPU and send the VM quietly
        // down its correct-but-slow CPU path.
        DeviceError::UnsupportedImage(_) => GpuErrorKind::Compile,
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

/// A submission that failed before anything was launched, not yet in
/// the registry. [`record_failed_submission`] registers one for the
/// handle-returning entry points; the synchronous dispatch path hands
/// it straight to its caller.
#[cfg(feature = "gpu-offload")]
fn failed_submission(
    stream: Option<std::sync::Arc<Stream>>,
    kind: GpuErrorKind,
    message: String,
) -> std::sync::Arc<StreamSubmission> {
    // Every explicit-dispatch failure ends here, and until this line
    // existed none of them said anything: the reason was stored on the
    // submission and only ever surfaced if the Java side successfully
    // called back for it. `--print-gpu-decisions` turns this on along
    // with the analyzer verdicts.
    tracing::warn!("gpu offload: submission failed — {message}");
    let handle = NEXT_SUBMISSION_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::sync::Arc::new(StreamSubmission {
        handle,
        stream,
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Failed { message, kind }),
        finalize: parking_lot::Mutex::new(None),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
            active_stream_armed: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Resolve the managed stream handle associated with `submission_handle`, if any.
#[cfg(feature = "gpu-offload")]
pub fn submission_stream_handle(submission_handle: u64) -> Option<u64> {
    if submission_handle == 0 {
        return None;
    }
    lookup_submission(submission_handle).and_then(|sub| sub.stream_handle)
}

/// Estimate off-heap VRAM (bytes) retained by an active or completed GPU submission.
#[cfg(feature = "gpu-offload")]
pub fn submission_retained_vram(submission_handle: u64) -> u64 {
    if submission_handle == 0 {
        return 0;
    }
    let Some(sub) = lookup_submission(submission_handle) else {
        return 0;
    };
    let ephem_h = sub
        .ephemeral_output_handle
        .load(std::sync::atomic::Ordering::Acquire);
    if ephem_h != 0 {
        if let Some(buf) = device_cache::get_cached(ephem_h) {
            return buf.byte_len();
        }
    }
    let finalize_guard = sub.finalize.lock();
    if let Some(fin) = finalize_guard.as_ref() {
        let mut total = 0u64;
        for wb in &fin.writebacks {
            if let MarshalWriteback::DeviceArrayOutput { elem, len, .. } = wb {
                let elem_bytes = match elem {
                    cratonvm_native_api::registry::DeviceArrayElem::I32
                    | cratonvm_native_api::registry::DeviceArrayElem::F32 => 4u64,
                    cratonvm_native_api::registry::DeviceArrayElem::I64
                    | cratonvm_native_api::registry::DeviceArrayElem::F64 => 8u64,
                };
                total = total.saturating_add((*len as u64).saturating_mul(elem_bytes));
            }
        }
        return total;
    }
    0
}

#[cfg(feature = "gpu-offload")]
fn record_failed_submission(
    stream: Option<std::sync::Arc<Stream>>,
    kind: GpuErrorKind,
    message: String,
) -> u64 {
    register_submission(failed_submission(stream, kind, message))
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
    let submission = dispatch_method_inner(
        shared,
        class_name,
        method_name,
        descriptor,
        java_args,
        stream_handle,
        Completion::Reaper,
    );
    // Register the submission and return its handle. The Java side wraps
    // this handle in `GpuFutureImpl`.
    register_submission(submission)
}

/// The transparent interpreter hook's dispatch: the same marshal and
/// launch as [`dispatch_method_from_native`], handed back as the
/// submission itself for the caller to finalize synchronously.
///
/// AUDIT 2026-09-02. `try_dispatch` used to go through the handle API —
/// register in the process-wide table, register a `cuLaunchHostFunc`
/// callback, queue for the reaper, look the handle back up, finalize,
/// release — for a call that blocks on the completion event before it
/// returns and so never needed any of it. See [`Completion::Caller`].
#[cfg(feature = "gpu-offload")]
pub fn dispatch_method_sync(
    shared: &crate::vm::SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[cratonvm_types::Value],
) -> std::sync::Arc<StreamSubmission> {
    dispatch_method_inner(
        shared,
        class_name,
        method_name,
        descriptor,
        java_args,
        None,
        Completion::Caller,
    )
}

#[cfg(feature = "gpu-offload")]
/// Nanoseconds `dispatch_method_inner` accounted to named phases on this
/// thread, for the caller to subtract from its own measurement of the
/// whole call.
///
/// A thread-local rather than a return value because `dispatch_method_inner`
/// has eight call sites and returns a submission; threading a timing tuple
/// through all of them would put the instrument in every signature. It is
/// only ever written under `enabled()`, and the caller takes it with a
/// `replace(0)` so a dispatch that did not report leaves nothing behind for
/// the next one to absorb.
#[cfg(feature = "gpu-offload")]
thread_local! {
    static DISPATCH_INNER_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Nanoseconds `dispatch_method_inner` spent BEFORE its device work:
    /// the cache handle, the second class+method resolve, and the memo.
    static DISPATCH_PROLOGUE_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(feature = "gpu-offload")]
enum StreamTarget {
    Handle(Option<u64>),
    Direct {
        stream: std::sync::Arc<Stream>,
        stream_handle: Option<u64>,
    },
}

#[cfg(feature = "gpu-offload")]
#[derive(Clone)]
struct ChainedDeviceArrayArg {
    array_handle: u64,
    buf: device_cache::CachedBuffer,
    len: usize,
    /// `true` when cloned from a resident `GpuArray` entry (needs `Resident*`
    /// dirty-mark writeback); `false` when forwarded from an ephemeral
    /// predecessor `DeviceArrayOutput`.
    is_resident: bool,
}

#[cfg(feature = "gpu-offload")]
struct EphemeralPinGuard(Vec<u64>);

#[cfg(feature = "gpu-offload")]
impl EphemeralPinGuard {
    fn track(&mut self, handle: u64) {
        if device_cache::is_ephemeral_handle(handle) {
            self.0.push(handle);
        }
    }
}

#[cfg(feature = "gpu-offload")]
impl Drop for EphemeralPinGuard {
    fn drop(&mut self) {
        for handle in self.0.drain(..) {
            release_ephemeral_array_after_enqueue(handle);
        }
    }
}

#[cfg(feature = "gpu-offload")]
enum ChainPredecessorValue {
    None,
    DeviceArray(ChainedDeviceArrayArg),
    Scalar(cratonvm_types::Value),
}

#[cfg(feature = "gpu-offload")]
fn read_gpu_future_handle(
    shared: &crate::vm::SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<u64> {
    let cid = shared.mem.heap.class_id_of(obj_ref);
    let slot = {
        let cm = shared.classes.class_manager.read();
        let cls = cm.get_class(cid)?;
        if cls.name.as_ref() != "craton/gpu/internal/GpuFutureImpl" {
            return None;
        }
        let mut current = Some(cid);
        let mut found = None;
        while let Some(cur_cid) = current {
            let Some(c) = cm.get_class(cur_cid) else {
                break;
            };
            if let Some((idx, _)) = c.find_own_field("handle") {
                found = Some(idx);
                break;
            }
            current = c.superclass;
        }
        found?
    };
    match shared.mem.heap.get_field(obj_ref, slot) {
        cratonvm_types::Value::Long(h) if h != 0 => Some(h as u64),
        _ => None,
    }
}

#[cfg(feature = "gpu-offload")]
fn extract_completed_predecessor_value(
    pred_handle: u64,
    result: &SerializedResult,
) -> Result<ChainPredecessorValue, String> {
    match result {
        SerializedResult::Void => Ok(ChainPredecessorValue::None),
        SerializedResult::ScalarI32(v) => {
            Ok(ChainPredecessorValue::Scalar(cratonvm_types::Value::Int(*v)))
        }
        SerializedResult::ScalarI64(v) => Ok(ChainPredecessorValue::Scalar(
            cratonvm_types::Value::Long(*v),
        )),
        SerializedResult::ScalarF32(v) => Ok(ChainPredecessorValue::Scalar(
            cratonvm_types::Value::Float(*v),
        )),
        SerializedResult::ScalarF64(v) => Ok(ChainPredecessorValue::Scalar(
            cratonvm_types::Value::Double(*v),
        )),
        SerializedResult::DeviceArray {
            array_handle, len, ..
        } => {
            let Some((_, _, buf)) = device_cache::get_entry_meta_and_pin(*array_handle) else {
                return Err(format!(
                    "predecessor submission {pred_handle} device array handle {array_handle} is no longer cached"
                ));
            };
            Ok(ChainPredecessorValue::DeviceArray(ChainedDeviceArrayArg {
                array_handle: *array_handle,
                buf,
                len: *len,
                is_resident: false,
            }))
        }
        SerializedResult::PrimitiveArrayI32 { .. }
        | SerializedResult::PrimitiveArrayI64 { .. }
        | SerializedResult::PrimitiveArrayF32 { .. }
        | SerializedResult::PrimitiveArrayF64 { .. } => Ok(ChainPredecessorValue::None),
    }
}

#[cfg(feature = "gpu-offload")]
fn resolve_chain_predecessor(
    shared: &crate::vm::SharedVm,
    pred_handle: u64,
    pred_sub: &std::sync::Arc<StreamSubmission>,
) -> Result<ChainPredecessorValue, String> {
    {
        let status = pred_sub.status.lock();
        match &*status {
            SubmissionStatus::Failed { message, .. } => {
                return Err(format!(
                    "predecessor submission {pred_handle} failed: {message}"
                ));
            }
            SubmissionStatus::Completed { result } => {
                return extract_completed_predecessor_value(pred_handle, result);
            }
            SubmissionStatus::Running => {}
        }
    }

    let mut need_scalar_sync = false;
    {
        let finalize_guard = pred_sub.finalize.lock();
        if let Some(fs) = finalize_guard.as_ref() {
            for wb in &fs.writebacks {
                if let MarshalWriteback::DeviceArrayOutput {
                    array_handle,
                    len,
                    buf,
                    ..
                } = wb
                {
                    let _ = device_cache::pin_ephemeral(*array_handle);
                    return Ok(ChainPredecessorValue::DeviceArray(ChainedDeviceArrayArg {
                        array_handle: *array_handle,
                        buf: buf.clone(),
                        len: *len,
                        is_resident: false,
                    }));
                }
            }
            need_scalar_sync = fs.writebacks.iter().any(|wb| {
                matches!(
                    wb,
                    MarshalWriteback::ScalarI32 { .. }
                        | MarshalWriteback::ScalarI64 { .. }
                        | MarshalWriteback::ScalarF32 { .. }
                        | MarshalWriteback::ScalarF64 { .. }
                )
            });
            if !need_scalar_sync {
                return Ok(ChainPredecessorValue::None);
            }
        }
    }

    if need_scalar_sync {
        finalize_submission(shared, pred_sub)?;
    }
    let status = pred_sub.status.lock();
    match &*status {
        SubmissionStatus::Completed { result } => {
            extract_completed_predecessor_value(pred_handle, result)
        }
        SubmissionStatus::Failed { message, .. } => Err(format!(
            "predecessor submission {pred_handle} failed: {message}"
        )),
        SubmissionStatus::Running => Ok(ChainPredecessorValue::None),
    }
}

/// Chain a downstream kernel launch onto `pred_handle`'s CUDA stream without
/// stalling the host on intermediate device-resident arrays (`DeviceArray`).
///
/// Reuses `pred_handle`'s underlying stream (even when it came from the
/// internal dispatch stream pool with `stream_handle == None`) and wires any
/// `GpuFutureImpl` arguments cleanly to their predecessor's device buffer or
/// scalar result, releasing ephemeral intermediate handles after enqueue.
#[cfg(feature = "gpu-offload")]
pub fn chain_submission_from_native(
    shared: &crate::vm::SharedVm,
    pred_handle: u64,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[cratonvm_types::Value],
) -> Result<u64, String> {
    use cratonvm_types::Value;
    shared.offload_registry.ensure_vm_hooks(shared);
    if pred_handle == 0 {
        return Err("chain_submission_from_native: predecessor submission handle was 0".to_string());
    }
    let pred_sub = lookup_submission(pred_handle)
        .ok_or_else(|| format!("unknown predecessor submission handle {pred_handle}"))?;

    let mut pin_guard = EphemeralPinGuard(Vec::new());
    let pred_val = resolve_chain_predecessor(shared, pred_handle, &pred_sub)?;
    if let ChainPredecessorValue::DeviceArray(ref arg) = pred_val {
        pin_guard.track(arg.array_handle);
    }
    let Some(pred_stream) = pred_sub.stream.clone() else {
        return Err("unsupported".to_string());
    };

    let mut resolved_args: Vec<Value> = java_args.to_vec();
    let mut chained_arrays: Vec<Option<ChainedDeviceArrayArg>> = vec![None; java_args.len()];
    let mut cached_pred_val = Some(pred_val);
    let cache = shared
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config);

    for (idx, val) in java_args.iter().enumerate() {
        let Value::Object(Some(obj_ref)) = *val else {
            continue;
        };
        let Some(fut_handle) = read_gpu_future_handle(shared, obj_ref) else {
            if let Some((_, len, arr_handle)) = try_gpu_array_shape(shared, &cache, obj_ref) {
                if let Some(cached) = device_cache::get_cached(arr_handle) {
                    chained_arrays[idx] = Some(ChainedDeviceArrayArg {
                        array_handle: arr_handle,
                        buf: cached,
                        len,
                        is_resident: true,                    });                }            }
            continue;
        };
        let resolved = if fut_handle == pred_handle {
            if let Some(first) = cached_pred_val.take() {
                first
            } else {
                let r = resolve_chain_predecessor(shared, fut_handle, &pred_sub)?;
                if let ChainPredecessorValue::DeviceArray(ref arg) = r {
                    pin_guard.track(arg.array_handle);
                }
                r
            }
        } else {
            let other_sub = lookup_submission(fut_handle).ok_or_else(|| {
                format!("unknown predecessor submission handle {fut_handle} in chained arg #{idx}")
            })?;
            let val = resolve_chain_predecessor(shared, fut_handle, &other_sub)?;
            if let ChainPredecessorValue::DeviceArray(ref arg) = val {
                pin_guard.track(arg.array_handle);
            }
            if matches!(val, ChainPredecessorValue::DeviceArray(_)) {
                let same_stream = other_sub
                    .stream
                    .as_ref()
                    .is_some_and(|s| std::sync::Arc::ptr_eq(s, &pred_stream));
                if !same_stream {
                    if let Some(ev) = other_sub.event.as_ref() {
                        ev.synchronize()
                            .map_err(|e| format!("cross-stream sync for arg #{idx}: {e}"))?;
                    }
                }
            }
            val
        };
        match resolved {
            ChainPredecessorValue::DeviceArray(arg) => {
                chained_arrays[idx] = Some(arg);
            }
            ChainPredecessorValue::Scalar(scalar_val) => {
                resolved_args[idx] = scalar_val;
            }
            ChainPredecessorValue::None => {
                return Err("unsupported".to_string());
            }
        }
    }

    let submission = dispatch_method_inner_ext(
        shared,
        class_name,
        method_name,
        descriptor,
        &resolved_args,
        StreamTarget::Direct {
            stream: pred_stream,
            stream_handle: pred_sub.stream_handle,
        },
        Some(&chained_arrays),
        Completion::Reaper,
    );
    let handle = register_submission(submission);
    drop(pin_guard);
    Ok(handle)
}

/// Atomically promote a completed (or runnable) `DeviceArray` submission result
/// into a user-owned `GpuArray` entry registered under `new_array_handle`,
/// returning `Some((elem, len))` once and downgrading the submission's result to
/// `SerializedResult::Void` to enforce single-claim ownership.
#[cfg(feature = "gpu-offload")]
pub fn promote_submission_device_array(
    shared: &crate::vm::SharedVm,
    future_handle: u64,
    new_array_handle: u64,
) -> Option<(cratonvm_native_api::registry::DeviceArrayElem, usize)> {
    if future_handle == 0 || new_array_handle == 0 {
        return None;
    }
    let sub = lookup_submission(future_handle)?;
    let is_running = matches!(&*sub.status.lock(), SubmissionStatus::Running);
    if is_running && finalize_submission(shared, &sub).is_err() {
        return None;
    }
    let mut status = sub.status.lock();
    let SubmissionStatus::Completed { result } = &mut *status else {
        return None;
    };
    let SerializedResult::DeviceArray {
        array_handle,
        elem,
        len,
    } = *result
    else {
        return None;
    };
    let promoted = if len == 0 && array_handle == 0 {
        true
    } else {
        device_cache::promote_ephemeral_to_resident(array_handle, new_array_handle)
    };
    if !promoted {
        return None;
    }
    *result = SerializedResult::Void;
    sub.ephemeral_output_handle
        .store(0, std::sync::atomic::Ordering::Release);
    Some((elem, len))
}

#[cfg(feature = "gpu-offload")]
fn dispatch_method_inner(
    shared: &crate::vm::SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[cratonvm_types::Value],
    stream_handle: Option<u64>,
    completion: Completion,
) -> std::sync::Arc<StreamSubmission> {
    dispatch_method_inner_ext(
        shared,
        class_name,
        method_name,
        descriptor,
        java_args,
        StreamTarget::Handle(stream_handle),
        None,
        completion,
    )
}

#[cfg(feature = "gpu-offload")]
#[allow(clippy::too_many_arguments)]
fn dispatch_method_inner_ext(
    shared: &crate::vm::SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    java_args: &[cratonvm_types::Value],
    stream_target: StreamTarget,
    chained_arrays: Option<&[Option<ChainedDeviceArrayArg>]>,
    completion: Completion,
) -> std::sync::Arc<StreamSubmission> {
    use cratonvm_types::{ArrayElementType, Value};
    use cuda_bridge::{KernelArgs, Stream as CudaStream};
    use std::sync::Arc;

    let dispatch_entered = std::time::Instant::now();

    // 1. Resolve the cache and ensure VM GC/VRAM coordination hooks are bound.
    let cache = shared
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config);
    shared.offload_registry.ensure_vm_hooks(shared);

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
        can_fail_after_store,
    ): (
        crate::classloading::ClassId,
        u16,
        bool,
        Vec<String>,
        u64,
        ParamKind,
        jit_cuda::emitter::WorkBound,
        bool,
    ) = if let Some(hit) = cache.dispatch_memo_get(class_name, method_name, descriptor) {
        hit.into_tuple()
    } else {
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
            return failed_submission(
                None,
                GpuErrorKind::Compile,
                format!("submitMethod: load class failed for {class_name}: {e:?}"),
            );
        }
        let cm = shared.classes.class_manager.read();
        let class_id = match cm.get_loaded_class_id(class_name) {
            Some(id) => id,
            None => {
                return failed_submission(
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
                return failed_submission(
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
                return failed_submission(
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
                        return failed_submission(
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
                        return failed_submission(
                            None,
                            GpuErrorKind::Compile,
                            format!(
                                "submitMethod: this_field_cps[{}]=#{cp} has no resolvable \
                                 NameAndType",
                                names.len(),
                            ),
                        );
                    };
                    names.push(nm.to_string());                }
                let resolved = ResolvedDispatch {
                    class_name: class_name.to_string(),
                    method_name: method_name.to_string(),
                    descriptor: descriptor.to_string(),
                    class_id,
                    method_index: mi,
                    is_static: is_static_local,
                    this_field_names: names,
                    writes_param_mask: compiled.signature.writes_param_mask,
                    return_kind: compiled.signature.return_kind,
                    work_bound: compiled.signature.work_bound,
                    can_fail_after_store: compiled.can_fail_after_store,
                };
                cache.dispatch_memo_put(class_name, method_name, descriptor, &resolved);
                resolved.into_tuple()
            }
            LookupOutcome::Skip => {
                return failed_submission(
                    None,
                GpuErrorKind::Compile,
                    format!(
                        "submitMethod: method not offloadable (Skip): {class_name}.{method_name}{descriptor}",
                    ),);
            }
            LookupOutcome::Blacklisted => {
                return failed_submission(
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

    // Inner phase timing. `inner_ns` accumulates only the phases named
    // below; the caller subtracts it from its own measurement of the
    // whole call and reports the remainder as `dispatch_other`.
    // See `cratonvm_types::gpu_offload_phase_census`.
    let timed = cratonvm_types::gpu_offload_phase_census::enabled();
    let mut inner_ns = 0u64;
    let mut mark = std::time::Instant::now();
    if timed {
        // Steps 1-3 happened before this point: the cache handle, the
        // SECOND class+method resolve (the first was `try_dispatch`'s)
        // and the memo that exists to make it cheap.
        DISPATCH_PROLOGUE_NS.with(|c| {
            c.set(c.get() + mark.saturating_duration_since(dispatch_entered).as_nanos() as u64)
        });
    }

    // 4. From here on we need a real device context. The Failed-fast
    //    path is identical to dispatch_async's no-device branch.
    let ctx = match cache.device() {
        Some(c) => c,
        None => {
            return failed_submission(
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
    let (stream, effective_stream_handle): (Arc<Stream>, Option<u64>) = match stream_target {
        StreamTarget::Direct {
            stream,
            stream_handle,
        } => (stream, stream_handle),
        StreamTarget::Handle(Some(h)) => match cache.resolve_stream(h) {
            Some(s) => (s, Some(h)),
            None => {
                return failed_submission(
                    None,
                    GpuErrorKind::Launch,
                    format!(
                        "submitMethod: unknown or released stream handle {h} \
                         ({class_name}.{method_name}{descriptor})",
                    ),
                );
            }
        },
        // A handle-less caller takes a pooled stream rather than a
        // freshly created one — see `OffloadCache::dispatch_streams`.
        StreamTarget::Handle(None) => match cache.dispatch_stream(ctx) {
            Ok(s) => (s, None),
            Err(e) => {
                return failed_submission(
                    None,
                    kind_of_device_error(&e),
                    format!("submitMethod: no dispatch stream available: {e}"),
                );
            }
        },
    };

    if timed {
        let n = mark.elapsed().as_nanos() as u64;
        cratonvm_types::gpu_offload_phase_census::add(3, n);
        inner_ns += n;
        mark = std::time::Instant::now();
    }
    // 6. Open the MARSHAL window. The zero-copy upload hands the device
    //    the heap arena's own address and host-blocks until the DMA has
    //    retired (`gpu_marshal::zerocopy_enabled`), so for the length of
    //    this window nothing may relocate: a `Relocation::Forbidden`
    //    token says so to every collector. The long-lived keep-alive
    //    token that outlives this frame is taken at step 9, once the
    //    writeback targets are known.
    //
    //    The `SafepointToken` the `gpu_marshal::*` signatures require is
    //    a type-level marker on a local counter; the registry token is
    //    what the collector reads.
    let marshal_window = GcCriticalGuard::acquire_for(
        shared.vm_identity as u64,
        None,
        "dispatch_method_inner:marshal",
        cuda_bridge::critical::Relocation::Forbidden,
        &[],
    );
    let local_counter = std::sync::atomic::AtomicU32::new(0);
    let token = cratonvm_gc::safepoint::SafepointToken::new(&local_counter);

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
    let mut retained_buffers: Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>> = Vec::new();
    let mut dma_guards: Vec<crate::runtime::gpu_marshal::AsyncDmaLifetime> = Vec::new();
    let mut h2d_zerocopy_keepalive: Vec<usize> = Vec::new();
    macro_rules! fail_after_marshal {
        ($kind:expr, $msg:expr) => {{
            if crate::runtime::gpu_marshal::requires_stream_sync_on_finish(&dma_guards) {
                let _ = stream.synchronize();
            }
            release_ephemeral_writebacks(&writebacks);
            drop(writebacks);
            drop(retained_buffers);
            drop(dma_guards);
            drop(token);
            drop(marshal_window);
            return failed_submission(Some(stream.clone()), $kind, $msg);
        }};
    }
    let mut max_array_len: usize = 0;
    // Per-declared-parameter array lengths, so `work_bound` can name
    // the one that is the loop trip count. Only the static path
    // populates this (the `pthis_*` prelude has no declared index),
    // which is also the only path `WorkBound::ParamLen` is derived on.
    let mut param_lens: Vec<Option<usize>> = Vec::new();
    // The same, for `int` SCALAR arguments, so `WorkBound::ParamScalar`
    // can name the one that is the loop trip count. Unlike an array
    // length this value is not bounded by anything the marshaller sees,
    // which is exactly why the grid has to be told about it.
    let mut param_scalars: Vec<Option<i32>> = Vec::new();
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
    let (java_args_to_marshal, chained_slice): (
        &[cratonvm_types::Value],
        Option<&[Option<ChainedDeviceArrayArg>]>,
    ) = if !is_static {
        // 7a. Pull the receiver from `java_args[0]`.
        let receiver = match java_args.first() {
            Some(Value::Object(Some(r))) => *r,
            Some(Value::Object(None)) => {
                fail_after_marshal!(
                    GpuErrorKind::Compile,
                    format!(
                        "submitMethod: non-static receiver is null ({class_name}.{method_name}{descriptor})",
                    )
                );
            }
            Some(other) => {
                fail_after_marshal!(
                    GpuErrorKind::Compile,
                    format!(
                        "submitMethod: non-static receiver is not an object reference: {other:?}",
                    )
                );
            }
            None => {
                fail_after_marshal!(
                    GpuErrorKind::Compile,
                    format!(
                        "submitMethod: non-static method called with no arguments \
                         (expected receiver as arg 0): {class_name}.{method_name}{descriptor}",
                    )
                );
            }
        };

        // 7b. Resolve each field name → slot on the receiver's
        //     concrete class, read the field, marshal it as a
        //     primitive-array kernel arg.
        let receiver_class_id = shared.mem.heap.class_id_of(receiver);
        for (i, field_name) in this_field_names.iter().enumerate() {
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
                        fail_after_marshal!(
                            GpuErrorKind::Compile,
                            format!(
                                "submitMethod: this_field `{field_name}` not found on receiver's \
                                 class hierarchy (receiver class_id={receiver_class_id:?})",
                            )
                        );
                    }
                }
            };
            let field_val = shared.mem.heap.get_field(receiver, slot);
            let field_obj = match field_val {
                Value::Object(Some(o)) => o,
                Value::Object(None) => {
                    fail_after_marshal!(
                        GpuErrorKind::Compile,
                        format!(
                            "submitMethod: this_field `{field_name}` (pthis_{i}) is null on receiver",
                        )
                    );
                }
                other => {
                    fail_after_marshal!(
                        GpuErrorKind::Compile,
                        format!(
                            "submitMethod: this_field `{field_name}` (pthis_{i}) is not an \
                             object reference: {other:?}",
                        )
                    );
                }
            };
            let Some(etype) = shared.mem.heap.array_element_type(field_obj) else {
                fail_after_marshal!(
                    GpuErrorKind::Compile,
                    format!(
                        "submitMethod: this_field `{field_name}` (pthis_{i}) does not point at a \
                         primitive array",
                    )
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
            match marshal_array_arg(
                shared,
                ctx,
                &stream,
                &cache.host_pin_cache,
                &cache.pinned_host_pool,
                field_obj,
                etype,
                true,
                can_fail_after_store,
                &token,
            ) {
                Ok((args_after, wb_opt, bytes, dma_life, retained)) => {
                    kernel_args = args_after(kernel_args);
                    h2d_bytes = h2d_bytes.saturating_add(bytes);
                    dma_guards.push(dma_life);
                    retained_buffers.push(retained);
                    if pthis_len > max_array_len {
                        max_array_len = pthis_len;
                    }
                    if let Some(wb) = wb_opt {
                        writebacks.push(wb);
                    }
                }
                Err(msg) => {
                    fail_after_marshal!(kind_of_device_message(&msg), msg);
                }
            }
        }
        // Skip the receiver itself — it's not a kernel arg.
        (&java_args[1..], chained_arrays.map(|s| &s[1..]))
    } else {
        // Static methods: everything in java_args is a kernel arg.
        (java_args, chained_arrays)
    };

    for (i, arg) in java_args_to_marshal.iter().enumerate() {
        if let Some(Some(chained)) = chained_slice.and_then(|s| s.get(i)) {
            match marshal_from_cached_entry(
                chained.array_handle,
                chained.buf.clone(),
                chained.len,
                chained.is_resident,
            ) {
                Ok((args_after, wb_opt, retained)) => {
                    kernel_args = args_after(kernel_args);
                    retained_buffers.push(retained);
                    if let Some(wb) = wb_opt {
                        writebacks.push(wb);
                    }
                    if chained.len > max_array_len {
                        max_array_len = chained.len;
                    }
                    record_param_len(&mut param_lens, i, chained.len);
                }
                Err(msg) => {
                    fail_after_marshal!(kind_of_device_message(&msg), msg);
                }
            }
            continue;
        }
        match arg {
            Value::Int(v) => {
                record_param_scalar(&mut param_scalars, i, *v);
                kernel_args = kernel_args.push_i32(*v)
            }
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
                    match marshal_array_arg(
                        shared,
                        ctx,
                        &stream,
                        &cache.host_pin_cache,
                        &cache.pinned_host_pool,
                        *obj_ref,
                        element_type,
                        is_written,
                        can_fail_after_store,
                        &token,
                    ) {
                        Ok((args_after, wb_opt, bytes, dma_life, retained)) => {
                            kernel_args = args_after(kernel_args);
                            h2d_bytes = h2d_bytes.saturating_add(bytes);
                            if wb_opt.is_none()
                                && matches!(
                                    dma_life,
                                    crate::runtime::gpu_marshal::AsyncDmaLifetime::ZeroCopySpan(_)
                                        | crate::runtime::gpu_marshal::AsyncDmaLifetime::SharedHostSpan(_)
                                )
                            {
                                let addr = obj_ref.as_ptr() as usize;
                                if !h2d_zerocopy_keepalive.contains(&addr) {
                                    h2d_zerocopy_keepalive.push(addr);
                                }
                            }
                            dma_guards.push(dma_life);
                            retained_buffers.push(retained);
                            if arr_len > max_array_len {
                                max_array_len = arr_len;
                            }
                            record_param_len(&mut param_lens, i, arr_len);
                            if let Some(wb) = wb_opt {
                                writebacks.push(wb);
                            }
                        }
                        Err(msg) => {
                            fail_after_marshal!(kind_of_device_message(&msg), msg);
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
                if let Some((etype, len, arr_handle)) =
                    try_gpu_array_shape(shared, &cache, *obj_ref)
                {
                    match marshal_resident_array_arg(ctx, etype, len, arr_handle, shared.vm_identity) {
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
                            fail_after_marshal!(kind_of_device_message(&msg), msg);
                        }
                    }
                    continue;
                }
                // Third: is it a boxed primitive? Java's varargs
                // autobox `int` → Integer, etc.
                match try_unbox_primitive(shared, *obj_ref) {
                    Some(Value::Int(v)) => {
                        // A boxed `Integer` is what a varargs `submit`
                        // hands over, and it is the ONLY way GPULlama3's
                        // dispatch site passes a scalar - so recording it
                        // here as well as in the unboxed arm is what makes
                        // `WorkBound::ParamScalar` usable from Java at all.
                        record_param_scalar(&mut param_scalars, i, v);
                        kernel_args = kernel_args.push_i32(v)
                    }
                    Some(Value::Long(v)) => kernel_args = kernel_args.push_i64(v),
                    Some(Value::Float(v)) => kernel_args = kernel_args.push_f32(v),
                    Some(Value::Double(v)) => kernel_args = kernel_args.push_f64(v),
                    _ => {
                        fail_after_marshal!(
                            GpuErrorKind::Compile,
                            format!(
                                "submitMethod: arg #{i} is not a primitive array, GpuArray, or boxed primitive",
                            )
                        );
                    }
                }
                continue;
            }
            Value::Object(None) => {
                fail_after_marshal!(
                    GpuErrorKind::Compile,
                    format!("submitMethod: arg #{i} is null")
                );
            }
            _ => {
                fail_after_marshal!(
                    GpuErrorKind::Compile,
                    format!("submitMethod: arg #{i} type unsupported: {arg:?}")
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
    let work_items: usize = match work_bound {
        jit_cuda::emitter::WorkBound::ParamLen(idx) => param_lens
            .get(idx as usize)
            .copied()
            .flatten()
            .unwrap_or(max_array_len),
        jit_cuda::emitter::WorkBound::Literal(v) if v > 0 => v as usize,
        jit_cuda::emitter::WorkBound::ParamScalar(idx) => {
            match param_scalars.get(idx as usize).copied().flatten() {
                Some(v) => max_array_len.max(v.max(0) as usize),
                None => {
                    fail_after_marshal!(
                        GpuErrorKind::Launch,
                        format!(
                            "the kernel's loop bound is `int` parameter {idx}, but no \
                             scalar argument was marshalled at that index - \
                             refusing to size the grid from the largest array"
                        )
                    );
                }
            }
        }
        _ => max_array_len,
    };
    let runtime_work: u32 = u32::try_from(work_items).unwrap_or(u32::MAX);

    macro_rules! push_scalar_return_buf {
        ($ty:ty, $variant:ident, $tag:literal) => {{
            let alloc_res = if ctx.is_shared_memory_gpu() {
                cuda_bridge::DeviceBuffer::<$ty>::new_shared_zeroed(ctx, 1)
            } else {
                cuda_bridge::DeviceBuffer::<$ty>::zeros(ctx, 1)
            };
            let buf = match alloc_res {
                Ok(b) => std::sync::Arc::new(b),
                Err(e) => {
                    fail_after_marshal!(
                        kind_of_device_error(&e),
                        format!(
                            "submitMethod: failed to allocate scalar-return ({}) buffer: {e}",
                            $tag,
                        )
                    );
                }
            };
            kernel_args = kernel_args.push_device_ptr(buf.as_ref());
            writebacks.push(MarshalWriteback::$variant { buf });
        }};
    }
    macro_rules! push_array_return_buf {
        ($ty:ty, $cb_variant:ident, $elem:expr, $tag:literal) => {{
            let out_len = work_items.max(1);
            let alloc_res = if ctx.is_shared_memory_gpu() {
                cuda_bridge::DeviceBuffer::<$ty>::new_shared_zeroed(ctx, out_len)
            } else {
                cuda_bridge::DeviceBuffer::<$ty>::zeros(ctx, out_len)
            };
            let buf = match alloc_res {
                Ok(b) => std::sync::Arc::new(b),
                Err(e) => {
                    fail_after_marshal!(
                        kind_of_device_error(&e),
                        format!(
                            "submitMethod: failed to allocate array-return ({}) buffer: {e}",
                            $tag,
                        )
                    );
                }
            };
            let cached = device_cache::CachedBuffer::$cb_variant(buf.clone());
            let array_handle = device_cache::insert_ephemeral(cached.clone(), shared.vm_identity);
            kernel_args = kernel_args
                .push_device_ptr(buf.as_ref())
                .push_i32(i32::try_from(out_len).unwrap_or(i32::MAX));
            writebacks.push(MarshalWriteback::DeviceArrayOutput {
                array_handle,
                elem: $elem,
                len: out_len,
                buf: cached,
            });
        }};
    }
    match return_kind {
        ParamKind::I32 => push_scalar_return_buf!(i32, ScalarI32, "i32"),
        ParamKind::I64 => push_scalar_return_buf!(i64, ScalarI64, "i64"),
        ParamKind::F32 => push_scalar_return_buf!(f32, ScalarF32, "f32"),
        ParamKind::F64 => push_scalar_return_buf!(f64, ScalarF64, "f64"),
        ParamKind::I32Array => push_array_return_buf!(
            i32,
            I32,
            cratonvm_native_api::registry::DeviceArrayElem::I32,
            "i32[]"
        ),
        ParamKind::I64Array => push_array_return_buf!(
            i64,
            I64,
            cratonvm_native_api::registry::DeviceArrayElem::I64,
            "i64[]"
        ),
        ParamKind::F32Array => push_array_return_buf!(
            f32,
            F32,
            cratonvm_native_api::registry::DeviceArrayElem::F32,
            "f32[]"
        ),
        ParamKind::F64Array => push_array_return_buf!(
            f64,
            F64,
            cratonvm_native_api::registry::DeviceArrayElem::F64,
            "f64[]"
        ),
        ParamKind::Void
        | ParamKind::I16Array
        | ParamKind::I8Array
        | ParamKind::BoolArray => {}
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
    // While capturing, every launch in the graph shares ONE flag cell.
    //
    // Not an optimization -- the alternative does not work. A pooled
    // flag is returned to the pool when its submission finalizes, and a
    // captured launch never finalizes, so 453 flags would be held out
    // of the pool for the life of the graph and, worse, a replay would
    // have to read all 453 of them to learn whether anything failed:
    // one 8-byte download per node, against a mechanism whose entire
    // point is to stop doing per-node host work. One shared cell costs
    // one download per replay and answers the only question the caller
    // can act on -- did a bounds check fail in this step -- at the
    // granularity the replay actually has, which is the whole step.
    //
    // It is sticky: nothing resets it between replays, so the first
    // failure keeps reporting. That is the right bias for a condition
    // that means the kernel indexed out of range.
    let (failure_flag_buf, flag_pool_key) = match cache.capture_shared_flag(&stream) {
        Some(shared) => (shared, None),
        None => match flag_pool::take(pool_key, ctx) {
            Ok(b) => (b, Some(pool_key)),
            Err(e) => {
                fail_after_marshal!(
                    GpuErrorKind::Compile,
                    format!("submitMethod: failed to allocate failure_flag buffer: {e}")
                );
            }
        },
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
        pool_key: flag_pool_key,
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

    if timed {
        let n = mark.elapsed().as_nanos() as u64;
        cratonvm_types::gpu_offload_phase_census::add(4, n);
        inner_ns += n;
        mark = std::time::Instant::now();
    }
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
    // Retain `Relocation::Forbidden` whenever zero-copy pinned arena DMAs are
    // in flight on `stream`; otherwise downgrade to `Relocation::KeepAliveOnly`
    // once marshalling is complete.
    let mut keepalive: Vec<usize> = writebacks
        .iter()
        .filter_map(MarshalWriteback::heap_target)
        .map(|obj| obj.as_ptr() as usize)
        .collect();
    // Append read-only ZeroCopySpan H2D array inputs AFTER output writeback
    // targets so `finalize_submission`'s 1:1 `zip(live)` over `heap_target_mut`
    // stays aligned while GC keeps H2D DMA source objects alive.
    keepalive.extend(h2d_zerocopy_keepalive);
    let needs_relocation_veto = crate::runtime::gpu_marshal::requires_relocation_veto(&dma_guards);
    let relocation = if needs_relocation_veto {
        cuda_bridge::critical::Relocation::Forbidden
    } else {
        cuda_bridge::critical::Relocation::KeepAliveOnly
    };
    let gc_guard = GcCriticalGuard::acquire_for(
        shared.vm_identity as u64,
        None,
        "dispatch_method_inner:submission",
        relocation,
        &keepalive,
    );
    drop(token);
    drop(marshal_window);
    if timed {
        let n = mark.elapsed().as_nanos() as u64;
        cratonvm_types::gpu_offload_phase_census::add(5, n);
        inner_ns += n;
        DISPATCH_INNER_NS.with(|c| c.set(c.get() + inner_ns));
    }
    if timed {
        cratonvm_native_builtins::craton_gpu::dispatch_timing::add(
            5,
            mark.elapsed().as_nanos() as u64,
        );
        mark = std::time::Instant::now();
    }
    let submission = cache.dispatch_async_on_handle(
        stream.clone(),
        effective_stream_handle,
        class_id,
        method_index,
        kernel_args,
        runtime_work,
        Some(FinalizeState {
            writebacks,
            retained_buffers,
            dma_guards,
            gc_critical: gc_guard,
        }),
        shared.self_arc.read().as_ref().cloned().unwrap_or_default(),
        completion,
    );

    submission
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
        mut writebacks,
        retained_buffers,
        dma_guards,
        gc_critical,
    }) = pending
    {
        let has_zerocopy_h2d =
            crate::runtime::gpu_marshal::requires_stream_sync_on_finish(&dma_guards);
        let sync_before_drop_h2d = || {
            if has_zerocopy_h2d {
                if let Some(event) = submission.event.as_ref() {
                    let _ = event.synchronize();
                } else if let Some(stream) = submission.stream.as_ref() {
                    let _ = stream.synchronize();
                }
            }
        };
        // 0. A revoked token means the registry stopped keeping this
        //    submission's arrays alive — its lease ran out, or its VM is
        //    gone. Nothing below may touch the heap.
        if gc_critical.is_revoked() {
            sync_before_drop_h2d();
            drop(dma_guards);
            drop(retained_buffers);
            let message = "GPU critical-section token was revoked (lease expired or VM shut \
                           down) before the writeback ran; result discarded"
                .to_string();
            *submission.status.lock() = SubmissionStatus::Failed {
                message: message.clone(),
                kind: GpuErrorKind::Launch,
            };
            drop(writebacks);
            drop(gc_critical);
            return Err(message);
        }
        // 1. Wait for the kernel to complete via the recorded event.
        //
        //    Skipped for a chunked submission (unless zero-copy H2D pinned
        //    DMAs ran on `submission.stream`): its writeback waits each chunk
        //    event in turn and copies that chunk out, overlapping with GPU work.
        let is_chunked = writebacks
            .iter()
            .any(|wb| matches!(wb, MarshalWriteback::Chunked { .. }));
        let finalize_timed = cratonvm_types::gpu_offload_phase_census::enabled();
        let finalize_mark = std::time::Instant::now();
        if let Some(event) = submission
            .event
            .as_ref()
            .filter(|_| !is_chunked || has_zerocopy_h2d)
        {
            if let Err(e) = event.synchronize() {
                if has_zerocopy_h2d {
                    if let Some(stream) = submission.stream.as_ref() {
                        let _ = stream.synchronize();
                    }
                }
                drop(dma_guards);
                drop(retained_buffers);
                let mut status = submission.status.lock();
                *status = SubmissionStatus::Failed {
                    message: format!("event.synchronize: {e}"),
                    kind: kind_of_device_error(&e),
                };
                drop(writebacks);
                drop(gc_critical);
                return Err(match &*status {
                    SubmissionStatus::Failed { message, .. } => message.clone(),
                    _ => unreachable!(),
                });
            }
        }
        for wb in writebacks.iter() {
            if let Err(msg) = wb.sync_chunks() {
                if has_zerocopy_h2d {
                    if let Some(stream) = submission.stream.as_ref() {
                        let _ = stream.synchronize();
                    }
                }
                drop(dma_guards);
                drop(retained_buffers);
                let mut status = submission.status.lock();
                *status = SubmissionStatus::Failed {
                    message: msg.clone(),
                    kind: GpuErrorKind::Launch,
                };
                drop(writebacks);
                drop(gc_critical);
                return Err(msg);
            }
        }
        drop(dma_guards);
        drop(retained_buffers);
        if finalize_timed {
            // Phase 8: the host waiting for the DEVICE. Everything after
            // this point is phase 9, the writeback itself.
            cratonvm_types::gpu_offload_phase_census::add(
                8,
                finalize_mark.elapsed().as_nanos() as u64,
            );
        }
        let writeback_mark = std::time::Instant::now();
        // 2. Open the WRITEBACK window: the downloads below land in the
        //    heap arena in place, so relocation is forbidden until they
        //    have retired. Taken BEFORE the addresses are read back, so
        //    nothing can move them between the read and the write; the
        //    acquisition itself waits out any moving cycle in progress
        //    (`Registry::begin_moving_cycle`).
        let writeback_window = GcCriticalGuard::acquire_for(
            gc_critical_vm(shared),
            Some(submission.handle),
            "finalize_submission:writeback",
            cuda_bridge::critical::Relocation::Forbidden,
            &[],
        );
        // The collector may have moved the target arrays while the kernel
        // ran; the registry has their current addresses.
        if gc_critical.is_revoked() {
            let message = "GPU critical-section token was revoked while waiting for the \
                           device; result discarded"
                .to_string();
            *submission.status.lock() = SubmissionStatus::Failed {
                message: message.clone(),
                kind: GpuErrorKind::Launch,
            };
            drop(writeback_window);
            drop(writebacks);
            drop(gc_critical);
            return Err(message);
        }
        let live = gc_critical.keepalive_addrs();
        for (target, addr) in writebacks
            .iter_mut()
            .filter_map(MarshalWriteback::heap_target_mut)
            .zip(live)
        {
            // SAFETY: `addr` is the collector-maintained current address of
            // the object this writeback declared at dispatch, kept alive by
            // the token that is still held.
            *target = unsafe { cratonvm_types::ObjectRef::from_raw(addr as *mut u8) };
        }
        // The `SafepointToken` the `gpu_marshal::*` signatures require is
        // a type-level marker on a local counter; `writeback_window` is
        // what the collector reads.
        let local_counter = std::sync::atomic::AtomicU32::new(0);
        let local_token = cratonvm_gc::safepoint::SafepointToken::new(&local_counter);

        // 3. Drain writebacks. First failure marks the submission
        //    Failed and stops further writebacks.
        let cache_opt = shared.offload_registry.get(shared.config.gpu_device_ordinal);
        let d2h_ctx = match (submission.stream.as_deref(), cache_opt.as_ref()) {
            (Some(stream), Some(cache)) => {
                Some((stream, &cache.host_pin_cache, &cache.pinned_host_pool))
            }
            _ => None,
        };
        let mut d2h_guards: Vec<crate::runtime::gpu_marshal::AsyncDmaLifetime> = Vec::new();
        let mut first_err: Option<String> = None;
        let mut scalar_result: Option<SerializedResult> = None;
        let is_flag = |wb: &&MarshalWriteback| matches!(wb, MarshalWriteback::FailureFlag { .. });
        for wb in writebacks
            .iter()
            .filter(is_flag)
            .chain(writebacks.iter().filter(|wb| !is_flag(wb)))
        {
            if first_err.is_some() {
                break;
            }
            match wb.writeback(shared, &local_token, d2h_ctx, &mut d2h_guards) {
                Ok(Some(result)) => scalar_result = Some(result),
                Ok(None) => {}
                Err(msg) => {
                    first_err = Some(msg);
                    break;
                }
            }
        }
        if crate::runtime::gpu_marshal::requires_stream_sync_on_finish(&d2h_guards) {
            if let Some(stream) = submission.stream.as_ref() {
                if let Err(e) = stream.synchronize() {
                    if first_err.is_none() {
                        first_err = Some(format!("writeback stream.synchronize: {e}"));
                    }
                }
            }
        }
        drop(d2h_guards);
        if let Some(cache) = cache_opt.as_ref() {
            cache.pinned_host_pool.reclaim_completed();
        }
        // 4. Close both windows BEFORE we touch the status mutex so a
        //    concurrent reader of status doesn't hold the collector off
        //    longer than necessary.
        drop(local_token);
        drop(writeback_window);
        drop(writebacks);
        drop(gc_critical);
        if finalize_timed {
            cratonvm_types::gpu_offload_phase_census::add(
                9,
                writeback_mark.elapsed().as_nanos() as u64,
            );
        }

        // 4. Transition status.
        let mut status = submission.status.lock();
        match (&*status, first_err) {
            (SubmissionStatus::Running, None) => {
                *status = SubmissionStatus::Completed {
                    result: scalar_result.unwrap_or(SerializedResult::Void),
                };
                submission.disarm_active_stream();
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
                submission.disarm_active_stream();
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

/// The VM identity a token names, as the registry's `u64`.
#[cfg(feature = "gpu-offload")]
fn gc_critical_vm(shared: &crate::vm::SharedVm) -> u64 {
    shared.vm_identity as u64
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

    // addr-keyed: VM identity key, not an object address
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
        let res = if ctx.is_shared_memory_gpu() {
            DeviceBuffer::<u64>::new_shared_zeroed(ctx, 1)
        } else {
            DeviceBuffer::<u64>::zeros(ctx, 1)
        };
        res.map(Arc::new).map_err(|e| e.to_string())
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

    #[derive(Clone)]
    pub enum CachedBuffer {
        /// 16-bit elements: IEEE-754 binary16 bit patterns carried in a
        /// Java `short[]`, for the fp16 built-in kernels. Nothing in the
        /// cache interprets them; only the kernel does.
        I16(Arc<DeviceBuffer<i16>>),
        I32(Arc<DeviceBuffer<i32>>),
        I64(Arc<DeviceBuffer<i64>>),
        F32(Arc<DeviceBuffer<f32>>),
        F64(Arc<DeviceBuffer<f64>>),
    }

    impl CachedBuffer {
        pub(crate) fn byte_len(&self) -> u64 {
            match self {
                Self::I16(a) => (a.len() * std::mem::size_of::<i16>()) as u64,
                Self::I32(a) => (a.len() * std::mem::size_of::<i32>()) as u64,
                Self::I64(a) => (a.len() * std::mem::size_of::<i64>()) as u64,
                Self::F32(a) => (a.len() * std::mem::size_of::<f32>()) as u64,
                Self::F64(a) => (a.len() * std::mem::size_of::<f64>()) as u64,
            }
        }

        pub(crate) fn as_any_arc(&self) -> Arc<dyn std::any::Any + Send + Sync> {
            match self {
                Self::I16(a) => a.clone(),
                Self::I32(a) => a.clone(),
                Self::I64(a) => a.clone(),
                Self::F32(a) => a.clone(),
                Self::F64(a) => a.clone(),
            }
        }
    }

    /// Phase 9 #1 — `dirty` tracks whether the device side has
    /// writes a subsequent `Native.arrayToHost` would need to
    /// pull back. Set by `mark_dirty` from the post-kernel
    /// writeback, cleared by `download_into_bytes_if_dirty`.
    pub(crate) struct Entry {
        pub buf: CachedBuffer,
        pub dirty: bool,
        pub ephemeral: bool,
        /// `true` while an owning [`StreamSubmission`] is still live
        /// (`GpuFutureImpl` not yet GC'd / `release_submission` not yet run).
        pub submission_owned: bool,
        /// Active transient consumer pins (e.g. `chain_submission_from_native`
        /// resolving inputs before the child submission retains `Arc` clones).
        pub ephemeral_pins: usize,
        pub vm_identity: Option<usize>,
    }

    pub(crate) trait CacheElem: Sized {
        fn from_cached(buf: &CachedBuffer) -> Option<Arc<DeviceBuffer<Self>>>;
    }

    impl CacheElem for i16 {
        fn from_cached(buf: &CachedBuffer) -> Option<Arc<DeviceBuffer<Self>>> {
            match buf {
                CachedBuffer::I16(a) => Some(a.clone()),
                _ => None,
            }
        }
    }

    impl CacheElem for i32 {
        fn from_cached(buf: &CachedBuffer) -> Option<Arc<DeviceBuffer<Self>>> {
            match buf {
                CachedBuffer::I32(a) => Some(a.clone()),
                _ => None,
            }
        }
    }

    impl CacheElem for i64 {
        fn from_cached(buf: &CachedBuffer) -> Option<Arc<DeviceBuffer<Self>>> {
            match buf {
                CachedBuffer::I64(a) => Some(a.clone()),
                _ => None,
            }
        }
    }

    impl CacheElem for f32 {
        fn from_cached(buf: &CachedBuffer) -> Option<Arc<DeviceBuffer<Self>>> {
            match buf {
                CachedBuffer::F32(a) => Some(a.clone()),
                _ => None,
            }
        }
    }

    impl CacheElem for f64 {
        fn from_cached(buf: &CachedBuffer) -> Option<Arc<DeviceBuffer<Self>>> {
            match buf {
                CachedBuffer::F64(a) => Some(a.clone()),
                _ => None,
            }
        }
    }

    const EPHEMERAL_HANDLE_TAG: u64 = 0x8000_0000_0000_0000;

    pub(crate) fn is_ephemeral_handle(handle: u64) -> bool {
        (handle & EPHEMERAL_HANDLE_TAG) != 0
    }

    struct CacheState {
        entries: FxHashMap<u64, Entry>,
        next_ephemeral_seq: u64,
    }

    static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();

    fn state() -> &'static Mutex<CacheState> {
        CACHE.get_or_init(|| {
            Mutex::new(CacheState {
                entries: FxHashMap::default(),
                next_ephemeral_seq: 1,
            })
        })
    }

    /// Atomically clone the `Arc<DeviceBuffer<T>>` for `handle` inside the
    /// `device_cache` lock so concurrent `release(handle)` (`StreamCleaner`)
    /// cannot drop the last reference while marshalling reads it.
    pub(crate) fn get_typed<T: CacheElem>(handle: u64) -> Option<Arc<DeviceBuffer<T>>> {
        let guard = state().lock();
        let entry = guard.entries.get(&handle)?;
        T::from_cached(&entry.buf)
    }

    /// Atomically clone the `CachedBuffer` for `handle` inside `device_cache`.
    pub(crate) fn get_cached(handle: u64) -> Option<CachedBuffer> {
        state().lock().entries.get(&handle).map(|e| e.buf.clone())
    }

    fn insert_resident(handle: u64, buf: CachedBuffer, vm_identity: usize) {
        let old = state().lock().entries.insert(
            handle,
            Entry {
                buf,
                dirty: false,
                ephemeral: false,
                submission_owned: false,
                ephemeral_pins: 0,
                vm_identity: Some(vm_identity),
            },
        );
        drop(old);
    }

    pub(crate) fn get_i16(handle: u64) -> Option<Arc<DeviceBuffer<i16>>> {
        get_typed::<i16>(handle)
    }

    pub(crate) fn put_i16(handle: u64, buf: Arc<DeviceBuffer<i16>>, vm_identity: usize) {
        insert_resident(handle, CachedBuffer::I16(buf), vm_identity);
    }

    pub(crate) fn get_i32(handle: u64) -> Option<Arc<DeviceBuffer<i32>>> {
        get_typed::<i32>(handle)
    }

    pub(crate) fn put_i32(handle: u64, buf: Arc<DeviceBuffer<i32>>, vm_identity: usize) {
        insert_resident(handle, CachedBuffer::I32(buf), vm_identity);
    }

    pub(crate) fn get_i64(handle: u64) -> Option<Arc<DeviceBuffer<i64>>> {
        get_typed::<i64>(handle)
    }

    pub(crate) fn put_i64(handle: u64, buf: Arc<DeviceBuffer<i64>>, vm_identity: usize) {
        insert_resident(handle, CachedBuffer::I64(buf), vm_identity);
    }

    pub(crate) fn get_f32(handle: u64) -> Option<Arc<DeviceBuffer<f32>>> {
        get_typed::<f32>(handle)
    }

    pub(crate) fn put_f32(handle: u64, buf: Arc<DeviceBuffer<f32>>, vm_identity: usize) {
        insert_resident(handle, CachedBuffer::F32(buf), vm_identity);
    }

    pub(crate) fn get_f64(handle: u64) -> Option<Arc<DeviceBuffer<f64>>> {
        get_typed::<f64>(handle)
    }

    pub(crate) fn put_f64(handle: u64, buf: Arc<DeviceBuffer<f64>>, vm_identity: usize) {
        insert_resident(handle, CachedBuffer::F64(buf), vm_identity);
    }

    /// Insert a kernel-produced ephemeral device buffer into `device_cache`
    /// under a distinct tagged handle (`0x8000_0000_0000_0000 | seq`).
    /// Starts with `submission_owned = true` (owned by the producing
    /// [`StreamSubmission`]) and `ephemeral_pins = 0`.
    pub(crate) fn insert_ephemeral(buf: CachedBuffer, vm_identity: usize) -> u64 {
        let mut guard = state().lock();
        let seq = guard.next_ephemeral_seq;
        guard.next_ephemeral_seq = guard.next_ephemeral_seq.wrapping_add(1).max(1);
        let handle = EPHEMERAL_HANDLE_TAG | (seq & !EPHEMERAL_HANDLE_TAG);
        guard.entries.insert(
            handle,
            Entry {
                buf,
                dirty: true,
                ephemeral: true,
                submission_owned: true,
                ephemeral_pins: 0,
                vm_identity: Some(vm_identity),
            },
        );
        handle
    }

    fn meta_from_buf(
        buf: &CachedBuffer,
    ) -> Option<(
        cratonvm_native_api::registry::DeviceArrayElem,
        usize,
        CachedBuffer,
    )> {
        use cratonvm_native_api::registry::DeviceArrayElem;
        match buf {
            CachedBuffer::I32(b) => Some((DeviceArrayElem::I32, b.len(), buf.clone())),
            CachedBuffer::I64(b) => Some((DeviceArrayElem::I64, b.len(), buf.clone())),
            CachedBuffer::F32(b) => Some((DeviceArrayElem::F32, b.len(), buf.clone())),
            CachedBuffer::F64(b) => Some((DeviceArrayElem::F64, b.len(), buf.clone())),
            CachedBuffer::I16(_) => None,
        }
    }

    /// Resolve element metadata + buffer clone for a cached array handle.
    pub(crate) fn get_entry_meta(
        handle: u64,
    ) -> Option<(
        cratonvm_native_api::registry::DeviceArrayElem,
        usize,
        CachedBuffer,
    )> {
        let guard = state().lock();
        let entry = guard.entries.get(&handle)?;
        meta_from_buf(&entry.buf)
    }

    /// Atomically clone metadata + buffer and (if ephemeral) increment
    /// `ephemeral_pins` inside the same cache lock so a concurrent cleaner or
    /// enqueue-release cannot evict `handle` while a chain dispatch reads it.
    pub(crate) fn get_entry_meta_and_pin(
        handle: u64,
    ) -> Option<(
        cratonvm_native_api::registry::DeviceArrayElem,
        usize,
        CachedBuffer,
    )> {
        let mut guard = state().lock();
        let entry = guard.entries.get_mut(&handle)?;
        let meta = meta_from_buf(&entry.buf)?;
        if entry.ephemeral {
            entry.ephemeral_pins = entry.ephemeral_pins.saturating_add(1);
        }
        Some(meta)
    }

    /// Acquire an ephemeral consumer pin while `handle` is still live.
    pub(crate) fn pin_ephemeral(handle: u64) -> bool {
        let mut guard = state().lock();
        if let Some(entry) = guard.entries.get_mut(&handle) {
            if entry.ephemeral {
                entry.ephemeral_pins = entry.ephemeral_pins.saturating_add(1);
                return true;
            }
        }
        false
    }

    /// Promote a kernel-produced ephemeral buffer to a resident user-owned
    /// `GpuArray` entry registered under `resident_handle` without host copies.
    /// Marks `dirty = true` so a later `toHost()` downloads device writes, and
    /// preserves `ephemeral_handle` until any active `ephemeral_pins` drop.
    pub(crate) fn promote_ephemeral_to_resident(
        ephemeral_handle: u64,
        resident_handle: u64,
    ) -> bool {
        if ephemeral_handle == 0 || resident_handle == 0 || ephemeral_handle == resident_handle {
            return false;
        }
        let (old_resident, old_ephemeral) = {
            let mut guard = state().lock();
            let Some(entry) = guard.entries.get_mut(&ephemeral_handle) else {
                return false;
            };
            let buf = entry.buf.clone();
            let vm_identity = entry.vm_identity;
            entry.submission_owned = false;
            let old_ephemeral = if entry.ephemeral && entry.ephemeral_pins == 0 {
                guard.entries.remove(&ephemeral_handle)
            } else {
                None
            };
            let old_resident = guard.entries.insert(
                resident_handle,
                Entry {
                    buf,
                    dirty: true,
                    ephemeral: false,
                    submission_owned: false,
                    ephemeral_pins: 0,
                    vm_identity,
                },
            );
            (old_resident, old_ephemeral)
        };
        drop(old_resident);
        drop(old_ephemeral);
        true
    }

    /// Release the owning [`StreamSubmission`]'s ownership of an ephemeral
    /// handle (called from `StreamSubmission::drop` or dispatch failure cleanup).
    /// Removes the `device_cache` entry once no consumer pins remain.
    pub(crate) fn release_ephemeral_submission_owner(handle: u64) -> bool {
        let removed = {
            let mut guard = state().lock();
            let should_remove = match guard.entries.get_mut(&handle) {
                Some(entry) if entry.ephemeral => {
                    entry.submission_owned = false;
                    entry.ephemeral_pins == 0
                }
                _ => return false,
            };
            if should_remove {
                guard.entries.remove(&handle)
            } else {
                None
            }
        };
        let evicted = removed.is_some();
        drop(removed);
        evicted
    }

    /// Release a transient ephemeral consumer pin for `handle` (idempotent if
    /// called after host materialization without an extra pin). Evicts the
    /// cache entry only when it is ephemeral, the producing submission no longer
    /// owns it (`submission_owned == false`), and `ephemeral_pins == 0`.
    pub fn release_ephemeral(handle: u64) -> bool {
        let removed = {
            let mut guard = state().lock();
            let should_remove = match guard.entries.get_mut(&handle) {
                Some(entry) if entry.ephemeral => {
                    entry.ephemeral_pins = entry.ephemeral_pins.saturating_sub(1);
                    !entry.submission_owned && entry.ephemeral_pins == 0
                }
                _ => return false,
            };
            if should_remove {
                guard.entries.remove(&handle)
            } else {
                None
            }
        };
        let evicted = removed.is_some();
        drop(removed);
        evicted
    }

    /// Drop every cached entry (resident or ephemeral) belonging to `vm_identity` on VM teardown.
    pub fn clear_vm(vm_identity: usize) {
        let evicted: Vec<Entry> = {
            let mut guard = state().lock();
            let keys: Vec<u64> = guard
                .entries
                .iter()
                .filter(|(_, e)| e.vm_identity == Some(vm_identity))
                .map(|(&k, _)| k)
                .collect();
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                if let Some(e) = guard.entries.remove(&k) {
                    out.push(e);
                }
            }
            out
        };
        drop(evicted);
    }

    /// Phase 9 #1 — flag the cache entry as "device has writes the
    /// host hasn't seen yet". Called by the Resident-variant
    /// writebacks instead of doing an eager D→H copy. A subsequent
    /// `Native.arrayToHost` consults
    /// [`download_into_bytes_if_dirty`] to materialize host bytes
    /// before returning the Java array.
    pub fn mark_dirty(handle: u64) {
        if let Some(entry) = state().lock().entries.get_mut(&handle) {
            entry.dirty = true;
        }
    }

    fn download_snapshot(arc_snapshot: CachedBufferArcs) -> Option<Vec<u8>> {
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

    /// Unconditionally download cached device buffer `handle` to little-endian
    /// bytes without clearing `dirty`. Used for fallback materialization.
    pub fn download_into_bytes(handle: u64) -> Option<Vec<u8>> {
        let arc_snapshot = {
            let guard = state().lock();
            let entry = guard.entries.get(&handle)?;
            match &entry.buf {
                CachedBuffer::I16(_) => return None,
                CachedBuffer::I32(a) => CachedBufferArcs::I32(a.clone()),
                CachedBuffer::I64(a) => CachedBufferArcs::I64(a.clone()),
                CachedBuffer::F32(a) => CachedBufferArcs::F32(a.clone()),
                CachedBuffer::F64(a) => CachedBufferArcs::F64(a.clone()),
            }
        };
        download_snapshot(arc_snapshot)
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
            let mut guard = state().lock();
            let entry = guard.entries.get_mut(&handle)?;
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
        download_snapshot(arc_snapshot)
    }

    /// Overwrite a cached device buffer in place from little-endian
    /// host bytes, keeping its device pointer.
    ///
    /// The upload counterpart of [`download_into_bytes_if_dirty`], and
    /// the piece a captured graph needs: a graph node holds the device
    /// pointer it was captured with, so new input for a replay has to
    /// be written through that pointer rather than into a fresh
    /// allocation.
    ///
    /// Answers:
    ///
    /// * `Some(true)`  — written.
    /// * `Some(false)` — the handle has a cached buffer but the bytes
    ///   do not fit it, or the copy failed. The caller must not treat
    ///   the device buffer as updated.
    /// * `None`        — no cached device buffer for this handle. Not a
    ///   failure: the array has never been marshalled, so the host-side
    ///   store is the only copy and updating it is enough. The first
    ///   dispatch that uses the array will upload it.
    ///
    /// The dirty bit is cleared: a host write makes the device side
    /// authoritative-and-equal, so there is nothing to pull back. Not
    /// clearing it would let a later `arrayToHost` overwrite the value
    /// just written with a stale download.
    pub fn upload_from_bytes(handle: u64, bytes: &[u8]) -> Option<bool> {
        // Same lock discipline as the download: snapshot the Arc under
        // the lock, copy with no lock held.
        let arc_snapshot: CachedBufferArcs = {
            let mut guard = state().lock();
            let entry = guard.entries.get_mut(&handle)?;
            entry.dirty = false;
            match &entry.buf {
                // fp16 entries carry `short[]` bit patterns; nothing in
                // this workspace writes one from the host after wrap.
                CachedBuffer::I16(_) => return Some(false),
                CachedBuffer::I32(a) => CachedBufferArcs::I32(a.clone()),
                CachedBuffer::I64(a) => CachedBufferArcs::I64(a.clone()),
                CachedBuffer::F32(a) => CachedBufferArcs::F32(a.clone()),
                CachedBuffer::F64(a) => CachedBufferArcs::F64(a.clone()),
            }
        };
        fn write<T: bytemuck::Pod + cuda_bridge::DeviceElem>(
            buf: &DeviceBuffer<T>,
            bytes: &[u8],
        ) -> bool {
            // `try_cast_slice` rather than `cast_slice`: the bytes come
            // from a Java array through two `Vec<u8>` hops, and a
            // misaligned or wrongly-sized slice must be a refusal, not
            // a panic inside a native call.
            match bytemuck::try_cast_slice::<u8, T>(bytes) {
                Ok(src) if src.len() == buf.len() => buf.copy_from_host(src).is_ok(),
                _ => false,
            }
        }
        Some(match arc_snapshot {
            CachedBufferArcs::I32(buf) => write(&buf, bytes),
            CachedBufferArcs::I64(buf) => write(&buf, bytes),
            CachedBufferArcs::F32(buf) => write(&buf, bytes),
            CachedBufferArcs::F64(buf) => write(&buf, bytes),
        })
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
        let removed = state().lock().entries.remove(&handle);
        drop(removed);
    }
}

/// Release a transient `device_cache` array handle produced by an upstream
/// `DeviceArray` submission after a downstream kernel enqueue (or host
/// materialization) has consumed it.
///
/// Only drops `ephemeral` entries; user-owned `GpuArray` handles remain resident
/// until `GpuArray.close()` / `releaseArray()`.
#[cfg(feature = "gpu-offload")]
pub fn release_ephemeral_array_after_enqueue(array_handle: u64) {
    if array_handle != 0 {
        let _ = device_cache::release_ephemeral(array_handle);
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, OnceLock};

    pub(crate) enum CachedBuffer {
        I32(Arc<DeviceBuffer<i32>>),
        I64(Arc<DeviceBuffer<i64>>),
        F32(Arc<DeviceBuffer<f32>>),
        F64(Arc<DeviceBuffer<f64>>),
        I16(Arc<DeviceBuffer<i16>>),
        I8(Arc<DeviceBuffer<i8>>),
        U16(Arc<DeviceBuffer<u16>>),
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
    // addr-keyed: outer key is VM identity; inner ObjectRef map scanned/remapped by memory::native_roots
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

    /// Buckets a COMPILED array store wrote, awaiting eviction.
    ///
    /// # Why the compiled tiers need a side table at all
    ///
    /// Every interpreted write path calls [`invalidate`] and is done.
    /// Neither compiled tier can: the single-pass backend lowers all
    /// seven primitive `*astore` opcodes inline and the IR backend
    /// lowers `Op::ArrayStore` to a raw `MOV`, so `jit_iastore` --
    /// which does invalidate -- is not reached from either. Until
    /// 2026-09-04 what stood in for a barrier was
    /// `runtime::offload_jit_gate` refusing to COMPILE any method
    /// containing one of those opcodes, which on kfusion cost the whole
    /// TornadoVM vector/image accessor family its compilation whether or
    /// not a kernel ever offloaded.
    ///
    /// Making the compiled store call `invalidate` would mean flushing
    /// caller-saved registers on the hot path to fund a call taken
    /// essentially never. Instead it sets one byte here -- see
    /// `cratonvm_jit::gpu_barrier` for the eleven-instruction sequence --
    /// and [`drain_locked`] does the eviction from Rust before anything
    /// can READ the cache.
    ///
    /// Indexed by the same `(addr >> 3) & 63` bucket [`addr_bit`] uses,
    /// so a dirty bucket names at most the entries that filter bit
    /// already names; a collision costs a re-upload. Bytes rather than
    /// bits because a byte store needs no read-modify-write and no
    /// variable-count shift, which is what keeps the emitted sequence
    /// free of a third scratch register.
    ///
    /// Process-global, like [`ADDR_FILTER`]: the drain sweeps every VM's
    /// table, so a bucket set by one VM can only over-evict in another,
    /// never under-evict.
    static DIRTY: [std::sync::atomic::AtomicU8; 64] =
        [const { std::sync::atomic::AtomicU8::new(0) }; 64];

    /// Address of [`ADDR_FILTER`], for the compiled barrier to test.
    pub(crate) fn filter_addr() -> usize {
        &ADDR_FILTER as *const AtomicU64 as usize
    }

    /// Address of [`DIRTY`]`[0]`, for the compiled barrier to mark.
    pub(crate) fn dirty_addr() -> usize {
        DIRTY.as_ptr() as usize
    }

    /// Evict every entry a compiled array store marked dirty, with the
    /// cache mutex ALREADY HELD.
    ///
    /// **The only drain.** Every getter below calls it after taking the
    /// lock and before the lookup, and [`remap_and_sweep`] calls it
    /// before the re-key -- while the table's keys are still the
    /// addresses the compiled store bucketed. That is what makes the
    /// deferral invisible: the window between a compiled store and the
    /// next drain contains no consultation of the cache, so no stale
    /// buffer can be handed out inside it.
    ///
    /// Cheap when nothing is dirty, which is every call in a run whose
    /// compiled code never stored into a cached array: one pass over a
    /// single cache line of bytes.
    ///
    /// # Why the clear must happen under the lock
    ///
    /// Until 2026-09-05 the caller cleared `DIRTY` with `swap(0)` and
    /// only then took the lock to evict. That clears the flags BEFORE
    /// performing the eviction they authorise, so a second thread
    /// entering the drain in that window saw a clean table, returned on
    /// the fast path, took the lock first and read an entry the first
    /// thread had not evicted yet:
    ///
    ///   thread A: drain            thread B: get_i32(X)
    ///     swap DIRTY[b] -> 0
    ///                                drain: DIRTY all clear, returns
    ///                                lock; reads X  <- STALE, the
    ///                                  eviction A owes has not happened
    ///     lock; evict bucket b
    ///
    /// Doing both under one hold makes the clear and the eviction atomic
    /// with respect to every other reader of the cache.
    ///
    /// The getters compounded it a second way, which is why this takes
    /// the guard rather than the lock: they used to drain through a
    /// wrapper that took and released the lock, then re-acquire it for
    /// the lookup, so even a correct drain left a gap a compiled store
    /// could land in. One acquisition now covers drain-then-lookup, and
    /// the wrapper is gone -- it had no callers left once every one of
    /// them needed the single hold.
    ///
    /// Measured on an RTX 2060: `GpuRuntimeStress` scenario 1 under
    /// `-XX:+UseGenerationalGC` failed 12/300 with the barrier armed and
    /// 0/300 with `CRATONVM_JIT_GPU_ARRAY_BARRIER=0`, alternating arms
    /// in one binary. See
    /// internal/gpu/concurrent-dispatch-wrong-answer-FIXED-20260905.md.
    // addr-keyed: outer key is VM identity; inner ObjectRef map scanned/remapped by memory::native_roots
    fn drain_locked(tables: &mut FxHashMap<usize, FxHashMap<ObjectRef, Entry>>) {
        let mut buckets = 0u64;
        for (i, b) in DIRTY.iter().enumerate() {
            if b.swap(0, Ordering::AcqRel) != 0 {
                buckets |= 1u64 << i;
            }
        }
        if buckets == 0 {
            return;
        }
        let mut removed = false;
        for table in tables.values_mut() {
            let before = table.len();
            table.retain(|obj, _| buckets & addr_bit(*obj) == 0);
            removed |= table.len() != before;
        }
        if removed {
            rebuild_filter(tables);
        }
        cratonvm_types::gpu_jit_gate_census::note_compiled_write_drain(buckets.count_ones() as u64);
    }

    // addr-keyed: outer key is VM identity; inner ObjectRef map scanned/remapped by memory::native_roots
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
    // addr-keyed: outer key is VM identity; inner ObjectRef map scanned/remapped by memory::native_roots
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
        let mut g = map().lock();
        drain_locked(&mut g);
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
        let mut g = map().lock();
        drain_locked(&mut g);
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
        let mut g = map().lock();
        drain_locked(&mut g);
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
        let mut g = map().lock();
        drain_locked(&mut g);
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
    pub(crate) fn get_i16(vm: usize, obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<i16>>> {
        let mut g = map().lock();
        drain_locked(&mut g);
        let e = g.get(&vm)?.get(&obj)?;
        if e.element_type != ArrayElementType::Short || e.len != len {
            return None;
        }
        if let CachedBuffer::I16(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_i16(vm: usize, obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<i16>>) {
        insert(
            vm,
            obj,
            Entry {
                buf: CachedBuffer::I16(buf),
                len,
                element_type: ArrayElementType::Short,
            },
        );
    }
    pub(crate) fn get_i8(vm: usize, obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<i8>>> {
        let mut g = map().lock();
        drain_locked(&mut g);
        let e = g.get(&vm)?.get(&obj)?;
        if e.element_type != ArrayElementType::Byte || e.len != len {
            return None;
        }
        if let CachedBuffer::I8(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn get_u16(vm: usize, obj: ObjectRef, len: usize) -> Option<Arc<DeviceBuffer<u16>>> {
        let mut g = map().lock();
        drain_locked(&mut g);
        let e = g.get(&vm)?.get(&obj)?;
        if e.element_type != ArrayElementType::Char || e.len != len {
            return None;
        }
        if let CachedBuffer::U16(a) = &e.buf {
            Some(a.clone())
        } else {
            None
        }
    }
    pub(crate) fn put_u16(vm: usize, obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<u16>>) {
        insert(
            vm,
            obj,
            Entry {
                buf: CachedBuffer::U16(buf),
                len,
                element_type: ArrayElementType::Char,
            },
        );
    }
    pub(crate) fn put_i8(vm: usize, obj: ObjectRef, len: usize, buf: Arc<DeviceBuffer<i8>>) {
        insert(
            vm,
            obj,
            Entry {
                buf: CachedBuffer::I8(buf),
                len,
                element_type: ArrayElementType::Byte,
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
    /// Set once a method containing a primitive array store has been
    /// admitted to the JIT, after which nothing may be cached.
    ///
    /// # The trade this replaces
    ///
    /// This cache mirrors a Java array in device memory across submits,
    /// which is only sound while every host write to that array evicts
    /// the entry. The interpreter's `*astore` arms and the
    /// `jit_iastore`/`jit_bastore` helpers all call [`invalidate`]; the
    /// JIT's IR pipeline lowers `Op::ArrayStore` to an inline
    /// `MOVSS`/`MOVSD` with no helper to hook, so there was one path
    /// that could write an array behind the cache's back.
    ///
    /// `offload_jit_gate` closed it from the other side, by refusing to
    /// COMPILE any method containing `iastore`/`lastore`/`fastore`/
    /// `dastore` while a GPU is attached. That is sound and enormously
    /// broad: it has nothing to do with whether the method has ever seen
    /// a kernel, so passing `--gpu` de-optimised the CPU half of every
    /// mixed workload — a ray tracer's setup loops, an inference
    /// pipeline's array fills — to keep coherent a cache most of those
    /// methods will never touch.
    ///
    /// AUDIT 2026-09-02 made the choice explicit rather than implicit,
    /// and measured it. Both directions are sound; the question is which
    /// side pays. Blocking the JIT costs native code on methods that may
    /// have no connection to the device. Disabling the cache costs one
    /// H2D copy per submit on arrays that are re-submitted unchanged.
    ///
    /// The second sounded bounded — "it is only PCIe bandwidth" — and on
    /// the workload the cache exists for it is 5x. `GpuWarm f 2^22 5` on
    /// an RTX 2060: `warm_ms` 2 with the cache, 10 without, and
    /// `CRATONVM_GPU_TRACE_BYTES=1` showing 48 MB once against 48 MB
    /// every submit. So blocking the JIT remains the default and this
    /// path is reached only under
    /// `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow`, for the opposite shape:
    /// a mixed workload whose CPU half does real array work around a
    /// kernel that runs once.
    ///
    /// It is still decided lazily rather than up front. The flag flips
    /// at JIT ADMISSION of the first array-writing method, which is
    /// strictly before that method's compiled code can run, so a program
    /// that never compiles one keeps the cache even under `allow`.
    ///
    /// The existing entries are dropped at the same moment
    /// ([`disable_for_jit_array_writer`]), because an array cached a
    /// moment ago is one the about-to-run compiled code may write.
    static DISABLED_BY_JIT: AtomicBool = AtomicBool::new(false);
    // addr-keyed: VM identity key, not an object address
    static DISABLED_BY_JIT_VMS: std::sync::OnceLock<parking_lot::RwLock<rustc_hash::FxHashSet<usize>>> =
        std::sync::OnceLock::new();

    // addr-keyed: VM identity key, not an object address
    fn disabled_vms() -> &'static parking_lot::RwLock<rustc_hash::FxHashSet<usize>> {
        DISABLED_BY_JIT_VMS.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashSet::default()))
    }

    /// Whether the residency cache is still accepting entries globally.
    #[cfg(test)]
    pub(crate) fn is_enabled() -> bool {
        !DISABLED_BY_JIT.load(Ordering::Acquire)
    }

    /// Whether the residency cache is accepting entries for `vm_identity`.
    pub(crate) fn is_enabled_for(vm_identity: usize) -> bool {
        !DISABLED_BY_JIT.load(Ordering::Acquire)
            && !disabled_vms().read().contains(&vm_identity)
    }

    pub(crate) fn clear_vm_disabled(vm_identity: usize) {
        if let Some(set) = DISABLED_BY_JIT_VMS.get() {
            set.write().remove(&vm_identity);
        }
    }

    /// Give up the residency cache so a method that writes a primitive
    /// array can be JIT-compiled. See [`DISABLED_BY_JIT`].
    ///
    /// Idempotent, and cheap after the first call: one relaxed-ish load
    /// on a path (`offload_jit_gate::compute`) that is already memoised
    /// per method.
    ///
    /// Ordering is the whole argument. This runs at ADMISSION — before
    /// the method is compiled, and therefore before its compiled code
    /// can execute a single store. Entries inserted before this point
    /// are dropped here; entries after are refused by [`insert`]. There
    /// is no window in which a compiled store can run against a live
    /// entry.
    ///
    /// A concurrent marshal that already took an `Arc` out of the cache
    /// keeps its buffer alive and proceeds. That is not a new race: an
    /// interpreted store racing the same marshal has always been able to
    /// `invalidate` an entry a submit had already read. The explicit
    /// async API documents that writing a kernel's input array while the
    /// kernel runs is the caller's problem; the transparent path is
    /// synchronous and cannot reach it.
    #[cfg(test)]
    pub(crate) fn disable_for_jit_array_writer() {
        if DISABLED_BY_JIT.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut tables = map().lock();
        let dropped: usize = tables.values().map(|t| t.len()).sum();
        tables.clear();
        rebuild_filter(&tables);
        drop(tables);
        tracing::info!(
            "gpu offload: input-residency cache disabled ({dropped} entr(ies) dropped) \
             so methods writing primitive arrays can be JIT-compiled. Every kernel \
             argument is re-uploaded per submit from here on."
        );
    }

    pub(crate) fn disable_for_jit_array_writer_vm(vm_identity: usize) {
        if !disabled_vms().write().insert(vm_identity) {
            return;
        }
        let mut tables = map().lock();
        let dropped = tables.remove(&vm_identity).map_or(0, |t| t.len());
        rebuild_filter(&tables);
        drop(tables);
        tracing::info!(
            vm_identity,
            "gpu offload: input-residency cache disabled for VM ({dropped} entr(ies) dropped) \
             so methods writing primitive arrays can be JIT-compiled."
        );
    }

    /// Total entries across every VM's table.
    ///
    /// Test-only, and deliberately not per-VM: what
    /// `disable_for_jit_array_writer` has to guarantee is that NOTHING is
    /// cached anywhere, not that one heap's table is empty.
    #[cfg(test)]
    pub(crate) fn table_len_for_test() -> usize {
        map().lock().values().map(|t| t.len()).sum()
    }

    /// The membership filter `invalidate` reads on every array store in
    /// the VM. Test-only; see [`ADDR_FILTER`].
    #[cfg(test)]
    pub(crate) fn addr_filter_for_test() -> u64 {
        ADDR_FILTER.load(Ordering::Acquire)
    }

    fn insert(vm: usize, obj: ObjectRef, entry: Entry) {
        // Refused once a JIT-compiled array writer exists: there would be
        // no way to evict this entry when that code stores into `obj`.
        if !is_enabled_for(vm) {
            return;
        }
        // BOTH steps under ONE hold of the cache mutex, in this order.
        //
        // The ORDER (bit, then map) is what makes the lock-free fast
        // path in `invalidate` sound: a reader must never see the entry
        // while its bit is still clear.
        //
        // Holding the LOCK across both is what makes it sound under
        // concurrency, and until 2026-09-05 the `fetch_or` sat outside
        // it. `rebuild_filter` STORES a mask recomputed from the table,
        // so an inserting thread that had OR'd its bit but not yet
        // reached the map could have that bit erased by any concurrent
        // remover:
        //
        //   thread A: insert(X)        thread B: invalidate(Y)
        //     ADDR_FILTER |= bit(X)
        //                                lock
        //                                table.remove(Y)
        //                                rebuild_filter()  <- X is not
        //                                  in the table yet, so this
        //                                  STORES a mask with bit(X)
        //                                  CLEARED
        //                                unlock
        //     lock
        //     table.insert(X, entry)    <- X is now cached with its
        //     unlock                       filter bit clear
        //
        // From then on every `invalidate(X)` takes the fast path and
        // returns, so the host's writes to X never evict the device
        // mirror and the next dispatch computes from a stale copy. A
        // wrong answer, not a slow one — and it needs no GC: it was
        // reproduced with zero collections in the run, which is why the
        // remap/sweep path above is not the one at fault.
        //
        // See internal/gpu/concurrent-dispatch-wrong-answer-FIXED-20260905.md.
        let mut tables = map().lock();
        ADDR_FILTER.fetch_or(addr_bit(obj), Ordering::AcqRel);
        tables.entry(vm).or_default().insert(obj, entry);
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
        // BEFORE the re-key, not after: a compiled store bucketed the
        // array by the address it had when it ran, and this function is
        // about to replace that address with the one the collector moved
        // it to. Draining afterwards would look for a bucket that no
        // longer matches any key and silently keep a stale entry.
        // Mutators are stopped here, so nothing can dirty a bucket
        // between the drain and the re-key.
        let mut tables = map().lock();
        drain_locked(&mut tables);
        // Only this VM's table: `heap` belongs to `vm`, and asking it
        // about another VM's addresses would report every one of them
        // dead and silently flush that VM's cache.
        let Some(table) = tables.get_mut(&vm) else {
            return;
        };
        // `survived_in_place`, not a bare `is_object_address` (gc-common w3-g,
        // `handoff-w2g-sweep-address-keyed-tables-after-concurrent-reclamation`
        // edit 3): a freed Generational old-gen block still parses as its dead
        // array (`OldGen::free` does not zero) and a zeroed young span parses
        // as a field-less object, so the bare test kept a dead array's row and
        // served its device buffer to the next array placed on the address.
        let stats = crate::memory::addr_keyed::remap_and_sweep(table, pointer_map, &|addr| {
            crate::memory::addr_keyed::survived_in_place(heap, addr)
        });
        // Counted BEFORE the early return, and counting the collections
        // that changed nothing too. A run that reports `collections=25
        // re-keyed=0` has been through 25 collections without one of
        // them moving a cached array -- which is a different fact from
        // `collections=0`, and telling them apart is the whole reason
        // this is here. The `tracing::debug!` below cannot: `tracing`
        // is built with `max_level_info`, so it is compiled out of
        // every release binary.
        cratonvm_types::gpu_residency_census::note_gc(
            stats.moved as u64,
            stats.retained as u64,
            stats.dropped as u64,
        );
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
    I16 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i16>>,
        host: std::sync::Arc<cuda_bridge::PinnedHostBuffer<i16>>,
    },
    I8 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i8>>,
        host: std::sync::Arc<cuda_bridge::PinnedHostBuffer<i8>>,
    },
    U16 {
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<u16>>,
        host: std::sync::Arc<cuda_bridge::PinnedHostBuffer<u16>>,
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
/// Chunks the iteration space is split into by default.
/// Set to 1 (disabled / unchunked monolithic DMA) by default to avoid the 2x
/// writeback latency penalty on large framebuffers (e.g. RayTracer 8K UHD).
/// Operators can opt in to chunked writeback with GC safepoint pacing via
/// `CRATONVM_GPU_CHUNKS=8`.
#[cfg(feature = "gpu-offload")]
const CHUNK_COUNT_DEFAULT: usize = 1;

/// Streams the handle-less dispatch path rotates over, instead of
/// creating and destroying one per submission.
///
/// Four, not one, and not eight. One would be enough for the
/// transparent interpreter path on its own — it finalizes before
/// returning, so it never has two launches in flight — but
/// `dispatch_method_from_native` is also reachable from several Java
/// threads at once, and giving those a shared stream would serialise
/// dispatches the driver could have overlapped. Four covers that without
/// holding open more driver objects than a program that never offloads
/// anything would want to pay for.
///
/// Override with `CRATONVM_GPU_DISPATCH_STREAMS`, and set it to 1 to get
/// the strictest ordering if a bug is ever suspected to be one of
/// stream concurrency.
#[cfg(feature = "gpu-offload")]
const DISPATCH_STREAM_POOL_DEFAULT: usize = 4;

/// See [`DISPATCH_STREAM_POOL_DEFAULT`].
#[cfg(feature = "gpu-offload")]
#[inline(always)]
fn dispatch_stream_pool_size() -> usize {
    DISPATCH_STREAM_POOL_DEFAULT
}

#[cfg(feature = "gpu-offload")]
#[inline(always)]
fn chunk_streams_wanted() -> usize {
    CHUNK_STREAMS_DEFAULT
}

#[cfg(feature = "gpu-offload")]
#[inline(always)]
fn chunk_count_wanted() -> usize {
    CHUNK_COUNT_DEFAULT
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
/// # This no longer breaks the "no partial GPU state" rule
///
/// A chunked writeback USED TO commit each chunk into the Java array as
/// that chunk's event fired, which is BEFORE the bounds-failure flag has
/// been read, and `finalize_submission` drains the flag first precisely so
/// a failed kernel leaves the heap untouched and the CPU re-run starts
/// clean (docs/book/src/gpu/overview.md, Exceptions).
///
/// The relaxation was justified like this, for an array the kernel writes
/// and never READS:
///
///   * a committed chunk never contains garbage, because the flag is set
///     by the failing thread and not by its neighbours; and
///   * on deopt the interpreter re-runs the whole method from iteration 0,
///     rewriting every element it would have written and throwing at the
///     same index, "so the committed elements are a subset of what plain
///     Java writes before the throw".
///
/// **The second bullet was wrong, and it was the load-bearing one.** It
/// assumes sequential execution — everything before the throw ran,
/// everything after it did not. GPU threads are concurrent: when thread
/// `i` fails its bounds check, the threads for `j > i` have already run
/// and written `a[j]`, and those chunks commit. The CPU re-run then throws
/// at `i` having written only `[0, i)`, and the GPU's values stay live in
/// `[i, n)` — elements plain Java never wrote at all. Not a subset, and a
/// heap state no JVM can produce.
///
/// So the commit is no longer early. `finalize_submission` splits the
/// drain in two: `MarshalWriteback::sync_chunks` waits for the staging
/// DMAs ahead of the flag (touching only device memory and this
/// submission's pinned slab, which is where the overlap win actually
/// comes from), and the commit into the Java array runs after the flag
/// alongside every other array writeback, protected by the same `break`.
///
/// The `writes & !reads` gate below is KEPT. It is no longer load-bearing
/// for the failure case, but a chunked array that the kernel also reads
/// would make a partial commit the re-run's own input, and nothing else
/// rules that out.
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
                    | MarshalWriteback::I16 { .. }
                    | MarshalWriteback::I8 { .. }
                    | MarshalWriteback::U16 { .. }
            );
            // I16/I8 joined this set on 2026-09-03. They were excluded
            // when short[]/byte[] became offloadable because
            // `ChunkedStage` had no arm for them, which cost the whole
            // dispatch its copy/compute overlap rather than just theirs.
            // They now have staging slots, stage variants and drain arms
            // like the other four.
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
    // Shared-memory host-mapped writebacks already write in-place to DRAM.
    if writebacks[idx].is_shared_host_mapped() {
        return None;
    }
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
        MarshalWriteback::I16 { obj, buf, len } => (
            obj,
            ChunkedStage::I16 {
                buf,
                host: cache.staging_i16(ctx, len)?,
            },
        ),
        MarshalWriteback::I8 { obj, buf, len } => (
            obj,
            ChunkedStage::I8 {
                buf,
                host: cache.staging_i8(ctx, len)?,
            },
        ),
        MarshalWriteback::U16 { obj, buf, len } => (
            obj,
            ChunkedStage::U16 {
                buf,
                host: cache.staging_u16(ctx, len)?,
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
            ChunkedStage::I16 { buf, host } => copy_chunk!(buf, host),
            ChunkedStage::I8 { buf, host } => copy_chunk!(buf, host),
            ChunkedStage::U16 { buf, host } => copy_chunk!(buf, host),
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
#[cfg(feature = "gpu-offload")]
staging_slot!(staging_i16, i16, chunk_stage_i16);
#[cfg(feature = "gpu-offload")]
staging_slot!(staging_i8, i8, chunk_stage_i8);
#[cfg(feature = "gpu-offload")]
staging_slot!(staging_u16, u16, chunk_stage_u16);

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
    I16 {
        obj: cratonvm_types::ObjectRef,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i16>>,
        len: usize,
    },
    I8 {
        obj: cratonvm_types::ObjectRef,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<i8>>,
        len: usize,
    },
    U16 {
        obj: cratonvm_types::ObjectRef,
        buf: std::sync::Arc<cuda_bridge::DeviceBuffer<u16>>,
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
        ///
        /// `None` for the one flag a captured graph shares across all
        /// of its launches: that buffer's address is baked into every
        /// node of the graph, so returning it to the pool would hand it
        /// to an unrelated dispatch that the graph then overwrites.
        /// It is freed when the graph is.
        pool_key: Option<usize>,
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
    /// Ephemeral GPU-resident output array for a kernel returning a primitive
    /// array (`int[]`/`long[]`/`float[]`/`double[]`). Kept on device in
    /// `device_cache` under `array_handle`; `writeback` marks it dirty and
    /// surfaces `SerializedResult::DeviceArray` without an eager `to_host`.
    DeviceArrayOutput {
        array_handle: u64,
        elem: cratonvm_native_api::registry::DeviceArrayElem,
        len: usize,
        buf: device_cache::CachedBuffer,
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
    /// Wait for every chunk's DMA into page-locked staging, and do NOTHING
    /// else. A no-op for every variant but [`Self::Chunked`].
    ///
    /// This is the half of the chunked drain that has to happen before the
    /// failure flag is read, and it is the half that carries the whole
    /// performance argument: waiting on chunk 0 while chunks 1..n are still
    /// transferring is what overlaps the copy with the GPU work in flight.
    /// It touches only device memory and this submission's own pinned staging
    /// slab, never the Java heap, so a kernel that later turns out to have
    /// failed has published nothing.
    ///
    /// The commit half — staging into the Java array — stays in
    /// [`Self::writeback`] and runs after the flag, like every other array
    /// writeback.
    fn sync_chunks(&self) -> Result<(), String> {
        let Self::Chunked { chunks, .. } = self else {
            return Ok(());
        };
        for chunk in chunks {
            chunk
                .done
                .synchronize()
                .map_err(|e| format!("chunked staging wait (lo={}): {e}", chunk.lo))?;
        }
        Ok(())
    }

    /// at most one `ret_ptr`), so "last" is really "the only one".
    fn writeback(
        &self,
        shared: &crate::vm::SharedVm,
        token: &cratonvm_gc::safepoint::SafepointToken<'_>,
        d2h_ctx: Option<(
            &cuda_bridge::Stream,
            &cuda_bridge::pinned_pool::HostRegionPinCache,
            &cuda_bridge::pinned_pool::PinnedHostPool,
        )>,
        d2h_guards: &mut Vec<crate::runtime::gpu_marshal::AsyncDmaLifetime>,
    ) -> Result<Option<SerializedResult>, String> {
        use crate::runtime::gpu_marshal;
        macro_rules! d2h_arm {
            ($obj:expr, $buf:expr, $async_down:path, $sync_down:path, $tag:literal) => {{
                if $buf.is_shared_host_mapped() {
                    if cuda_bridge::backend_kind() == cuda_bridge::BackendKind::OpenCl {
                        $sync_down($buf.as_ref(), *$obj, &shared.mem.heap, token)
                            .map_err(|e| format!("sync shared {}: {e}", $tag))?;
                    }
                    return Ok(None);
                }
                if let Some((stream, pin_cache, pool)) = d2h_ctx {
                    let guard = $async_down(
                        $buf.as_ref(),
                        *$obj,
                        &shared.mem.heap,
                        token,
                        stream,
                        pin_cache,
                        pool,
                    )
                    .map_err(|e| format!("download {}: {e}", $tag))?;
                    d2h_guards.push(guard);
                } else {
                    $sync_down($buf.as_ref(), *$obj, &shared.mem.heap, token)
                        .map_err(|e| format!("download {}: {e}", $tag))?;
                }
                Ok(None)
            }};
        }
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
                    ChunkedStage::I16 { host, .. } => {
                        drain!(host, gpu_marshal::write_back_range_i16, "i16")
                    }
                    ChunkedStage::I8 { host, .. } => {
                        drain!(host, gpu_marshal::write_back_range_i8, "i8")
                    }
                    ChunkedStage::U16 { host, .. } => {
                        drain!(host, gpu_marshal::write_back_range_u16, "u16")
                    }
                }
                Ok(None)
            }
            Self::I32 { obj, buf, .. } => d2h_arm!(
                obj,
                buf,
                gpu_marshal::download_obj_async_i32,
                gpu_marshal::download_obj_i32,
                "i32"
            ),
            Self::I64 { obj, buf, .. } => d2h_arm!(
                obj,
                buf,
                gpu_marshal::download_obj_async_i64,
                gpu_marshal::download_obj_i64,
                "i64"
            ),
            Self::F32 { obj, buf, .. } => d2h_arm!(
                obj,
                buf,
                gpu_marshal::download_obj_async_f32,
                gpu_marshal::download_obj_f32,
                "f32"
            ),
            Self::F64 { obj, buf, .. } => d2h_arm!(
                obj,
                buf,
                gpu_marshal::download_obj_async_f64,
                gpu_marshal::download_obj_f64,
                "f64"
            ),
            Self::I16 { obj, buf, .. } => d2h_arm!(
                obj,
                buf,
                gpu_marshal::download_obj_async_i16,
                gpu_marshal::download_obj_i16,
                "i16"
            ),
            Self::I8 { obj, buf, .. } => d2h_arm!(
                obj,
                buf,
                gpu_marshal::download_obj_async_i8,
                gpu_marshal::download_obj_i8,
                "i8"
            ),
            Self::U16 { obj, buf, .. } => d2h_arm!(
                obj,
                buf,
                gpu_marshal::download_obj_async_u16,
                gpu_marshal::download_obj_u16,
                "u16 (char[])"
            ),
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
                if let Some(key) = pool_key {
                    flag_pool::give(*key, buf);
                }
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
            Self::DeviceArrayOutput {
                array_handle,
                elem,
                len,
                ..
            } => {
                device_cache::mark_dirty(*array_handle);
                Ok(Some(SerializedResult::DeviceArray {
                    array_handle: *array_handle,
                    elem: *elem,
                    len: *len,
                }))
            }
        }
    }

    /// Element-count of the array this writeback owns. Used by
    /// `dispatch_method_from_native` to compute the launch grid:
    /// the kernel needs `>= max(array_len)` threads to cover every
    /// output index. Returns `None` for the `FailureFlag` and
    /// `Scalar*` variants (no array body — just a 1-cell signal).
    /// Whether this writeback lands in the JVM heap or in a host
    /// buffer allocated for one dispatch.
    ///
    /// The dividing line a graph capture refuses on. A replay re-runs
    /// the recorded launches against the device addresses they were
    /// captured with; the resident variants name a `GpuArray` whose
    /// buffer lives for the life of the process, but these name a
    /// device buffer allocated for this dispatch and a host destination
    /// that was current when it was made. Capturing one bakes in a
    /// pointer that is freed before the first replay.
    /// `true` when this writeback backs a zero-copy host-mapped shared buffer.
    pub fn is_shared_host_mapped(&self) -> bool {
        match self {
            Self::I32 { buf, .. } => buf.is_shared_host_mapped(),
            Self::I64 { buf, .. } => buf.is_shared_host_mapped(),
            Self::F32 { buf, .. } => buf.is_shared_host_mapped(),
            Self::F64 { buf, .. } => buf.is_shared_host_mapped(),
            Self::I16 { buf, .. } => buf.is_shared_host_mapped(),
            Self::I8 { buf, .. } => buf.is_shared_host_mapped(),
            Self::U16 { buf, .. } => buf.is_shared_host_mapped(),
            _ => false,
        }
    }

    pub fn is_per_dispatch_host_target(&self) -> bool {
        matches!(
            self,
            Self::Chunked { .. }
                | Self::I32 { .. }
                | Self::I64 { .. }
                | Self::F32 { .. }
                | Self::F64 { .. }
                | Self::I16 { .. }
                | Self::I8 { .. }
                | Self::U16 { .. }
        )
    }

    /// The resident-store handle this writeback updates, if any.
    ///
    /// A replay runs the kernels but no writeback, so nothing would
    /// otherwise record that the device side of these arrays moved
    /// ahead of the host mirror -- and `GpuArray.toHost` would answer
    /// with bytes from before the replay. `graph_replay` marks each of
    /// these dirty instead, which costs a bit per handle and defers the
    /// download to whoever actually reads one.
    pub fn resident_handle(&self) -> Option<u64> {
        match self {
            Self::ResidentI32 { handle, .. }
            | Self::ResidentI64 { handle, .. }
            | Self::ResidentF32 { handle, .. }
            | Self::ResidentF64 { handle, .. } => Some(*handle),
            _ => None,
        }
    }

    /// The Java array this writeback lands in, if it lands in one.
    ///
    /// These are the addresses a submission declares as keep-alive roots
    /// at dispatch; the resident and cell variants name device buffers
    /// or the resident store, never the heap.
    pub fn heap_target(&self) -> Option<cratonvm_types::ObjectRef> {
        match self {
            Self::Chunked { obj, .. }
            | Self::I32 { obj, .. }
            | Self::I64 { obj, .. }
            | Self::F32 { obj, .. }
            | Self::F64 { obj, .. }
            | Self::I16 { obj, .. }
            | Self::I8 { obj, .. }
            | Self::U16 { obj, .. } => Some(*obj),
            _ => None,
        }
    }

    /// Mutable twin of [`MarshalWriteback::heap_target`], for the
    /// finalize path to install the collector's current address before
    /// writing. Same variant order, so `zip` with
    /// `GcCriticalGuard::keepalive_addrs` pairs each target with its own
    /// declaration.
    pub fn heap_target_mut(&mut self) -> Option<&mut cratonvm_types::ObjectRef> {
        match self {
            Self::Chunked { obj, .. }
            | Self::I32 { obj, .. }
            | Self::I64 { obj, .. }
            | Self::F32 { obj, .. }
            | Self::F64 { obj, .. }
            | Self::I16 { obj, .. }
            | Self::I8 { obj, .. }
            | Self::U16 { obj, .. } => Some(obj),
            _ => None,
        }
    }

    pub fn array_len(&self) -> Option<usize> {
        match self {
            // A chunked writeback covers the whole array; its length is
            // the sum of the chunks, which by construction tile it.
            Self::Chunked { chunks, .. } => Some(chunks.iter().map(|c| c.len).sum()),
            Self::I32 { len, .. }
            | Self::I64 { len, .. }
            | Self::F32 { len, .. }
            | Self::F64 { len, .. }
            | Self::I16 { len, .. }
            | Self::I8 { len, .. }
            | Self::U16 { len, .. }
            | Self::ResidentI32 { len, .. }
            | Self::ResidentI64 { len, .. }
            | Self::ResidentF32 { len, .. }
            | Self::ResidentF64 { len, .. }
            | Self::DeviceArrayOutput { len, .. } => Some(*len),
            Self::FailureFlag { .. }
            | Self::ScalarI32 { .. }
            | Self::ScalarI64 { .. }
            | Self::ScalarF32 { .. }
            | Self::ScalarF64 { .. } => None,
        }
    }
}

include!("offload/marshalling.rs");

#[cfg(all(test, feature = "gpu-offload"))]
mod marshaller_analyzer_agreement;

#[cfg(all(test, feature = "gpu-offload"))]
mod gpu_gc_roadmap_unit_tests;
