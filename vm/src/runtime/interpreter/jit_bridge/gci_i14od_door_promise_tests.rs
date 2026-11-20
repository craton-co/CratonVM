use super::{door_handed_record, jit_abi_arg_bits, publish_door_promise, WeakExecuteArgs};
use crate::classloading::ClassId;
use crate::runtime::interpreter::field_access::InvokeArgsRootGuard;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::Value;
use crate::vm::SharedVm;

/// The first-call door's weak arguments mark exactly its non-null
/// references, in argument order, so its record names those words and no
/// primitive or null.
#[test]
fn gci_i14od_the_first_call_door_record_names_exactly_its_weak_references() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "gci-i14od-first-call");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let args = [
        Value::Object(Some(a)),
        Value::Int(7),
        Value::Object(None),
        Value::Object(Some(b)),
    ];
    let guard = InvokeArgsRootGuard::new(&mut thread, &args);
    let jit_args: Vec<i64> = args.iter().map(jit_abi_arg_bits).collect();
    let weak = WeakExecuteArgs::take(&shared, &mut thread, &guard, &args).expect("taken");
    let pins = weak.handed_arg_pins();
    assert_eq!(pins.len(), args.len(), "one marker per argument");
    assert!(pins[0] != usize::MAX && pins[3] != usize::MAX, "the references");
    assert!(pins[1] == usize::MAX && pins[2] == usize::MAX, "an int and a null");
    let rec = door_handed_record(&jit_args, pins, 2, &[]);
    assert!(rec.active);
    assert_eq!(rec.chain_depth, 2);
    assert_eq!(rec.len, 2);
    assert!(rec.handed(a.as_ptr() as usize) && rec.handed(b.as_ptr() as usize));
    assert!(!rec.handed(7), "the int is never a handed word");
    drop(weak);
    drop(guard);
}

/// The publication names the chain index the body's entry guard takes
/// next (the current depth) and ends with the call.
#[test]
fn gci_i14od_a_door_publication_names_the_next_chain_entry_and_ends_with_the_call() {
    let mut thread = JvmThread::new(ThreadId(0), "gci-i14od-publish");
    let depth = crate::jit::conservative_roots::current_thread_jit_depth();
    {
        let _published = publish_door_promise(&mut thread, &[0x1000, 5], &[0, usize::MAX]);
        let rec = thread
            .door_promise
            .as_ref()
            .map(|s| s.record)
            .expect("published");
        assert!(rec.active);
        assert_eq!(rec.chain_depth, depth);
        assert!(rec.handed(0x1000) && !rec.handed(5));
    }
    let after = thread
        .door_promise
        .as_ref()
        .map(|s| s.record)
        .expect("box kept");
    assert!(!after.active, "no door in flight");
}

/// The three weak-args doors take the promise under the switch (the
/// cached-call door since i13/oj; the decoded door and `execute`'s
/// first-call door since i14/od), and no shim zeroes a register any more:
/// the callers' callee-saved values stay where the scan reads and the remap
/// rewrites them.
#[test]
fn gci_i14od_every_weak_args_door_takes_the_promise_and_no_register_is_zeroed() {
    let bridge = concat!(include_str!("../jit_bridge/mod.rs"), "\n", include_str!("../jit_bridge/jit_verdicts.rs"), "\n", include_str!("../jit_bridge/osr_continuation.rs"), "\n", include_str!("../jit_bridge/compile_osr_body.rs"), "\n", include_str!("../jit_bridge/osr_exception_exit.rs"), "\n", include_str!("../jit_bridge/try_osr.rs"), "\n", include_str!("../jit_bridge/native_shadow_arms.rs"), "\n", include_str!("../jit_bridge/compile_optimizing_artifact.rs"), "\n", include_str!("../jit_bridge/never_run_static_exit_pcs.rs"), "\n", include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"), "\n", include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"), "\n", include_str!("../jit_bridge/folded_handle_member_row.rs"), "\n", include_str!("../jit_bridge/callee_never_taken_rows.rs"), "\n", include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")).replace("\r\n", "\n");
    let body_of = |src: &str, head: &str| -> String {
        let start = src
            .find(head)
            .unwrap_or_else(|| panic!("`{head}` is defined"));
        let rest = &src[start..];
        // `\x7d` is the closing brace: the function ends at the first one
        // in column 0.
        let end = rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3);
        rest[..end].to_string()
    };
    for door in [
        "\npub(in crate::runtime::interpreter) fn execute_jit_call(",
        "\npub(in crate::runtime::interpreter) fn execute_jit_call_decoded(",
    ] {
        let body = body_of(bridge.as_str(), door);
        assert!(
            body.contains("weak_args.is_some() && door_promise_enabled(thread)"),
            "`{door}` asks the switch only with its arguments weak"
        );
        assert!(
            body.contains("run_jit_body_promised("),
            "`{door}` runs the body through the promising shim"
        );
    }
    let shim = body_of(
        bridge.as_str(),
        "\npub(in crate::runtime::interpreter::jit_bridge) fn run_jit_body_promised(",
    );
    assert!(shim.contains("publish_door_promise_with_weak_pins("));
    assert!(shim.contains("args_jit,\n        arg_pins,"));
    assert!(shim.contains("weak_pin_slots,"));
    assert!(bridge.contains("debugger_observes_locals(shared)"));
    assert!(!shim.contains("asm!"), "the shim zeroes no register");
    let raw = body_of(
        bridge.as_str(),
        "\npub(in crate::runtime::interpreter) fn run_jit_body_raw_promised(",
    );
    assert!(raw.contains("publish_door_promise_with_weak_pins("));
    assert!(raw.contains("weak.handed_arg_pins(),"));
    assert!(raw.contains("weak.weak_dispatch_pin_slots(),"));
    assert!(raw.contains("run_jit_body_raw(shared, thread, compiled, name, needs_heap, args_jit)"));
    let zeroing = ["fn zero_door_", "shim_owned_registers"].concat();
    assert!(!bridge.contains(&zeroing), "the zeroing helper is gone");
    let interp = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let execute = body_of(interp.as_str(), "\npub fn execute(");
    assert!(
        execute.contains("Some(weak) if jit_bridge::door_promise_enabled(thread) =>"),
        "the first-call door asks the switch only with its arguments weak"
    );
    assert!(execute.contains("jit_bridge::run_jit_body_raw_promised("));
    assert!(
        execute.contains("jit_bridge::run_jit_body_raw("),
        "the plain door is kept for the default"
    );
}
