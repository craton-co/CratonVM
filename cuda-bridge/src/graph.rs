// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! CUDA graph capture and replay.
//!
//! # Why
//!
//! A kernel launch costs the host something whether or not the device is
//! busy, and a workload that issues hundreds of small launches per unit of
//! work pays that cost hundreds of times. GPULlama3's forward pass is 453
//! launches per token on one stream: 14 ms of host time against 24 ms of
//! device time on an idle box, and 60 ms against 5 ms on a loaded one — the
//! device finishes and waits.
//!
//! A graph turns that whole sequence into one object the driver already
//! knows the shape of. Capture records the launches instead of issuing
//! them; instantiation resolves them once; replay is a single
//! `cuGraphLaunch` that issues all of them with no per-launch host work at
//! all.
//!
//! # Why the FFI is here
//!
//! `cudarc` 0.13 ships the raw `sys` symbols for all of this and no safe
//! wrappers for any of it, which is exactly the gap this crate exists to
//! fill. Everything below is the same pattern as [`crate::event`] and
//! [`crate::stream`]: hold the raw handle, bind the owning primary context
//! before touching it, and free it in `Drop`.
//!
//! # The one non-obvious call
//!
//! [`Stream::capturing_node`] is what makes selective replay possible.
//! After a launch is captured, `cuStreamGetCaptureInfo_v2` reports the
//! capture graph's current dependency set — for a linear single-stream
//! capture that is exactly the one node the launch just added. Recording it
//! per launch gives every dispatch a stable node identity, so a later
//! replay can update just the arguments that changed instead of
//! re-capturing -- see "What is deliberately not here yet". The
//! alternative — reading `cuGraphGetNodes` afterwards and assuming its
//! order matches launch order — is not a documented guarantee.
//!
//! # Updating a node's arguments between replays
//!
//! [`GraphExec::set_kernel_node_args`] is what makes a graph usable by a
//! caller whose arguments change. A graph bakes each argument VALUE into
//! its nodes, so without this the only way to feed a replay new input is
//! to put the changing values in device memory and write through a
//! pointer the graph already holds — which works, and is faster, but
//! requires changing the code that issues the launches. Not every caller
//! can do that.
//!
//! The update deliberately re-reads `func`, the grid, the block and the
//! shared-memory size from the node itself rather than taking them from
//! the caller. An argument update must be able to change arguments and
//! nothing else: a caller who passed a different grid would silently get
//! a graph that no longer matches the sequence it captured, and the
//! failure would appear as wrong output rather than as an error.

//! # One signature, two backends
//!
//! The crate builds without a driver so that everything above it — the
//! VM's offload path, its natives, its tests — compiles and runs on a
//! machine with no GPU. Every entry point below therefore has a
//! driverless arm, written as a `#[cfg]` inside the body rather than as
//! a second `impl` block.
//!
//! That is not a style preference. Two parallel `impl` blocks let the
//! signatures drift, and one did: `instantiate` was changed to consume
//! its `Graph` — the whole point being that a node handle cannot outlive
//! the graph it names — and the stub copy kept `&self`. Nothing caught
//! it, because `graph.instantiate()` compiles against either receiver
//! and the stub returns before it constructs anything. The ownership
//! rule simply was not a rule in stub builds. With one signature there
//! is nowhere for that to hide.
//!
//! No driverless arm reports success. A capture that quietly produced an
//! empty graph would replay successfully and run no kernels, which is the
//! one failure this whole mechanism must never have. The single
//! exception is [`Stream::capturing_node`], which answers `Ok(None)`
//! because "am I capturing?" has a true answer without a driver and
//! callers use it to choose a path rather than to do work.

use crate::{DeviceContext, DeviceError, Event, KernelArgs, Result, Stream};

/// What a capture does to work submitted on *other* threads' streams.
///
/// Only the thread-local mode is exposed. `Global` makes any unsafe
/// concurrent operation anywhere in the process invalidate the capture,
/// which in a JVM — with a collector, a JIT and a completion reaper all
/// running — is a capture that fails for reasons the caller cannot see or
/// control. `Relaxed` disables the safety checks entirely. Thread-local is
/// the only one whose failure modes belong to the caller.
#[derive(Debug, Clone, Copy)]
pub enum CaptureMode {
    /// Only this thread's activity is checked against the capture.
    ThreadLocal,
}

impl CaptureMode {
    #[cfg(feature = "cuda")]
    fn raw(self) -> cudarc::driver::sys::CUstreamCaptureMode {
        match self {
            CaptureMode::ThreadLocal => {
                cudarc::driver::sys::CUstreamCaptureMode::CU_STREAM_CAPTURE_MODE_THREAD_LOCAL
            }
        }
    }
}

/// A captured, not-yet-runnable graph.
///
/// Produced by [`Stream::end_capture`] and consumed by
/// [`Graph::instantiate`]. Holding one costs device-side memory for the
/// recorded topology and nothing else; it is not schedulable until
/// instantiated.
pub struct Graph {
    #[cfg(feature = "cuda")]
    raw: cudarc::driver::sys::CUgraph,
    #[cfg(feature = "cuda")]
    device: std::sync::Arc<cudarc::driver::safe::CudaDevice>,
    /// The `last_write` slots of every buffer the capture named as a
    /// kernel argument. See [`GraphExec::launch`].
    slots: Vec<crate::LastWriteSlot>,
}

// SAFETY: mirrors `EventCuda`. The handle is only ever touched after
// `bind_to_thread` on the retained owning device, and `Drop` re-binds
// before destroying.
unsafe impl Send for Graph {}
// SAFETY: every method takes `&self` and performs one driver call under the
// bound context; the driver serialises graph operations.
unsafe impl Sync for Graph {}

#[cfg(feature = "cuda")]
impl Drop for Graph {
    fn drop(&mut self) {
        let _ = self.device.bind_to_thread();
        // SAFETY: uniquely owned here, and its owning context was bound
        // immediately above.
        unsafe {
            let _ = cudarc::driver::sys::lib().cuGraphDestroy(self.raw);
        }
    }
}

impl Graph {
    /// Resolve the graph into something launchable.
    ///
    /// This is where the driver does the work a per-launch dispatch would
    /// otherwise repeat: validating the topology, resolving the kernels and
    /// laying out the argument buffers. It is expensive and it happens
    /// once.
    pub fn instantiate(self) -> Result<GraphExec> {
        #[cfg(feature = "cuda")]
        {
            self.device
                .bind_to_thread()
                .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
            let mut exec: cudarc::driver::sys::CUgraphExec = std::ptr::null_mut();
            // SAFETY: `self.raw` is a live graph on the bound context, and
            // `exec` is a valid out-pointer for the duration of the call.
            let status = unsafe {
                cudarc::driver::sys::lib().cuGraphInstantiateWithFlags(&mut exec, self.raw, 0)
            };
            if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!(
                    "cuGraphInstantiateWithFlags: {status:?}"
                )));
            }
            Ok(GraphExec {
                raw: exec,
                device: self.device.clone(),
                slots: self.slots.clone(),
                _graph: self,
            })
        }
        #[cfg(not(feature = "cuda"))]
        {
            // Nothing was captured, so there is nothing to instantiate.
            let _ = self;
            Err(DeviceError::NoDriver)
        }
    }

    /// How many nodes the capture recorded.
    ///
    /// The count a caller checks against its own launch count. A capture
    /// that recorded fewer nodes than the caller issued launches means
    /// something else on this thread was folded into the graph, and that is
    /// worth failing on rather than replaying.
    pub fn node_count(&self) -> Result<usize> {
        #[cfg(feature = "cuda")]
        {
            self.device
                .bind_to_thread()
                .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
            let mut n: usize = 0;
            // SAFETY: a null node array with a valid count out-pointer is the
            // documented "just count them" form.
            let status = unsafe {
                cudarc::driver::sys::lib().cuGraphGetNodes(self.raw, std::ptr::null_mut(), &mut n)
            };
            if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!("cuGraphGetNodes: {status:?}")));
            }
            Ok(n)
        }
        #[cfg(not(feature = "cuda"))]
        {
            // No driver, so there is no graph to count.
            Err(DeviceError::NoDriver)
        }
    }
}

/// An instantiated graph: one `cuGraphLaunch` runs every launch it holds.
pub struct GraphExec {
    #[cfg(feature = "cuda")]
    raw: cudarc::driver::sys::CUgraphExec,
    #[cfg(feature = "cuda")]
    device: std::sync::Arc<cudarc::driver::safe::CudaDevice>,
    /// Carried from the [`Graph`]; stamped on every replay.
    slots: Vec<crate::LastWriteSlot>,
    /// The graph this was instantiated from, kept alive for exactly as
    /// long as the exec.
    ///
    /// The driver permits destroying a graph after instantiating it, and
    /// an exec on its own stays perfectly valid — but a [`GraphNode`]
    /// belongs to the GRAPH, and
    /// [`GraphExec::set_kernel_node_args`] needs one. With the graph
    /// gone those handles dangle, and the driver does not say so:
    /// `cuGraphKernelNodeGetParams` on a destroyed graph's node returns
    /// `CUDA_SUCCESS` and an all-zero struct, so the failure surfaces
    /// one call later as `CUDA_ERROR_INVALID_VALUE` from
    /// `cuGraphExecKernelNodeSetParams` and points nowhere near the
    /// cause. Taking ownership makes it unrepresentable: `instantiate`
    /// consumes the graph, so a node handle cannot outlive it.
    ///
    /// Dropped after `Drop for GraphExec` has run `cuGraphExecDestroy`,
    /// which is the order the driver documents.
    _graph: Graph,
}

// SAFETY: as `Graph`.
unsafe impl Send for GraphExec {}
// SAFETY: as `Graph`.
unsafe impl Sync for GraphExec {}

#[cfg(feature = "cuda")]
impl Drop for GraphExec {
    fn drop(&mut self) {
        let _ = self.device.bind_to_thread();
        // SAFETY: uniquely owned here; context bound immediately above.
        unsafe {
            let _ = cudarc::driver::sys::lib().cuGraphExecDestroy(self.raw);
        }
    }
}

impl GraphExec {
    /// Submit every launch in the graph onto `stream`, and answer the
    /// event that fires when they are all done.
    ///
    /// Asynchronous, exactly like a single launch: the call returns once
    /// the work is queued.
    ///
    /// # Why it records an event rather than leaving that to the caller
    ///
    /// The returned event is not a convenience. Every buffer this graph
    /// names as a kernel argument had its `last_write` slot emptied when
    /// the launch was captured -- an event recorded on a capturing
    /// stream lives inside the graph and no other stream can wait on it,
    /// so there was nothing valid to leave there. This stamps all of
    /// them with the event below, which restores the invariant the rest
    /// of the crate depends on: a buffer's `last_write` names the work
    /// that most recently wrote it, and any stream that later reads the
    /// buffer waits on that.
    ///
    /// Without this, a replay's writes would be invisible to every
    /// stream except the one it ran on. The caller awaiting its own
    /// submission covers the single-stream case, which is the only one
    /// the VM uses today -- but "correct as long as nobody uses a second
    /// stream" is not a property worth shipping when one event fixes it.
    ///
    /// Stamping inputs as well as outputs is deliberate and matches
    /// `launch_raw_on_stream_inner` exactly: it cannot tell which
    /// arguments the kernel wrote, so it treats every device-pointer
    /// argument as written. Conservative, never wrong.
    pub fn launch(&self, ctx: &DeviceContext, stream: &Stream) -> Result<std::sync::Arc<Event>> {
        #[cfg(feature = "cuda")]
        {
            self.device
                .bind_to_thread()
                .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
            // SAFETY: both handles live on the bound context and the call only
            // enqueues.
            let status =
                unsafe { cudarc::driver::sys::lib().cuGraphLaunch(self.raw, stream.raw()) };
            if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!("cuGraphLaunch: {status:?}")));
            }
            let done = std::sync::Arc::new(Event::new(ctx)?);
            stream.record_event(&done)?;
            for slot in &self.slots {
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(done.clone());
            }
            Ok(done)
        }
        #[cfg(not(feature = "cuda"))]
        {
            // Nothing to launch, and no event to answer with.
            let _ = (ctx, stream);
            Err(DeviceError::NoDriver)
        }
    }

    /// Replace the arguments of one captured node, in the instantiated
    /// graph, without re-capturing.
    ///
    /// `node` comes from [`Stream::capturing_node`] at the time the
    /// launch was recorded. Everything about the node except its
    /// arguments — the function, the grid, the block, the shared-memory
    /// size — is read back from the node and passed through unchanged;
    /// see the module docs for why that is not the caller's to supply.
    ///
    /// # Cost
    ///
    /// One driver call per updated node. That makes a graph whose
    /// arguments change every replay cheaper than re-issuing its
    /// launches but more expensive than a graph that does not change at
    /// all, so a caller with the option should still prefer moving the
    /// changing values into device memory. This exists for callers
    /// without the option.
    ///
    /// # Safety of the argument stores
    ///
    /// `cuGraphExecKernelNodeSetParams` copies the pointed-at parameter
    /// bytes before returning, exactly as a launch does, so the two
    /// backing stores this builds — the `KernelArgs` vec for scalars and
    /// a local `Vec<u64>` for device addresses — only need to outlive
    /// the call. Both are bound for the whole function and anchored
    /// after it.
    pub fn set_kernel_node_args(&self, node: GraphNode, args: &KernelArgs) -> Result<()> {
        #[cfg(feature = "cuda")]
        {
            self.device
                .bind_to_thread()
                .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;

            // Read the node's current parameters. This is what supplies
            // `func` -- cudarc keeps `CudaFunction`'s raw handle private, and
            // asking the node is better than reaching for it anyway: the
            // launch shape then cannot drift from what was captured.
            let mut params = cudarc::driver::sys::CUDA_KERNEL_NODE_PARAMS::default();
            // SAFETY: `node` is a node of the graph this exec was
            // instantiated from, and `params` is a valid out-pointer.
            let rc = unsafe {
                cudarc::driver::sys::lib().cuGraphKernelNodeGetParams_v2(node.0, &mut params)
            };
            if rc != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!(
                    "cuGraphKernelNodeGetParams_v2: {rc:?}"
                )));
            }

            // Two backing stores, the same shape the launch path documents:
            // device addresses need a stable 8-byte slot to point AT, and
            // scalars are pointed at directly inside `args.raw`.
            let mut addrs: Vec<u64> = Vec::with_capacity(args.raw.len());
            for a in &args.raw {
                if let crate::KernelArg::DevicePtr { addr, .. } = a {
                    addrs.push(*addr);
                }
            }
            let mut next = 0usize;
            let mut param_ptrs: Vec<*mut std::ffi::c_void> = args
                .raw
                .iter()
                .map(|a| match a {
                    crate::KernelArg::I32(v) => v as *const i32 as *mut std::ffi::c_void,
                    crate::KernelArg::I64(v) => v as *const i64 as *mut std::ffi::c_void,
                    crate::KernelArg::F32(v) => v as *const f32 as *mut std::ffi::c_void,
                    crate::KernelArg::F64(v) => v as *const f64 as *mut std::ffi::c_void,
                    crate::KernelArg::DevicePtr { .. } => {
                        let slot = &addrs[next] as *const u64 as *mut std::ffi::c_void;
                        next += 1;
                        slot
                    }
                })
                .collect();

            params.kernelParams = param_ptrs.as_mut_ptr();
            // v2 params carry `func` AND a `kern`/`ctx` pair, and the driver
            // reads `kern` only when `func` is null. The getter returns both
            // halves populated; passing them straight back is what the
            // driver rejects with INVALID_VALUE. Keep `func`, which is the
            // handle the capture actually recorded.
            params.kern = std::ptr::null_mut();
            params.ctx = std::ptr::null_mut();
            // `extra` and `kernelParams` are mutually exclusive; the getter
            // may have returned a non-null `extra` and passing both is an
            // error. We supply arguments the `kernelParams` way, as the
            // launch path does.
            params.extra = std::ptr::null_mut();

            // SAFETY: every pointer in `param_ptrs` borrows into `args.raw`
            // or `addrs`, both alive here and un-reallocated since the
            // pointers were taken; the driver copies the parameter bytes
            // before returning.
            let rc = unsafe {
                cudarc::driver::sys::lib()
                    .cuGraphExecKernelNodeSetParams_v2(self.raw, node.0, &params)
            };
            // Liveness anchor: the call has returned, so the raw pointers are
            // no longer dereferenced. Naming both stores here makes a future
            // refactor that drops either one early fail to compile.
            let _keep_alive = (&args.raw, &addrs, &param_ptrs);
            if rc != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!(
                    "cuGraphExecKernelNodeSetParams_v2: {rc:?}"
                )));
            }
            Ok(())
        }
        #[cfg(not(feature = "cuda"))]
        {
            // No node to update.
            let _ = (node, args);
            Err(DeviceError::NoDriver)
        }
    }

    /// How many buffers a replay re-stamps. Exposed so a test can assert
    /// the capture actually took custody of them: a graph that collected
    /// none would replay, write, and leave every reader unsynchronised.
    pub fn tracked_buffer_count(&self) -> usize {
        #[cfg(feature = "cuda")]
        {
            self.slots.len()
        }
        #[cfg(not(feature = "cuda"))]
        {
            // Nothing was captured, so nothing is tracked.
            0
        }
    }
}

/// One node of a capture, as reported by [`Stream::capturing_node`].
#[cfg(feature = "cuda")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphNode(pub(crate) cudarc::driver::sys::CUgraphNode);

/// Stub-mode counterpart: there is no driver and therefore no node.
#[cfg(not(feature = "cuda"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphNode(pub(crate) ());

// SAFETY: an opaque driver handle, only dereferenced by the driver itself
// under a bound context.
unsafe impl Send for GraphNode {}
// SAFETY: as above; the handle is immutable.
unsafe impl Sync for GraphNode {}

impl Stream {
    /// Start recording launches on this stream instead of issuing them.
    ///
    /// Between this and [`Stream::end_capture`] the stream accepts work and
    /// runs none of it. Anything that would ask the device a question —
    /// synchronising the stream, querying an event recorded on it — is
    /// illegal during capture and invalidates it, which is why the VM takes
    /// a lean dispatch path while a capture is open rather than its usual
    /// event-and-callback one.
    pub fn begin_capture(&self, mode: CaptureMode) -> Result<()> {
        #[cfg(feature = "cuda")]
        {
            self.bind_device()?;
            // SAFETY: the stream belongs to the context bound above and the
            // call only changes that stream's mode.
            let status = unsafe {
                cudarc::driver::sys::lib().cuStreamBeginCapture_v2(self.raw(), mode.raw())
            };
            if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!(
                    "cuStreamBeginCapture_v2: {status:?}"
                )));
            }
            self.set_capturing(true);
            Ok(())
        }
        #[cfg(not(feature = "cuda"))]
        {
            // Capture needs a driver.
            let _ = mode;
            Err(DeviceError::NoDriver)
        }
    }

    /// Stop recording and hand back what was recorded.
    ///
    /// An invalidated capture — something illegal happened on this thread
    /// while it was open — ends with a null graph and is reported as an
    /// error rather than as an empty graph, because an empty graph replays
    /// successfully and does nothing, which is the worst possible way for
    /// this to fail.
    pub fn end_capture(&self, ctx: &DeviceContext) -> Result<Graph> {
        #[cfg(feature = "cuda")]
        {
            self.bind_device()?;
            // Cleared unconditionally, including on every failure path
            // below: a stream that is not capturing must not be left
            // claiming that it is, or every later launch on it would skip
            // the event discipline it needs.
            self.set_capturing(false);
            let mut raw: cudarc::driver::sys::CUgraph = std::ptr::null_mut();
            // SAFETY: valid out-pointer, stream on the bound context.
            let status =
                unsafe { cudarc::driver::sys::lib().cuStreamEndCapture(self.raw(), &mut raw) };
            if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!(
                    "cuStreamEndCapture: {status:?}"
                )));
            }
            if raw.is_null() {
                return Err(DeviceError::Driver(
                    "cuStreamEndCapture returned a null graph — the capture was invalidated \
                     (something on this thread asked the device a question while it was open)"
                        .to_string(),
                ));
            }
            Ok(Graph {
                raw,
                device: ctx.inner().device().clone(),
                slots: self.take_captured_slots(),
            })
        }
        #[cfg(not(feature = "cuda"))]
        {
            // Never an empty graph: one would replay successfully and run nothing.
            let _ = ctx;
            Err(DeviceError::NoDriver)
        }
    }

    /// The node the most recent captured operation added, if this stream is
    /// capturing.
    ///
    /// `Ok(None)` means the stream is not in capture mode. For a linear
    /// single-stream capture the dependency set is exactly one node — the
    /// last one recorded — and anything else means the capture has a shape
    /// this cannot attribute, which is reported rather than guessed at.
    pub fn capturing_node(&self) -> Result<Option<GraphNode>> {
        #[cfg(feature = "cuda")]
        {
            self.bind_device()?;
            let mut status_out =
                cudarc::driver::sys::CUstreamCaptureStatus::CU_STREAM_CAPTURE_STATUS_NONE;
            let mut id: cudarc::driver::sys::cuuint64_t = 0;
            let mut graph: cudarc::driver::sys::CUgraph = std::ptr::null_mut();
            let mut deps: *const cudarc::driver::sys::CUgraphNode = std::ptr::null();
            let mut num_deps: usize = 0;
            // SAFETY: five valid out-pointers; the driver writes them and
            // `deps` borrows storage the driver owns for the duration of the
            // capture, which is why the nodes are copied out immediately below.
            let rc = unsafe {
                cudarc::driver::sys::lib().cuStreamGetCaptureInfo_v2(
                    self.raw(),
                    &mut status_out,
                    &mut id,
                    &mut graph,
                    &mut deps,
                    &mut num_deps,
                )
            };
            if rc != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(DeviceError::Driver(format!(
                    "cuStreamGetCaptureInfo_v2: {rc:?}"
                )));
            }
            if status_out
                != cudarc::driver::sys::CUstreamCaptureStatus::CU_STREAM_CAPTURE_STATUS_ACTIVE
            {
                return Ok(None);
            }
            match num_deps {
                0 => Ok(None),
                1 => {
                    // SAFETY: the driver reported exactly one dependency and
                    // `deps` points at storage valid for this call.
                    Ok(Some(GraphNode(unsafe { *deps })))
                }
                n => Err(DeviceError::Driver(format!(
                    "cuStreamGetCaptureInfo_v2 reported {n} current dependencies; this attributes \
                     a captured launch to a node only for a linear single-stream capture, where \
                     there is exactly one"
                ))),
            }
        }
        #[cfg(not(feature = "cuda"))]
        {
            // A stream that cannot capture is never capturing. This one ANSWERS
            // rather than failing: the question has a true answer without a
            // driver, and callers use it to choose a path, not to do work.
            Ok(None)
        }
    }

    /// `bind_to_thread` on this stream's device — the prelude every raw
    /// handle use in this crate shares.
    ///
    /// Cuda-only, unlike everything above it: there is no raw handle to
    /// bind without a driver, and no driverless arm calls it.
    #[cfg(feature = "cuda")]
    fn bind_device(&self) -> Result<()> {
        self.device_arc()
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))
    }
}
