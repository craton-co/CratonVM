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
//! # What is deliberately not here yet
//!
//! `cuGraphExecKernelNodeSetParams`, which is what a later increment needs
//! to replay a graph whose scalars changed. It wants the same two-backing-
//! store argument marshalling `launch_raw_on_stream_inner` documents at
//! length — a stable `u64` slot per device pointer, and scalar pointers
//! into the `KernelArg` vec — and getting that wrong is a silent wrong
//! answer rather than a failure. It belongs with a refactor that shares
//! that packing rather than a second copy of it. Capture and replay do not
//! need it, and capture and replay are what has to be proven first.

use crate::{DeviceContext, DeviceError, Result, Stream};

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

#[cfg(feature = "cuda")]
impl Graph {
    /// Resolve the graph into something launchable.
    ///
    /// This is where the driver does the work a per-launch dispatch would
    /// otherwise repeat: validating the topology, resolving the kernels and
    /// laying out the argument buffers. It is expensive and it happens
    /// once.
    pub fn instantiate(&self) -> Result<GraphExec> {
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
        })
    }

    /// How many nodes the capture recorded.
    ///
    /// The count a caller checks against its own launch count. A capture
    /// that recorded fewer nodes than the caller issued launches means
    /// something else on this thread was folded into the graph, and that is
    /// worth failing on rather than replaying.
    pub fn node_count(&self) -> Result<usize> {
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
}

/// An instantiated graph: one `cuGraphLaunch` runs every launch it holds.
pub struct GraphExec {
    #[cfg(feature = "cuda")]
    raw: cudarc::driver::sys::CUgraphExec,
    #[cfg(feature = "cuda")]
    device: std::sync::Arc<cudarc::driver::safe::CudaDevice>,
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

#[cfg(feature = "cuda")]
impl GraphExec {
    /// Submit every launch in the graph onto `stream`.
    ///
    /// Asynchronous, exactly like a single launch: the call returns once the
    /// work is queued. Ordinary stream ordering applies, so the usual
    /// `Stream::synchronize` or an event says when it finished.
    pub fn launch(&self, stream: &Stream) -> Result<()> {
        self.device
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))?;
        // SAFETY: both handles live on the bound context and the call only
        // enqueues.
        let status = unsafe { cudarc::driver::sys::lib().cuGraphLaunch(self.raw, stream.raw()) };
        if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            return Err(DeviceError::Driver(format!("cuGraphLaunch: {status:?}")));
        }
        Ok(())
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

#[cfg(feature = "cuda")]
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
        self.bind_device()?;
        // SAFETY: the stream belongs to the context bound above and the
        // call only changes that stream's mode.
        let status =
            unsafe { cudarc::driver::sys::lib().cuStreamBeginCapture_v2(self.raw(), mode.raw()) };
        if status != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            return Err(DeviceError::Driver(format!(
                "cuStreamBeginCapture_v2: {status:?}"
            )));
        }
        self.set_capturing(true);
        Ok(())
    }

    /// Stop recording and hand back what was recorded.
    ///
    /// An invalidated capture — something illegal happened on this thread
    /// while it was open — ends with a null graph and is reported as an
    /// error rather than as an empty graph, because an empty graph replays
    /// successfully and does nothing, which is the worst possible way for
    /// this to fail.
    pub fn end_capture(&self, ctx: &DeviceContext) -> Result<Graph> {
        self.bind_device()?;
        // Cleared unconditionally, including on every failure path
        // below: a stream that is not capturing must not be left
        // claiming that it is, or every later launch on it would skip
        // the event discipline it needs.
        self.set_capturing(false);
        let mut raw: cudarc::driver::sys::CUgraph = std::ptr::null_mut();
        // SAFETY: valid out-pointer, stream on the bound context.
        let status = unsafe { cudarc::driver::sys::lib().cuStreamEndCapture(self.raw(), &mut raw) };
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
        })
    }

    /// The node the most recent captured operation added, if this stream is
    /// capturing.
    ///
    /// `Ok(None)` means the stream is not in capture mode. For a linear
    /// single-stream capture the dependency set is exactly one node — the
    /// last one recorded — and anything else means the capture has a shape
    /// this cannot attribute, which is reported rather than guessed at.
    pub fn capturing_node(&self) -> Result<Option<GraphNode>> {
        self.bind_device()?;
        let mut status_out = cudarc::driver::sys::CUstreamCaptureStatus::CU_STREAM_CAPTURE_STATUS_NONE;
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
        if status_out != cudarc::driver::sys::CUstreamCaptureStatus::CU_STREAM_CAPTURE_STATUS_ACTIVE
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

    /// `bind_to_thread` on this stream's device — the prelude every raw
    /// handle use in this crate shares.
    fn bind_device(&self) -> Result<()> {
        self.device_arc()
            .bind_to_thread()
            .map_err(|e| DeviceError::Driver(format!("bind_to_thread: {e:?}")))
    }
}

// ---------------------------------------------------------------------------
// Stub backend
// ---------------------------------------------------------------------------
//
// The crate builds without a driver so that everything above it -- the VM's
// offload path, its natives, its tests -- compiles and runs on a machine
// with no GPU. The types above exist there with the same shape and no
// fields; every entry point reports that there is no driver. There is
// deliberately no "succeeds and does nothing" variant: a capture that
// silently produced an empty graph would replay successfully and run no
// kernels, which is the one failure this whole mechanism must never have.

#[cfg(not(feature = "cuda"))]
impl Graph {
    /// Always [`DeviceError::NoDriver`]: nothing was captured.
    pub fn instantiate(&self) -> Result<GraphExec> {
        Err(DeviceError::NoDriver)
    }

    /// Always [`DeviceError::NoDriver`]: there is no graph to count.
    pub fn node_count(&self) -> Result<usize> {
        Err(DeviceError::NoDriver)
    }
}

#[cfg(not(feature = "cuda"))]
impl GraphExec {
    /// Always [`DeviceError::NoDriver`]: there is nothing to launch.
    pub fn launch(&self, _stream: &Stream) -> Result<()> {
        Err(DeviceError::NoDriver)
    }
}

#[cfg(not(feature = "cuda"))]
impl Stream {
    /// Always [`DeviceError::NoDriver`]: capture needs a driver.
    pub fn begin_capture(&self, _mode: CaptureMode) -> Result<()> {
        Err(DeviceError::NoDriver)
    }

    /// Always [`DeviceError::NoDriver`], never an empty graph.
    pub fn end_capture(&self, _ctx: &DeviceContext) -> Result<Graph> {
        Err(DeviceError::NoDriver)
    }

    /// Always `Ok(None)`: a stream that cannot capture is never capturing.
    ///
    /// This one answers rather than failing because the question it answers
    /// -- "am I in the middle of a capture?" -- has a true answer without a
    /// driver, and callers use it to choose a path rather than to do work.
    pub fn capturing_node(&self) -> Result<Option<GraphNode>> {
        Ok(None)
    }
}
