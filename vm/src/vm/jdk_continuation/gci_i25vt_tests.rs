use super::*;
use crate::threading::jvm_thread::JdkContinuationState;

fn read(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

/// The end of a free function / of a method.
const FN_END: &str = "\n\u{7d}\n";
const METHOD_END: &str = "\n    \u{7d}\n";

/// From `sig` to the end of the item (`end`: [`FN_END`] or
/// [`METHOD_END`]).
fn item(src: &str, sig: &str, end: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find(end)
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

fn at(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

/// Exactly the two JDK 25 states whose `afterYield()` allocates with no
/// OOME handling open a window; BLOCKED is what an orphan is moved to.
#[test]
fn gci_i25vt_only_the_timed_states_open_a_window() {
    assert_eq!(VTHREAD_TIMED_PARKING, 6);
    assert_eq!(VTHREAD_TIMED_WAITING, 17);
    assert_eq!(VTHREAD_BLOCKED, 13);
    assert!(after_yield_allocates(&Value::Int(6)));
    assert!(after_yield_allocates(&Value::Int(17)));
    // PARKING, WAITING, BLOCKING, YIELDING, the settled states, and a
    // SUSPENDED bit.
    for other in [0, 1, 2, 3, 4, 5, 7, 9, 10, 11, 12, 13, 14, 15, 16, 18, 99, 6 | 256, 17 | 256] {
        assert!(
            !after_yield_allocates(&Value::Int(other)),
            "state {other} must not open a window"
        );
    }
    assert!(!after_yield_allocates(&Value::Object(None)));
}

/// The window bit shares the carrier word: setting either keeps the
/// other and the JDK bit.
#[test]
fn gci_i25vt_the_window_bit_keeps_the_carrier_and_jdk_bits() {
    let mut state = JdkContinuationState::default();
    assert!(!state.post_yield_window());
    state.set_carrier(42);
    state.set_post_yield_window(true);
    assert!(state.post_yield_window());
    assert_eq!(state.carrier(), 42, "the flag is not part of the carrier id");
    state.set_carrier(0);
    assert!(state.post_yield_window(), "set_carrier keeps the flag");
    state.set_jdk();
    state.set_carrier(7);
    state.set_post_yield_window(false);
    assert!(state.jdk() && !state.post_yield_window());
    assert_eq!(state.carrier(), 7);
}

/// Orphans join the monitors' list once each, after it.
#[test]
fn gci_i25vt_orphans_join_the_unblock_list_once() {
    let mut ids = vec![ThreadId(5), ThreadId(9)];
    merge_unblock_ids(&mut ids, vec![9, 11, 11, 3]);
    assert_eq!(ids, vec![ThreadId(5), ThreadId(9), ThreadId(11), ThreadId(3)]);
    let mut none = Vec::new();
    merge_unblock_ids(&mut none, Vec::new());
    assert!(none.is_empty());
}

/// The carrier side: the window is resolved first thing in
/// `enterSpecial`, opened only after the refusal loop (a yield that
/// stands) and before the carrier stops hosting; the resolution tests
/// "not mounted since" and moves the state under the record lock, then
/// queues the orphan.
#[test]
fn gci_i25vt_enter_special_resolves_then_opens_the_window() {
    let src = read("src/vm/jdk_continuation.rs");
    let enter = item(&src, "pub(crate) fn enter_special(", FN_END);
    let resolve = at(&enter, "resolve_post_yield_window(ctx);", "resolve");
    let arc = at(&enter, "shared.try_get_arc()", "first VM work");
    assert!(resolve < arc, "resolved before anything else");
    let refuse = at(&enter, "refuse_yield_without_headroom(ctx, &outcome, vt_tid, carrier)", "loop");
    let open = at(&enter, "open_post_yield_window(ctx, vt_tid, carrier);", "open");
    let hosted = at(&enter, "runners.hosted.lock().remove(&carrier.0);", "hosted");
    assert!(refuse < open && open < hosted);
    assert!(enter[..open]
        .contains("if matches!(outcome, SliceOutcome::Yielded | SliceOutcome::Failed(_))"));

    let open_fn = item(&src, "fn open_post_yield_window(", FN_END);
    let state = at(&open_fn, "after_yield_allocates(", "state test");
    let count = at(&open_fn, ".deposited_mount_count(vt_tid.0)", "mount count");
    let insert = at(&open_fn, ".insert(carrier.0, (vt_tid.0, mounts));", "window");
    let flag = at(&open_fn, ".set_post_yield_window(true);", "flag");
    assert!(state < count && count < insert && insert < flag);

    let res = item(&src, "pub(crate) fn resolve_post_yield_window(", FN_END);
    let gate = at(&res, "if !ctx.thread.jdk_continuation.post_yield_window()", "gate");
    let take = at(&res, ".remove(&ctx.thread.thread_id.0);", "take the window");
    let locked = at(&res, ".with_deposit_unmounted_since(vt, mounts, ||", "locked test");
    let cas = at(&res, "super::vm_exec::compare_and_swap_field_shared(", "CAS");
    let queued = at(&res, "runners.post_yield_orphans.lock().push(vt);", "queue");
    assert!(gate < take && take < locked && locked < cas && cas < queued);
    assert!(res.contains("Value::Int(VTHREAD_BLOCKED)"));
    assert!(!res.contains("gc_alloc"), "the resolution allocates nothing");
}

/// The other two seams: a carrier's Java-level park resolves before it
/// waits, and the unblocker hands the orphans to the JDK in both
/// `CRATONVM_VTHREAD_MONITOR_UNMOUNT` arms.
#[test]
fn gci_i25vt_park_and_unblocker_seams() {
    let exec = read("src/vm/vm_exec/invoke_static_settling_with_first.rs");
    let park = item(
        &exec,
        "    pub(in crate::vm::vm_exec) fn park_with_state_inner(",
        METHOD_END,
    );
    let hook = at(
        &park,
        "if timed && self.thread.jdk_continuation.post_yield_window()",
        "park hook",
    );
    let call = at(
        &park,
        "super::jdk_continuation::resolve_post_yield_window_at_park(self);",
        "call",
    );
    let span = at(&park, "ParkEventSpan::begin_until(", "first park work");
    assert!(
        hook < call && call < span,
        "resolved before the park does anything"
    );

    let exec = read("src/vm/vm_exec/gen_referent_read_keep_alive.rs");
    let take = item(
        &exec,
        "    fn take_virtual_thread_list_to_unblock(",
        METHOD_END,
    );
    let off = at(
        &take,
        "std::thread::sleep(std::time::Duration::from_millis(50));",
        "off arm",
    );
    let on = at(&take, "monitors.take_vthread_unblock_list(", "on arm");
    let merge = at(
        &take,
        "super::jdk_continuation::with_post_yield_orphans(self.shared, ids);",
        "merge",
    );
    let empty = at(&take, "if ids.is_empty()", "empty test");
    assert!(off < merge && on < merge && merge < empty, "both arms reach the merge");
    assert!(!take[..merge].contains("return None;"), "no arm returns before the merge");
}
