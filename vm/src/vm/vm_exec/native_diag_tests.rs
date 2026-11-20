use super::*;

/// Serialises the tests that toggle `native_ring`'s process-global
/// `ENABLED`.
///
/// libtest runs these on parallel threads and the flag is global, so
/// without this one test's `enable(false)` lands between another's
/// `enable(true)` and its assertion. That is a genuinely flaky failure that
/// looks exactly like the bug the assertion is written to catch, which is
/// the worst kind: it would have been "fixed" by weakening the assertion.
static RING_TOGGLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The memoized half must contain ONLY startup-static gates.
///
/// ARCH-2026-08-04 A3 folded thirteen diagnostic gates on the native funnel
/// into one word, computing eleven of them once. Two —
/// `dispatch_trace::enable()` (called at runtime by the stack-dump watchdog
/// so a hang in native code still leaves a breadcrumb) and
/// `native_ring::enable()` — are toggleable *after* the first native call.
/// Memoizing either would silently disable a diagnostic at exactly the
/// moment someone switched it on to chase a hang, and nothing in the
/// funnel's behaviour would reveal it. Pin the split.
#[test]
fn the_memoized_half_never_holds_a_runtime_toggleable_bit() {
    let stat = native_diag_static();
    assert_eq!(
        stat & native_diag::DISPATCH_TRACE,
        0,
        "DISPATCH_TRACE is toggled at runtime by the stack-dump watchdog; \
         it must be re-read per call, not memoized"
    );
    assert_eq!(
        stat & native_diag::RING,
        0,
        "RING is toggled at runtime via native_ring::enable(); it must be \
         re-read per call, not memoized"
    );
}

/// Toggling the ring at runtime must move the mask, both directions.
///
/// This is the behavioural half of the test above: it proves the dynamic
/// bit is actually re-read rather than merely absent from the memo.
#[test]
fn the_ring_bit_tracks_runtime_toggling() {
    let _serialise = RING_TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let was = cratonvm_native_api::native_ring::is_enabled();

    // Keep the window in which the process-global is flipped as short as
    // physically possible — sample both masks, restore, and only THEN
    // assert. libtest runs the rest of the suite on parallel threads and
    // `RING.lock()` is process-wide; asserting inside the window would
    // hold the ring on across panic/formatting machinery and perturb
    // timing-sensitive neighbours.
    cratonvm_native_api::native_ring::enable(true);
    let on_mask = native_diag_mask();
    cratonvm_native_api::native_ring::enable(false);
    let off_mask = native_diag_mask();
    let track_on = cratonvm_native_api::native_ring::any_recording_enabled();
    cratonvm_native_api::native_ring::enable(was);

    assert_ne!(
        on_mask & native_diag::RING,
        0,
        "enabling the ring must set RING in the mask; if it does not, the \
         funnel will skip record_enter/record_exit and the watchdog's \
         STILL-IN-NATIVE breadcrumb is lost"
    );
    // `any_recording_enabled` also covers CRATONVM_TRACK_NATIVE, a
    // separate env gate — only assert the bit clears when that is off too.
    if !track_on {
        assert_eq!(
            off_mask & native_diag::RING,
            0,
            "disabling the ring must clear RING in the mask"
        );
    }
}

/// Every bit is distinct.
///
/// Thirteen hand-assigned shifts collapsed from thirteen hand-written gate
/// calls. A duplicated shift would silently alias two diagnostics — arming
/// one would arm the other, and disarming one would disarm both — with no
/// symptom until somebody chased a hang with the wrong dump in hand.
#[test]
fn every_diagnostic_bit_is_distinct() {
    let bits = [
        ("BLOCKGC", native_diag::BLOCKGC),
        ("VM_STATE", native_diag::VM_STATE),
        ("STRAYSTACK", native_diag::STRAYSTACK),
        ("EC_WATCH", native_diag::EC_WATCH),
        ("MEMWATCH", native_diag::MEMWATCH),
        ("YOUNGSCAN", native_diag::YOUNGSCAN),
        ("LINKAGE", native_diag::LINKAGE),
        ("ALTRACE", native_diag::ALTRACE),
        ("UNPIN_RING", native_diag::UNPIN_RING),
        ("STALE_OBJREF", native_diag::STALE_OBJREF),
        ("REMAP_TRACE", native_diag::REMAP_TRACE),
        ("DISPATCH_TRACE", native_diag::DISPATCH_TRACE),
        ("RING", native_diag::RING),
    ];
    let mut seen = 0u32;
    for (name, bit) in bits {
        assert_eq!(bit.count_ones(), 1, "{name} must be a single bit");
        assert_eq!(seen & bit, 0, "{name} aliases an earlier diagnostic bit");
        seen |= bit;
    }
    assert_eq!(seen.count_ones(), bits.len() as u32);
}

/// The pre-call helper acts on exactly the bits it is given.
///
/// The other half of the A3 transcription risk: the mask can be composed
/// correctly and still be *consumed* with the wrong bit at a use site. This
/// drives `native_diag_pre_call` with synthetic masks and checks that each
/// bit produces its own effect and no other — which the armed-flag route
/// cannot check, because these diagnostics are all fire-on-anomaly and
/// print nothing on a healthy run.
#[test]
fn pre_call_acts_only_on_the_bits_it_is_given() {
    fn probe(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _a: &[Value],
    ) -> MethodCallResult {
        Ok(None)
    }
    let cb: NativeCallback = probe;
    let mut thread = JvmThread::new(ThreadId(4242), "a3-pre-call-probe");

    // Nothing armed: no ring token, no straystack push, no vm-state name.
    let st = native_diag_pre_call(0, &mut thread, cb);
    assert!(st.ring_idx.is_none(), "mask 0 must not enter the ring");
    assert!(!st.straystack_pushed, "mask 0 must not push straystack");
    assert!(
        st.vm_state_name.is_none(),
        "mask 0 must not name the callee"
    );

    // RING alone: a token, and nothing else.
    //
    // Deliberately does NOT enable the ring. `record_enter` returns its
    // `DISABLED_TOKEN` when recording is off, so `ring_idx` is still
    // `Some(..)` and the assertion still proves the bit reached
    // `record_enter` — which is the entire claim. Enabling it would flip a
    // *process-global* that libtest's other threads observe, and
    // `RING.lock()` is process-wide, so this test would perturb every
    // timing-sensitive test running beside it. `record_exit` on the
    // disabled token is an explicit no-op.
    let st = native_diag_pre_call(native_diag::RING, &mut thread, cb);
    assert!(
        st.ring_idx.is_some(),
        "the RING bit must reach native_ring::record_enter"
    );
    assert!(!st.straystack_pushed);
    assert!(st.vm_state_name.is_none());
    if let Some(idx) = st.ring_idx {
        cratonvm_native_api::native_ring::record_exit(idx);
    }

    // STRAYSTACK alone: a push, and nothing else. Pop it back.
    let st = native_diag_pre_call(native_diag::STRAYSTACK, &mut thread, cb);
    assert!(
        st.straystack_pushed,
        "the STRAYSTACK bit must push the thread-local native stack"
    );
    assert!(st.ring_idx.is_none());
    assert!(st.vm_state_name.is_none());
    CURRENT_NATIVE_STACK.with(|s| {
        assert_eq!(
            s.borrow().len(),
            1,
            "STRAYSTACK must have pushed exactly one entry"
        );
        s.borrow_mut().pop();
    });

    // VM_STATE alone: a resolved name, and nothing else.
    let st = native_diag_pre_call(native_diag::VM_STATE, &mut thread, cb);
    assert!(
        st.vm_state_name.is_some(),
        "the VM_STATE bit must resolve the callee name"
    );
    assert!(st.ring_idx.is_none());
    assert!(!st.straystack_pushed);
}

/// With no diagnostics armed the mask is zero — the fast path is real.
///
/// A default `cargo test` process sets none of the thirteen env vars, so
/// this is the shape every production run takes. If this ever starts
/// failing, some gate acquired a default-on reading and every native call
/// in the VM is now paying for the cold path.
#[test]
fn a_default_process_arms_nothing() {
    let _serialise = RING_TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let was = cratonvm_native_api::native_ring::is_enabled();
    cratonvm_native_api::native_ring::enable(false);
    if cratonvm_native_api::native_ring::any_recording_enabled()
        || crate::dispatch_trace::is_enabled()
    {
        // Another test in this process armed a dynamic gate; the static
        // half is still the meaningful assertion.
        assert_eq!(
            native_diag_static(),
            0,
            "no CRATONVM_DBG_* var is set in a default test process, so \
             every memoized diagnostic bit must be clear"
        );
    } else {
        assert_eq!(
            native_diag_mask(),
            0,
            "no diagnostic is armed in a default test process, so the \
             native funnel's mask must be zero"
        );
    }
    cratonvm_native_api::native_ring::enable(was);
}
