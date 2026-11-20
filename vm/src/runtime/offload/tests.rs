use super::*;
use crate::config::VmConfig;

/// Identify which array parameter (if any) is the output sink.
///
/// First-cut convention: the **last** array in `param_kinds`. Documented
/// in the module docstring. Returns the index into `param_kinds` (not
/// the kernel-arg slot index). Moved into this test module in JIT round 22
/// wave 7 (lane review): the tests below were its only callers.
fn output_array_index(param_kinds: &[ParamKind]) -> Option<usize> {
    param_kinds
        .iter()
        .enumerate()
        .rev()
        .find(|(_, k)| k.is_array())
        .map(|(i, _)| i)
}
use cratonvm_reader::class_reader::read_class;
use std::path::PathBuf;

fn fixture_path(class_name: &str) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let root = std::path::Path::new(manifest_dir)
        .parent()
        .expect("workspace root is vm/..");
    let base = if root.join("test_classes").join("gpu").is_dir() {
        root.join("test_classes").join("gpu")
    } else {
        root.join("tools").join("test_classes").join("gpu")
    };
    base.join(format!("{class_name}.class"))
}

fn minimal_class_bytes(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    code: &[u8],
) -> Vec<u8> {
    fn push_utf8(buf: &mut Vec<u8>, s: &str) {
        buf.push(1);
        buf.extend_from_slice(&(s.len() as u16).to_be_bytes());
        buf.extend_from_slice(s.as_bytes());
    }
    let mut buf = Vec::with_capacity(128 + code.len());
    buf.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // minor
    buf.extend_from_slice(&52u16.to_be_bytes()); // major (Java 8)
    buf.extend_from_slice(&8u16.to_be_bytes()); // cp_count (7 entries)
    push_utf8(&mut buf, "Code"); // #1
    push_utf8(&mut buf, class_name); // #2
    buf.push(7);
    buf.extend_from_slice(&2u16.to_be_bytes()); // #3 this Class
    push_utf8(&mut buf, "java/lang/Object"); // #4
    buf.push(7);
    buf.extend_from_slice(&4u16.to_be_bytes()); // #5 super Class
    push_utf8(&mut buf, method_name); // #6
    push_utf8(&mut buf, descriptor); // #7
    buf.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    buf.extend_from_slice(&3u16.to_be_bytes()); // this_class
    buf.extend_from_slice(&5u16.to_be_bytes()); // super_class
    buf.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
    buf.extend_from_slice(&0u16.to_be_bytes()); // fields_count
    buf.extend_from_slice(&1u16.to_be_bytes()); // methods_count
    buf.extend_from_slice(&0x0009u16.to_be_bytes()); // ACC_PUBLIC | ACC_STATIC
    buf.extend_from_slice(&6u16.to_be_bytes()); // name_index
    buf.extend_from_slice(&7u16.to_be_bytes()); // descriptor_index
    buf.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
    buf.extend_from_slice(&1u16.to_be_bytes()); // "Code"
    let attr_len = (12 + code.len()) as u32;
    buf.extend_from_slice(&attr_len.to_be_bytes());
    buf.extend_from_slice(&4u16.to_be_bytes()); // max_stack
    buf.extend_from_slice(&4u16.to_be_bytes()); // max_locals
    buf.extend_from_slice(&(code.len() as u32).to_be_bytes());
    buf.extend_from_slice(code);
    buf.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    buf.extend_from_slice(&0u16.to_be_bytes()); // code attributes_count
    buf.extend_from_slice(&0u16.to_be_bytes()); // class attributes_count
    buf
}

fn synthetic_fixture_bytes(class_name: &str) -> Option<Vec<u8>> {
    match class_name {
        "EligibleVectorAdd" => {
            // public static void vectorAdd(int[] a, int[] b, int[] c):
            //   for (int i = 0; i < a.length; i++) c[i] = a[i] + b[i];
            const CODE: &[u8] = &[
                0x03, // 0: iconst_0
                0x3e, // 1: istore_3 (i)
                0x1d, // 2: iload_3
                0x2a, // 3: aload_0 (a)
                0xbe, // 4: arraylength
                0xa2, 0x00, 19, // 5: if_icmpge -> 24
                0x2c, // 8: aload_2 (c)
                0x1d, // 9: iload_3
                0x2a, // 10: aload_0 (a)
                0x1d, // 11: iload_3
                0x2e, // 12: iaload
                0x2b, // 13: aload_1 (b)
                0x1d, // 14: iload_3
                0x2e, // 15: iaload
                0x60, // 16: iadd
                0x4f, // 17: iastore
                0x84, 0x03, 0x01, // 18: iinc 3, 1
                0xa7, 0xff, 0xed, // 21: goto -19 -> 2
                0xb1, // 24: return
            ];
            Some(minimal_class_bytes(
                "EligibleVectorAdd",
                "vectorAdd",
                "([I[I[I)V",
                CODE,
            ))
        }
        "RejectAllocation" => {
            // public static int[] build(int n) { return new int[n]; }
            const CODE: &[u8] = &[
                0x1a, // iload_0
                0xbc, 0x0a, // newarray T_INT
                0xb0, // areturn
            ];
            Some(minimal_class_bytes(
                "RejectAllocation",
                "build",
                "(I)[I",
                CODE,
            ))
        }
        _ => None,
    }
}

/// Load a real `.class` file and return its parsed methods, its
/// `this_class` name, and its constant pool. We avoid constructing
/// the heavy `cratonvm_classloading::Class` — the cache API only
/// needs `(class_id, class_name, method_index, &ClassFileMethod,
/// &ConstantPool)`.
fn load_methods(class_name: &str) -> (Vec<ClassFileMethod>, String, ConstantPool) {
    let path = fixture_path(class_name);
    let bytes = std::fs::read(&path)
        .ok()
        .or_else(|| synthetic_fixture_bytes(class_name))
        .unwrap_or_else(|| panic!("failed to read fixture {}", path.display()));
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
    let root = std::path::Path::new(manifest_dir)
        .parent()
        .expect("workspace root is vm/..");
    let base = if root.join("test_classes").join("gpu").is_dir() {
        root.join("test_classes").join("gpu")
    } else {
        root.join("tools").join("test_classes").join("gpu")
    };
    let path = base.join("annotations").join("ExcludedKernel.class");
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
    let root = std::path::Path::new(manifest_dir)
        .parent()
        .expect("workspace root is vm/..");
    let base = if root.join("test_classes").join("gpu").is_dir() {
        root.join("test_classes").join("gpu")
    } else {
        root.join("tools").join("test_classes").join("gpu")
    };
    let path = base.join("annotations").join("ExcludedAndKernel.class");
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
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Failed {
            message: "test: dispatch-time failure".to_string(),
            kind: GpuErrorKind::Unknown,
        }),
        finalize: parking_lot::Mutex::new(None),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
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
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Running),
        finalize: parking_lot::Mutex::new(None),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
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
// (see `gpu-offload-followups-20260711.md`).

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
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Running),
        finalize: parking_lot::Mutex::new(Some(FinalizeState::new(
            Vec::new(),
            GcCriticalGuard::acquire(),
        ))),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
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
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Running),
        finalize: parking_lot::Mutex::new(Some(FinalizeState::new(
            Vec::new(),
            GcCriticalGuard::acquire(),
        ))),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(sub.clone());

    finalize_enqueued_handle(&weak_vm, &sub);

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
        stream_handle: None,
        event: None,
        status: parking_lot::Mutex::new(SubmissionStatus::Running),
        finalize: parking_lot::Mutex::new(Some(FinalizeState::new(
            Vec::new(),
            GcCriticalGuard::acquire(),
        ))),
        ephemeral_output_handle: std::sync::atomic::AtomicU64::new(0),
        device_done: std::sync::atomic::AtomicBool::new(false),
        active_stream_armed: std::sync::atomic::AtomicBool::new(false),
    });
    register_submission(sub.clone());

    finalize_enqueued_handle(&weak_vm, &sub);

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
