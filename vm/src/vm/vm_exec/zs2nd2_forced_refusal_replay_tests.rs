//! zs s2/nd2 (`zgc-zs1nd-proposal-forced-refusal-replay-test-20261014.md`):
//! the behavioural proof of the restartable list. For a listed body, a
//! clean dispatch through the funnel records its answer, a digest of its
//! arguments afterwards and the number of one-attempt doors it entered;
//! then for every k the k-th door is forced to refuse
//! (`native_oom::forced_door_refusal`), the funnel's replay re-runs the
//! body, and the answer and the argument digest must equal the clean
//! run's. The sweep makes the refusal land after EVERY prefix of the body,
//! which is the admission rule's claim. The source ratchet
//! (`native-builtins/tests/oom_restartable_ratchet.rs`) cannot see effects
//! inside helpers or tell a fresh object from an argument; this can.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::runtime::native_oom::{forced_door_arm, forced_door_count};
use cratonvm_native_builtins::oom_restartable::restartable_bodies;
use std::sync::atomic::Ordering;

// Indices into `restartable_bodies()` (frozen, in order, by the ratchet).
const GET_DECLARED_FIELDS: usize = 0;
const GET_DECLARED_METHODS: usize = 1;
const GET_DECLARED_CONSTRUCTORS: usize = 2;
const GET_INTERFACES: usize = 4;
const GET_DECLARED_CLASSES: usize = 5;
const GET_NAME: usize = 6;
// zt t1/nd: the zs/dev merge put dev's `native_class_get_name_leaf` at 7
// (and the two `valueOf` bodies plus their trampolines at the end), so
// the indices after `GET_NAME` moved by one and the list is 21 long.
const INTEGER_TO_STRING: usize = 11;
const LONG_TO_STRING: usize = 12;
const DOUBLE_TO_STRING: usize = 13;
const INTEGER_TO_HEX_STRING: usize = 14;
const OBJECT_GET_CLASS: usize = 15;

#[cfg(feature = "zgc")]
fn zgc_vm() -> Option<SharedVm> {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    if shared.mem.heap.is_zgc() {
        return Some(shared);
    }
    // zt t3/nd (t2/rv hunk ND-2, zs3rv2 §4): a `zgc` build must give a ZGC
    // heap; only a build without the feature skips, and says so.
    assert!(!cfg!(feature = "zgc"), "GcAlgorithm::Zgc built a non-ZGC heap");
    eprintln!("SKIP: forced-refusal sweep needs the `zgc` feature");
    None
}

/// A value rendered for comparison: strings by text, mirrors by class
/// name, arrays by elements, other objects by class name and fields, to
/// `depth` levels (fresh objects compare by content, never by address).
pub(super) fn render(ctx: &NativeContextImpl<'_>, v: Value, depth: u32) -> String {
    let Value::Object(Some(o)) = v else {
        return format!("{v:?}");
    };
    let cid = ctx.class_id_of_object(o);
    let class = ctx.class_name_of_id(cid).unwrap_or_default();
    if class == "java/lang/String" {
        return format!("S{:?}", ctx.read_string(o));
    }
    if class == "java/lang/Class" {
        let named = ctx
            .class_id_from_mirror(o)
            .and_then(|c| ctx.class_name_of_id(c));
        return format!("C<{named:?}>");
    }
    if depth == 0 {
        return format!("<{class}>");
    }
    if ctx.object_is_array(o) {
        let n = ctx.array_length(o);
        let items: Vec<String> = (0..n)
            .map(|i| render(ctx, ctx.get_array_element(o, i), depth - 1))
            .collect();
        return format!("{class}[{}]", items.join(","));
    }
    let fields: Vec<String> = (0..ctx.object_num_fields(o))
        .map(|i| render(ctx, ctx.get_field(o, i), depth - 1))
        .collect();
    format!("{class}{{{}}}", fields.join(","))
}

/// The outcome of one dispatch: the rendered answer (or error kind) and
/// the rendered arguments after it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Outcome {
    pub(super) answer: String,
    pub(super) args_after: String,
}

/// Dispatch `cb` on `args` through the funnel with the knob armed at `k`
/// (`0`: count only). The object arguments are pinned below the funnel's
/// base for the call, so the replay's collection cannot stale them here.
/// Answers the outcome, the doors counted, and whether the arm fired.
pub(super) fn dispatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cb: NativeCallback,
    args: &[Value],
    k: u32,
) -> (Outcome, u32, bool) {
    let base = thread.native_pin_roots.len();
    let objs: Vec<(usize, usize)> = args
        .iter()
        .enumerate()
        .filter_map(|(i, v)| match v {
            Value::Object(Some(o)) => {
                thread.native_pin_roots.push(*o);
                Some((i, thread.native_pin_roots.len() - 1))
            }
            _ => None,
        })
        .collect();
    forced_door_arm(k);
    let out = safe_native_call(shared, thread, cb, args);
    let doors = forced_door_count();
    let fired = k != 0 && doors >= k;
    forced_door_arm(0);
    let mut now = args.to_vec();
    for &(i, slot) in &objs {
        now[i] = Value::Object(Some(thread.native_pin_roots[slot]));
    }
    thread.native_pin_roots.truncate(base);
    let ctx = NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    let answer = match out {
        Ok(Some(v)) => render(&ctx, v, 2),
        Ok(None) => "void".to_string(),
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { .. },
        ))) => "OOME".to_string(),
        Err(e) => format!("ERR {e:?}"),
    };
    let args_after: Vec<String> = now.iter().map(|v| render(&ctx, *v, 2)).collect();
    (
        Outcome {
            answer,
            args_after: args_after.join(" | "),
        },
        doors,
        fired,
    )
}

/// The sweep. `fixture` builds fresh arguments for each run. `Ok(Some(k))`
/// is the number of forced refusals that landed (each one replayed);
/// `Ok(None)`: the CLEAN run already answered an error (the unit-test VM
/// lacks what the body needs; the caller reports the row as skipped);
/// `Err` names the first k whose answer or argument digest differed.
fn sweep(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cb: NativeCallback,
    fixture: &mut dyn FnMut(&SharedVm, &mut JvmThread) -> Vec<Value>,
) -> Result<Option<u32>, String> {
    // One unmeasured warm-up run first: a body's FIRST call may load a
    // class the later calls find loaded (the first `getDeclaredFields0`
    // of this VM answered an array whose component was not
    // `java/lang/reflect/Field`, because `class_id_by_name` ran before
    // `create_field_object` loaded it; recorded in the s2/nd2 report).
    // The property under test is "a refused run, replayed, equals one
    // run from the same state", so every compared run starts warm.
    let warm = fixture(shared, thread);
    let _ = dispatch(shared, thread, cb, &warm, 0);
    let args = fixture(shared, thread);
    let (clean, doors, _) = dispatch(shared, thread, cb, &args, 0);
    if clean.answer.starts_with("ERR") || clean.answer == "OOME" {
        return Ok(None);
    }
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    let mut landed = 0;
    for k in 1..=doors {
        let args = fixture(shared, thread);
        let before = census.native_replays.load(Ordering::Relaxed);
        let (forced, _, fired) = dispatch(shared, thread, cb, &args, k);
        let replays = census.native_replays.load(Ordering::Relaxed) - before;
        if fired {
            landed += 1;
            assert!(replays >= 1, "k={k}: the refusal must be replayed");
        }
        if forced != clean {
            return Err(format!("k={k}/{doors}: clean {clean:?} forced {forced:?}"));
        }
        assert!(thread.native_pin_roots.is_empty(), "k={k}: no pin left");
    }
    Ok(Some(landed))
}

/// One row: a sweep that must answer cleanly (`required`) or may be
/// skipped (recorded by name).
fn row(
    shared: &SharedVm,
    thread: &mut JvmThread,
    idx: usize,
    what: &str,
    required: bool,
    fixture: &mut dyn FnMut(&SharedVm, &mut JvmThread) -> Vec<Value>,
    ran: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> u32 {
    match sweep(shared, thread, restartable_bodies()[idx], fixture) {
        Ok(Some(n)) => {
            ran.push(format!("{idx}:{what}"));
            n
        }
        Ok(None) if !required => {
            skipped.push(format!("{idx}:{what}"));
            0
        }
        Ok(None) => panic!("body {idx} ({what}): the clean run must answer"),
        Err(e) => panic!("body {idx} ({what}): {e}"),
    }
}

fn primitive(v: Value) -> impl FnMut(&SharedVm, &mut JvmThread) -> Vec<Value> {
    move |_, _| vec![v]
}

/// `int[4]` holding 1..=4 (fresh per run).
fn int_array(shared: &SharedVm, thread: &mut JvmThread) -> ObjectRef {
    let mut ctx = NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    let a = ctx.new_array(ArrayElementType::Int, 4);
    for i in 0..4 {
        ctx.set_array_element(a, i, Value::Int(i as i32 + 1));
    }
    a
}

fn mirror_of_class(shared: &SharedVm, thread: &mut JvmThread, name: &str) -> Option<ObjectRef> {
    let mut ctx = NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    let cid = ctx.class_id_by_name(name)?;
    Some(ctx.get_class_mirror(cid))
}

/// Every listed body this VM can build a fixture for keeps its answer and
/// its arguments under a forced refusal at every door. Rows that need a
/// JDK class the unit-test VM did not load are skipped by name, never
/// silently: the pure rows always run, and at least one refusal must land.
#[test]
#[cfg(feature = "zgc")]
fn every_listed_body_answers_the_same_under_a_forced_refusal_at_every_door() {
    let Some(shared) = zgc_vm() else {
        return;
    };
    let bodies = restartable_bodies();
    assert_eq!(
        bodies.len(),
        21,
        "the index constants follow the ratchet's list"
    );
    let mut thread = JvmThread::new(ThreadId(2201), "zs2nd2-sweep");
    let mut landed = 0u32;
    let mut ran = Vec::new();
    let mut skipped = Vec::new();
    let t = &mut thread;
    for (idx, what, v) in [
        (
            INTEGER_TO_STRING,
            "Integer.toString",
            Value::Int(-1_234_567),
        ),
        (LONG_TO_STRING, "Long.toString", Value::Long(-9_876_543_210)),
        (DOUBLE_TO_STRING, "Double.toString", Value::Double(1.5e10)),
        (
            INTEGER_TO_HEX_STRING,
            "Integer.toHexString",
            Value::Int(0xBEEF),
        ),
    ] {
        landed += row(
            &shared,
            t,
            idx,
            what,
            true,
            &mut primitive(v),
            &mut ran,
            &mut skipped,
        );
    }
    // `Object.getClass` of a fresh `int[]` (the array arm mints the array
    // class's mirror), and `Class.getName` / `getInterfaces0` of that mirror.
    let mut arr_fixture = |s: &SharedVm, t: &mut JvmThread| -> Vec<Value> {
        vec![Value::Object(Some(int_array(s, t)))]
    };
    landed += row(
        &shared,
        t,
        OBJECT_GET_CLASS,
        "getClass(int[])",
        true,
        &mut arr_fixture,
        &mut ran,
        &mut skipped,
    );
    let mut arr_mirror = |s: &SharedVm, t: &mut JvmThread| -> Vec<Value> {
        let a = int_array(s, t);
        let mut ctx = NativeContextImpl {
            shared: s,
            thread: t,
        };
        let cid = ctx.class_id_of_object(a);
        vec![Value::Object(Some(ctx.get_class_mirror(cid)))]
    };
    landed += row(
        &shared,
        t,
        GET_NAME,
        "getName(int[])",
        false,
        &mut arr_mirror,
        &mut ran,
        &mut skipped,
    );
    landed += row(
        &shared,
        t,
        GET_INTERFACES,
        "getInterfaces0(int[])",
        false,
        &mut arr_mirror,
        &mut ran,
        &mut skipped,
    );
    // The reflection listings of a loaded JDK class, when there is one.
    match ["java/lang/Thread", "java/lang/String", "java/lang/Object"]
        .into_iter()
        .find(|n| mirror_of_class(&shared, t, n).is_some())
    {
        Some(name) => {
            let mut cls = |s: &SharedVm, t: &mut JvmThread| -> Vec<Value> {
                vec![Value::Object(mirror_of_class(s, t, name)), Value::Int(0)]
            };
            for (idx, what) in [
                (GET_DECLARED_FIELDS, "getDeclaredFields0"),
                (GET_DECLARED_METHODS, "getDeclaredMethods0"),
                (GET_DECLARED_CONSTRUCTORS, "getDeclaredConstructors0"),
                (GET_DECLARED_CLASSES, "getDeclaredClasses0"),
                (GET_NAME, "getName"),
            ] {
                let what = format!("{what}({name})");
                landed += row(
                    &shared,
                    t,
                    idx,
                    &what,
                    false,
                    &mut cls,
                    &mut ran,
                    &mut skipped,
                );
            }
        }
        None => skipped.push("reflection listings (no JDK class loaded)".to_string()),
    }
    eprintln!(
        "zs2nd2 sweep: ran bodies {ran:?}, {landed} refusals landed, skipped {skipped:?}"
    );
    // Four formatters (one String each) and getClass (its mirror door).
    assert!(landed >= 5, "the knob must land refusals: {landed}");
}

thread_local! {
    static CONTROL_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The positive control's body: writes its ARGUMENT before it allocates
/// (the `StringBuilder.append` shape the admission rule forbids).
fn writes_its_argument_then_allocates(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    CONTROL_CALLS.with(|c| c.set(c.get() + 1));
    let Some(Value::Object(Some(a))) = args.first().copied() else {
        return Ok(None);
    };
    let Value::Int(v) = ctx.get_array_element(a, 0) else {
        return Ok(None);
    };
    ctx.set_array_element(a, 0, Value::Int(v + 10));
    let s = ctx.create_string(&format!("{}", v + 10));
    Ok(Some(Value::Object(Some(s))))
}

/// Positive control: admitted as restartable (test list), the
/// argument-writing body FAILS the sweep at k=1 -- the replay applied the
/// write twice -- so a green sweep above is not vacuous.
#[test]
#[cfg(feature = "zgc")]
fn an_argument_writer_fails_the_sweep() {
    let Some(shared) = zgc_vm() else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(2202), "zs2nd2-control");
    TEST_OOM_RESTARTABLE.with(|c| c.set(writes_its_argument_then_allocates as usize));
    let mut fixture = |s: &SharedVm, t: &mut JvmThread| -> Vec<Value> {
        vec![Value::Object(Some(int_array(s, t)))]
    };
    let verdict = sweep(
        &shared,
        &mut thread,
        writes_its_argument_then_allocates,
        &mut fixture,
    );
    TEST_OOM_RESTARTABLE.with(|c| c.set(0));
    let err = verdict.expect_err("the double write must be caught");
    assert!(err.starts_with("k=1/"), "{err}");
}

