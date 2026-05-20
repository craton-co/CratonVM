//! Phase 3 Item P3-8 — stub-mode integration test for the async GPU
//! offload path.
//!
//! The dev box has no CUDA driver. This test file exists to verify
//! that the Java <-> Rust wiring (Java native call → Rust handler →
//! Java return) compiles and that, where the harness allows, the
//! stub-mode synthetic answers come back through the JNI boundary
//! correctly. Items P3-4 (native shims), P3-5 (offload cache), and
//! P3-6 (dispatch_async) are designed so that on no-device boxes the
//! handlers return well-defined synthetic results: a non-null
//! `GpuExecutor`, a `Failed` `GpuFuture` with "no CUDA device" in the
//! message, and a working host-side `ResidencyTracker` round trip.
//!
//! # File-level gating
//!
//! Everything in here is gated behind the `gpu-offload` feature so
//! the default VM build does not pull in any Phase 3 symbols. Without
//! the feature, this file compiles to an empty translation unit.
//!
//! # Why tests are mostly `#[ignore]`d
//!
//! Three of the four tests need a `SharedVm` with the
//! `craton.gpu.*` classes loaded from the compiled annotations jar
//! (`craton_gpu::ANNOTATIONS_DIR` / `ANNOTATIONS_JAR`) on the
//! classpath. The harness for "spin up a SharedVm with arbitrary
//! classpath entries and invoke a static Java method by name" is
//! itself a Phase 3 deliverable that other Items (notably P3-4 and
//! P3-5) build out. They are scaffolded here so a future agent only
//! has to remove the `#[ignore]` once that harness exists; the test
//! bodies are commented with `PHASE3-GUESS` markers wherever the
//! exact wiring is not yet pinned down.
//!
//! # What does run today
//!
//! `residency_tracker_compiles` is a true unit smoke test against
//! `rustjvm_vm::runtime::gpu_residency::ResidencyTracker` (Item
//! P3-7). It does not need the SharedVm harness and runs on the
//! no-GPU dev box.

#![cfg(feature = "gpu-offload")]

use rustjvm_vm::runtime::gpu_residency::{PrimitiveType, ResidencyTracker};

/// True unit smoke. The `ResidencyTracker` is a pure host-side
/// structure (Item P3-7); it tracks Java primitive-array handles
/// and their byte buffers so the dispatch path can do `arrayWrapInt`,
/// `arrayToHost`, `arrayIsResident`, and `releaseArray` without ever
/// touching CUDA. On a no-GPU box `is_resident` must be `false` for
/// every handle — wrapping is purely a host bookkeeping operation
/// until a real device upload happens.
#[test]
fn residency_tracker_compiles() {
    let tracker = ResidencyTracker::new();

    // [1, 2, 3] as i32 little-endian.
    let bytes: Vec<u8> = vec![
        1, 0, 0, 0, //
        2, 0, 0, 0, //
        3, 0, 0, 0, //
    ];

    let handle = tracker.wrap(PrimitiveType::I32, bytes.clone());
    assert!(handle > 0, "wrap must return a non-zero handle");

    assert_eq!(
        tracker.to_host(handle).as_deref(),
        Some(bytes.as_slice()),
        "to_host must return the exact bytes we wrapped"
    );

    assert_eq!(
        tracker.element_type(handle),
        Some(PrimitiveType::I32),
        "element_type must remember the primitive kind"
    );

    assert!(
        !tracker.is_resident(handle),
        "no CUDA on this box — is_resident must be false"
    );

    tracker.release(handle);
    assert!(
        tracker.to_host(handle).is_none(),
        "after release, the handle must no longer resolve"
    );
}

/// PHASE3-GUESS: needs the SharedVm-with-classpath harness so we can
/// load `craton.gpu.Native` from the compiled annotations jar and
/// invoke `craton.gpu.Native.openExecutor(0)` through the
/// interpreter. The harness is an explicit Phase 3 deliverable (see
/// `craton_gpu::ANNOTATIONS_JAR` / `ANNOTATIONS_DIR`); once it lands,
/// drop the `#[ignore]` and fill in the call sequence.
#[test]
#[ignore = "needs SharedVm-with-classpath harness; structure-only Phase 3 scaffold"]
fn open_executor_returns_handle() {
    // PHASE3-GUESS: shape of the test once the harness exists.
    //
    //   use rustjvm_vm::config::VmConfig;
    //   use rustjvm_vm::vm::SharedVm;
    //   use std::sync::Arc;
    //
    //   let mut config = VmConfig::default();
    //   config.gpu_offload_enabled = true;
    //   // Append the compiled annotations jar (craton.gpu.*) to the
    //   // VM classpath. Constant name is per the spec; if Item P3-1
    //   // settles on a different export, adjust the use line.
    //   config
    //       .extra_classpath
    //       .push(craton_gpu::ANNOTATIONS_JAR.into());
    //
    //   let shared = Arc::new(SharedVm::new(config));
    //
    //   // Invoke `craton.gpu.Native.openExecutor(0)` and assert the
    //   // returned reference is non-null and has class
    //   // "craton.gpu.GpuExecutor" (or a subtype).
    //   let result = invoke_static_int_to_ref(
    //       &shared,
    //       "craton/gpu/Native",
    //       "openExecutor",
    //       "(I)Lcraton/gpu/GpuExecutor;",
    //       0,
    //   );
    //   assert!(result.is_some(), "openExecutor(0) must return non-null");
    panic!("scaffold — see PHASE3-GUESS comment above");
}

/// PHASE3-GUESS: needs the SharedVm-with-classpath harness AND a way
/// to construct a `craton.gpu.Callable` from Rust (or to compile a
/// trivial Java fixture that wraps the call). On the no-device box
/// the dispatch_async path returns a `Failed` submission with the
/// canonical "no CUDA device" message; this test pins that contract.
#[test]
#[ignore = "needs SharedVm-with-classpath harness"]
fn submit_returns_failed_future_no_device() {
    // PHASE3-GUESS:
    //
    //   1. Open an executor: `let exec = Native.openExecutor(0);`.
    //   2. Construct a no-op `Callable` reference.
    //   3. Submit: `let fut = Native.submit(exec, callable);`.
    //   4. Read future status:
    //        assert_eq!(Native.futureStatus(fut), 2 /* Failed */);
    //   5. Read error message:
    //        let msg = Native.futureGetErrorMessage(fut);
    //        assert!(msg.contains("no CUDA device"));
    //
    // The numeric `2 = Failed` constant matches the
    // `SubmissionStatus` enum in `rustjvm_vm::runtime::offload`
    // (Item P3-6).
    panic!("scaffold — see PHASE3-GUESS comment above");
}

/// PHASE3-GUESS: needs the SharedVm-with-classpath harness. The
/// `arrayWrapInt`/`arrayToHost` pair is the pure-host round trip;
/// it is the smoke equivalent of `residency_tracker_compiles` but
/// run through the Java native bridge to verify the JNI marshalling
/// is symmetric.
#[test]
#[ignore = "needs SharedVm-with-classpath harness"]
fn array_wrap_round_trip() {
    // PHASE3-GUESS:
    //
    //   let handle = Native.arrayWrapInt(&[1, 2, 3]);
    //   assert!(handle != 0);
    //   let host = Native.arrayToHost(handle);
    //   assert_eq!(host, [1, 2, 3]);
    //   assert!(!Native.arrayIsResident(handle));
    //   Native.releaseArray(handle);
    panic!("scaffold — see PHASE3-GUESS comment above");
}

/// PHASE3-GUESS: `register_submission` + `lookup_submission` are
/// public (Item P3-6) but `KernelArgs` construction needs a
/// resolved `CompiledKernel` which in turn needs analyzer output for
/// a real method, which is a non-trivial fixture to assemble in a
/// unit test. Marked `#[ignore]` for now; once Item P3-6 lands its
/// own helper (e.g. `StreamSubmission::failed_stub`) for building
/// no-device synthetic submissions, drop the `#[ignore]` and use
/// that helper here.
#[test]
#[ignore = "needs StreamSubmission stub helper from Item P3-6"]
fn submission_handle_unique() {
    // PHASE3-GUESS:
    //
    //   use rustjvm_vm::runtime::offload::{
    //       lookup_submission, register_submission, SerializedResult,
    //       StreamSubmission, SubmissionStatus,
    //   };
    //
    //   // Construct two distinct "Failed: no CUDA device" stubs and
    //   // verify the handles they get back from register_submission
    //   // are unique and each round-trips through lookup_submission.
    //   let s1 = StreamSubmission::failed_stub("no CUDA device".into());
    //   let s2 = StreamSubmission::failed_stub("no CUDA device".into());
    //   let h1 = register_submission(s1);
    //   let h2 = register_submission(s2);
    //   assert_ne!(h1, h2, "handles must be unique");
    //   assert!(matches!(
    //       lookup_submission(h1).map(|s| s.status()),
    //       Some(SubmissionStatus::Failed),
    //   ));
    //   let _ = SerializedResult::Empty; // touch the type so the import is live
    panic!("scaffold — see PHASE3-GUESS comment above");
}
