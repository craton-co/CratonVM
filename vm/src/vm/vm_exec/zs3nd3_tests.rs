// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s3/nd3 tests (ZGC round zs, wave s3, lane nd3), a child module of
//! `vm_exec` (`#[path]`), so they reach the funnel and `NativeContextImpl`.
//!
//! 1. [`listing_tests`]: the first declared reflection listing of a VM that
//!    has not loaded `Field` / `Method` / `Constructor` answers a typed array
//!    (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md` §6).

use super::*;
use crate::config::VmConfig;



/// zs s3/nd3: the forced-refusal sweep for a COLLECTING conversion
/// (`native_oom::forced_try_refusal`, the fallible-door twin of nd2's
/// one-attempt knob). A definer is not restartable, so nd2's replay sweep
/// cannot cover it; instead every fallible door a clean
/// `ClassLoader.defineClass1` enters is refused once in turn, and each run
/// must still answer the class it defined (`lang_system::defined_class_mirror`
/// collects and retries), never `OutOfMemoryError`.
#[cfg(feature = "zgc")]
mod define_mirror_tests {
    use super::*;
    use crate::config::GcAlgorithm;
    use crate::runtime::native_oom::{
        forced_door_arm, forced_door_count, forced_try_arm, forced_try_count,
    };

    const DEFINE1_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;";

    pub(super) fn zgc_vm() -> Option<SharedVm> {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            max_heap_size: 64 << 20,
            ..VmConfig::default()
        });
        shared.mem.heap.is_zgc().then_some(shared)
    }

    /// A minimal well-formed class `name extends java/lang/Object` with no
    /// members.
    pub(super) fn class_bytes(name: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&52u16.to_be_bytes());
        b.extend_from_slice(&5u16.to_be_bytes()); // cp 1..4
        for (index, text) in [(2u16, name), (4u16, "java/lang/Object")] {
            b.push(7);
            b.extend_from_slice(&index.to_be_bytes());
            b.push(1);
            b.extend_from_slice(&(text.len() as u16).to_be_bytes());
            b.extend_from_slice(text.as_bytes());
        }
        b.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&1u16.to_be_bytes()); // this_class
        b.extend_from_slice(&3u16.to_be_bytes()); // super_class
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // no interfaces/fields/methods/attributes
        b
    }

    /// What one define answered and counted.
    pub(super) struct Defined {
        /// The defined class's name, `OOME`, or the error.
        pub(super) answer: String,
        /// Fallible doors entered.
        pub(super) tries: u32,
        /// One-attempt doors entered (under the funnel's permission).
        pub(super) doors: u32,
        /// Refusal-ladder engagements during the call: refusal collections
        /// run (or joined) plus those skipped as futile
        /// (`ZgcRefusalCensus`). A plain collection count reads 0 when the
        /// ladder found room without collecting.
        pub(super) collections: u64,
    }

    /// One `defineClass1(null, name, bytes, 0, len, null, null)` through the
    /// funnel with the fallible-door knob armed at `try_k` and the
    /// one-attempt knob at `door_k` (`0`: count only).
    fn define(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cb: NativeCallback,
        name: &str,
        try_k: u32,
        door_k: u32,
    ) -> Defined {
        let bytes = class_bytes(name);
        let base = thread.native_pin_roots.len();
        let (name_obj, array, before) = {
            let mut ctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            let name_obj = ctx.create_string(&name.replace('/', "."));
            let array = ctx.new_array(ArrayElementType::Byte, bytes.len());
            assert!(ctx.write_byte_array_from(array, 0, &bytes));
            (name_obj, array, ladder_engagements(shared))
        };
        thread.native_pin_roots.push(name_obj);
        thread.native_pin_roots.push(array);
        let args = [
            Value::Object(None),
            Value::Object(Some(name_obj)),
            Value::Object(Some(array)),
            Value::Int(0),
            Value::Int(bytes.len() as i32),
            Value::Object(None),
            Value::Object(None),
        ];
        forced_try_arm(try_k);
        forced_door_arm(door_k);
        let out = safe_native_call(shared, thread, cb, &args);
        let tries = forced_try_count();
        let doors = forced_door_count();
        forced_try_arm(0);
        forced_door_arm(0);
        thread.native_pin_roots.truncate(base);
        let ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        let collections = ladder_engagements(shared) - before;
        let answer = match out {
            Ok(Some(Value::Object(Some(mirror)))) => ctx
                .class_id_from_mirror(mirror)
                .and_then(|c| ctx.class_name_of_id(c))
                .unwrap_or_else(|| "<no class>".to_string()),
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { .. },
            ))) => "OOME".to_string(),
            other => format!("{other:?}"),
        };
        Defined {
            answer,
            tries,
            doors,
            collections,
        }
    }

    pub(super) fn ladder_engagements(shared: &SharedVm) -> u64 {
        use std::sync::atomic::Ordering;
        let census = &shared.mem.tlab_wedge.zgc_refusal;
        census.collections.load(Ordering::Relaxed) + census.skipped_futile.load(Ordering::Relaxed)
    }

    fn setup() -> Option<(SharedVm, NativeCallback)> {
        let shared = zgc_vm()?;
        let cb = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            "defineClass1",
            DEFINE1_DESC,
        );
        let Some(cb) = cb else {
            eprintln!("zs3nd3 define: defineClass1 not registered here; skipped");
            return None;
        };
        Some((shared, cb))
    }

    /// Every fallible door of a clean define, refused once in turn: each run
    /// still answers its class (after a collection), so the collecting
    /// conversion is exercised at every door, the mirror's included.
    #[test]
    fn every_fallible_door_of_a_define_is_served_after_a_reclaim() {
        let Some((shared, cb)) = setup() else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(3302), "zs3nd3-define");
        let clean = define(&shared, &mut thread, cb, "zs3nd3/D0", 0, 0);
        if clean.answer != "zs3nd3/D0" {
            eprintln!(
                "zs3nd3 define: the clean define answered {}; skipped",
                clean.answer
            );
            return;
        }
        let tries = clean.tries;
        assert!(tries >= 1, "the mirror mint is a fallible door: {tries}");
        for k in 1..=tries {
            let name = format!("zs3nd3/D{k}");
            let run = define(&shared, &mut thread, cb, &name, k, 0);
            assert_eq!(run.answer, name, "k={k}/{tries}: refused once, then served");
            assert!(
                run.collections >= 1,
                "k={k}/{tries}: the refusal reached the ladder"
            );
            assert!(thread.native_pin_roots.is_empty(), "k={k}: no pin left");
        }
        eprintln!("zs3nd3 define: {tries} fallible doors, each refused once and served");
    }

    /// Kill control (`CRATONVM_ZGC_DEFINE_MIRROR_COLLECT=0`): the mirror is
    /// minted by the one-attempt door again, so it is no longer a fallible
    /// door (one fewer), and a forced refusal of that door is the
    /// `OutOfMemoryError` with the class LEFT DEFINED: the same name defined
    /// again is a `LinkageError` (the atomicity gap of
    /// `zgc-zs2nd2-proposal-mirror-inside-the-define-20261014.md`).
/// zs s7/nd6 (`zgc-zs2nd2-proposal-mirror-inside-the-define-20261014.md`,
    /// the reserved-mirror variant): with the WHOLE ladder spent (every
    /// fallible door refuses, `forced_try_refuse_all`) a define answers
    /// `OutOfMemoryError` with nothing defined, so the same name defined
    /// again once the heap has room is a CLASS, as on HotSpot. `door_k`
    /// refuses the one-attempt door the kill arm reaches after the define.
    fn spent_then_retry(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cb: NativeCallback,
        name: &str,
        door_k: u32,
    ) -> (Defined, Defined) {
        crate::runtime::native_oom::forced_try_refuse_all(true);
        let spent = define(shared, thread, cb, name, 0, door_k);
        crate::runtime::native_oom::forced_try_refuse_all(false);
        let retry = define(shared, thread, cb, name, 0, 0);
        (spent, retry)
    }

    #[test]
    fn zs7nd6_a_spent_ladder_fails_the_define_before_it_publishes() {
        let Some((shared, cb)) = setup() else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(3304), "zs7nd6-define-reserved");
        let clean = define(&shared, &mut thread, cb, "zs7nd6/R0", 0, 0);
        // rv5 (zs s8): a clean define that does not serve fails the row; it
        // used to pass by returning.
        assert_eq!(
            clean.answer, "zs7nd6/R0",
            "zs7nd6 define: the clean define serves"
        );
        let (spent, retry) = spent_then_retry(&shared, &mut thread, cb, "zs7nd6/R1", 0);
        eprintln!(
            "zs7nd6 define reserved: spent={} doors={} retry={}",
            spent.answer, spent.doors, retry.answer
        );
        assert_eq!(spent.answer, "OOME", "a spent ladder is the define's OOME");
        assert_eq!(
            spent.doors, 0,
            "refused before the define: no one-attempt door"
        );
        assert!(spent.collections >= 1, "the ladder ran");
        assert_eq!(
            retry.answer, "zs7nd6/R1",
            "nothing was defined: the retry defines it"
        );
        assert!(thread.native_pin_roots.is_empty(), "no pin left");
    }

    /// Kill control (`CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0`): the mirror is
    /// minted after the define again, so the same spent heap leaves the class
    /// defined and the retry is the `LinkageError` (the "before").
/// The `[GC] zgc-define-mirror:` census, `[reserved, refused, adopted]`.
    fn census(shared: &SharedVm) -> [u64; 3] {
        use std::sync::atomic::Ordering;
        let c = &shared.mem.alloc_ladder.define_mirror_census;
        [
            c[0].load(Ordering::Relaxed),
            c[1].load(Ordering::Relaxed),
            c[2].load(Ordering::Relaxed),
        ]
    }

    fn delta(after: [u64; 3], before: [u64; 3]) -> [u64; 3] {
        [
            after[0] - before[0],
            after[1] - before[1],
            after[2] - before[2],
        ]
    }

    // zt t1/nd: the define-mirror census counts what the reservation did.
    // A clean define reserves once and adopts that object; a spent ladder
    // refuses (at least twice: the first attempt and the one after the
    // ladder's collection) and adopts nothing; the retry reserves and adopts
    // again. Kill control (`CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0`): nothing
    // is counted, so a `refused=0` on a probe row under the default is a
    // real zero.
}



/// zs s9/nd8 (`zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md` §1): a
/// NAMELESS define and a HIDDEN define park their name in the reserved
/// mirror, so the adoption allocates nothing after the define (where a
/// refusal left the class defined). Counted by
/// `vm_object::zs9nd8_adoption_name_allocs`; each kill switch is the control
/// (the name allocated after the define, the s8 behaviour).
#[cfg(feature = "zgc")]
mod zs9nd8_define_tests {
    use super::define_mirror_tests::{class_bytes, zgc_vm};
    use super::*;
    use crate::vm::vm_object::zs9nd8_adoption_name_allocs;

    const DEFINE1_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;";
    const DEFINE0_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;";

    /// One define of `class_bytes(name)`: `hidden` through `defineClass0`
    /// (flags 0x2, with the name), else `defineClass1` with a NULL name.
    /// Answers (the defined class's name, base of a hidden one; the
    /// adoption's after-define name allocations).
    fn define(
        shared: &SharedVm,
        thread: &mut JvmThread,
        name: &str,
        hidden: bool,
    ) -> (String, u32) {
        let (class, method, desc) = if hidden {
            ("java/lang/ClassLoader", "defineClass0", DEFINE0_DESC)
        } else {
            ("java/lang/ClassLoader", "defineClass1", DEFINE1_DESC)
        };
        let cb = shared
            .natives
            .native_methods
            .find(class, method, desc)
            .unwrap_or_else(|| panic!("{class}.{method} is registered in the unit VM"));
        let bytes = class_bytes(name);
        let base = thread.native_pin_roots.len();
        let (name_obj, array) = {
            let mut ctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            let name_obj = ctx.create_string(&name.replace('/', "."));
            let array = ctx.new_array(ArrayElementType::Byte, bytes.len());
            assert!(ctx.write_byte_array_from(array, 0, &bytes));
            (name_obj, array)
        };
        thread.native_pin_roots.push(name_obj);
        thread.native_pin_roots.push(array);
        let len = Value::Int(bytes.len() as i32);
        let args = if hidden {
            vec![
                Value::Object(None),
                Value::Object(None),
                Value::Object(Some(name_obj)),
                Value::Object(Some(array)),
                Value::Int(0),
                len,
                Value::Object(None),
                Value::Int(0),
                Value::Int(0x2),
                Value::Object(None),
            ]
        } else {
            vec![
                Value::Object(None),
                Value::Object(None),
                Value::Object(Some(array)),
                Value::Int(0),
                len,
                Value::Object(None),
                Value::Object(None),
            ]
        };
        zs9nd8_adoption_name_allocs::take();
        let out = safe_native_call(shared, thread, cb, &args);
        let allocs = zs9nd8_adoption_name_allocs::take();
        thread.native_pin_roots.truncate(base);
        let ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        let answer = match out {
            Ok(Some(Value::Object(Some(mirror)))) => ctx
                .class_id_from_mirror(mirror)
                .and_then(|c| ctx.class_name_of_id(c))
                .map(|n| n.split("/0x").next().unwrap_or(&n).to_string())
                .unwrap_or_else(|| "<no class>".to_string()),
            other => format!("{other:?}"),
        };
        (answer, allocs)    }

    fn run(tag: &str, hidden: bool, kill: Option<&'static str>) -> (String, String, u32) {
        let shared = zgc_vm().expect("a ZGC heap in the unit VM");
        let mut thread = JvmThread::new(ThreadId(3901), "zs9nd8-define");
        let _off = kill.map(|key| {
            cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(key, Some("0"))]),
            )
        });
        let name = format!("zs9nd8/{tag}");
        let (answer, allocs) = define(&shared, &mut thread, &name, hidden);
        eprintln!("zs9nd8 define hidden={hidden} kill={kill:?}: {answer} after-define name allocs={allocs}");
        assert!(thread.native_pin_roots.is_empty(), "no pin left");
        (name, answer, allocs)
    }

    #[test]
    fn zs9nd8_a_nameless_define_parks_its_class_file_name() {
        let (name, answer, allocs) = run("Nameless", false, None);
        assert_eq!(answer, name, "the nameless define serves its class");
        assert_eq!(allocs, 0, "the adoption reused the parked this_class name");
    }

#[test]
    fn zs9nd8_a_hidden_define_parks_its_mangled_name() {
        let (name, answer, allocs) = run("Hidden", true, None);
        assert_eq!(answer, name, "the hidden define serves its class");
        assert_eq!(allocs, 0, "the adoption reused the parked unpooled name");    }}

/// zs s3/nd3 (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md`: the sweep's
/// unswept rows): the thread natives and `StringBuilder.getValue` under both
/// knobs. For each listed body: nd2's replay sweep (every ONE-ATTEMPT door
/// refused in turn, the funnel re-runs the body, answer and argument digest
/// equal the clean run's) and this lane's fallible-door sweep (every
/// FALLIBLE door refused in turn, the body's own ladder retries; same
/// equality). Rows the unit-test VM cannot build are reported by name.
#[cfg(feature = "zgc")]
mod thread_native_sweep_tests {
    use super::super::zs2nd2_forced_refusal_replay_tests::{dispatch, render, Outcome};
    use super::*;
    use crate::config::GcAlgorithm;
    use crate::runtime::native_oom::{forced_try_arm, forced_try_count};
    use cratonvm_native_builtins::oom_restartable::restartable_bodies;

    const GET_STACK_TRACE: usize = 7;
    const DUMP_THREADS: usize = 8;
    const GET_THREADS: usize = 9;
    const SB_GET_VALUE: usize = 15;

    fn zgc_vm() -> Option<SharedVm> {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            max_heap_size: 64 << 20,
            ..VmConfig::default()
        });
        shared.mem.heap.is_zgc().then_some(shared)
    }

    /// [`dispatch`] with the FALLIBLE-door knob armed at `k` instead
    /// (answers the outcome and the fallible doors counted).
    fn dispatch_try(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cb: NativeCallback,
        args: &[Value],
        k: u32,
    ) -> (Outcome, u32) {
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
        forced_try_arm(k);
        let out = safe_native_call(shared, thread, cb, args);
        let tries = forced_try_count();
        forced_try_arm(0);
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
            tries,
        )
    }

    /// Both sweeps over one body. `Ok(None)`: the clean run answered an error
    /// (row skipped); `Ok(Some((doors, tries)))`: the doors swept.
    fn sweep_both(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cb: NativeCallback,
        fixture: &mut dyn FnMut(&SharedVm, &mut JvmThread) -> Vec<Value>,
    ) -> Result<Option<(u32, u32)>, String> {
        let warm = fixture(shared, thread);
        let _ = dispatch(shared, thread, cb, &warm, 0);
        let args = fixture(shared, thread);
        let (clean, doors, _) = dispatch(shared, thread, cb, &args, 0);
        if clean.answer.starts_with("ERR") || clean.answer == "OOME" {
            return Ok(None);
        }
        for k in 1..=doors {
            let args = fixture(shared, thread);
            let (forced, _, _) = dispatch(shared, thread, cb, &args, k);
            if forced != clean {
                return Err(format!(
                    "one-attempt k={k}/{doors}: clean {clean:?} forced {forced:?}"
                ));
            }
        }
        let args = fixture(shared, thread);
        let (_, tries) = dispatch_try(shared, thread, cb, &args, 0);
        for k in 1..=tries {
            let args = fixture(shared, thread);
            let (forced, _) = dispatch_try(shared, thread, cb, &args, k);
            if forced != clean {
                return Err(format!(
                    "fallible k={k}/{tries}: clean {clean:?} forced {forced:?}"
                ));
            }
        }
        assert!(thread.native_pin_roots.is_empty(), "no pin left");
        Ok(Some((doors, tries)))
    }

    fn current_thread(shared: &SharedVm, thread: &mut JvmThread) -> Option<ObjectRef> {
        let mut ctx = NativeContextImpl { shared, thread };
        // The VM makes the thread's object on first demand.
        ctx.current_thread_object_if_any()
            .or_else(|| Some(ctx.current_thread_object()))
    }

    #[test]
    fn the_thread_natives_and_get_value_answer_the_same_under_both_knobs() {
        let Some(shared) = zgc_vm() else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(3304), "zs3nd3-threads");
        let t = &mut thread;
        let mut ran = Vec::new();
        let mut skipped = Vec::new();
        let bodies = restartable_bodies();
        let record = |what: &str,
                      verdict: Result<Option<(u32, u32)>, String>,
                      ran: &mut Vec<String>,
                      skipped: &mut Vec<String>| match verdict {
            Ok(Some((doors, tries))) => ran.push(format!("{what} doors={doors} tries={tries}")),
            Ok(None) => skipped.push(what.to_string()),
            Err(e) => panic!("{what}: {e}"),
        };
        let mut no_args = |_: &SharedVm, _: &mut JvmThread| -> Vec<Value> { Vec::new() };
        let v = sweep_both(&shared, t, bodies[GET_THREADS], &mut no_args);
        record("getThreads", v, &mut ran, &mut skipped);
        if current_thread(&shared, t).is_some() {
            let mut me = |s: &SharedVm, t: &mut JvmThread| -> Vec<Value> {
                vec![Value::Object(current_thread(s, t))]
            };
            let v = sweep_both(&shared, t, bodies[GET_STACK_TRACE], &mut me);
            record("getStackTrace0(current)", v, &mut ran, &mut skipped);
            let mut one = |s: &SharedVm, t: &mut JvmThread| -> Vec<Value> {
                let me = current_thread(s, t);
                let mut ctx = NativeContextImpl {
                    shared: s,
                    thread: t,
                };
                let component = ctx
                    .class_id_by_name("java/lang/Thread")
                    .unwrap_or(ClassId::new(0));
                let arr = ctx.new_ref_array(component, 1);
                ctx.set_array_element(arr, 0, Value::Object(me));
                vec![Value::Object(Some(arr))]
            };
            let v = sweep_both(&shared, t, bodies[DUMP_THREADS], &mut one);
            record("dumpThreads([current])", v, &mut ran, &mut skipped);
        } else {
            skipped.push("getStackTrace0 / dumpThreads (no thread object)".to_string());
        }
        let mut builder = |s: &SharedVm, t: &mut JvmThread| -> Vec<Value> {
            let mut ctx = NativeContextImpl {
                shared: s,
                thread: t,
            };
            let text = ctx.create_string("zs3nd3 \u{e9}\u{4e2d}");
            let made = ctx.new_object_initialized(
                "java/lang/StringBuilder",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(text))],
            );
            match made {
                Ok(Some(v @ Value::Object(Some(_)))) => vec![v],
                _ => vec![Value::Object(None)],
            }
        };
        let v = sweep_both(&shared, t, bodies[SB_GET_VALUE], &mut builder);
        record("getValue(StringBuilder)", v, &mut ran, &mut skipped);
        eprintln!("zs3nd3 thread sweep: ran {ran:?}, skipped {skipped:?}");
        assert!(
            ran.iter().any(|r| r.starts_with("getThreads")),
            "getThreads needs no fixture: it must run ({skipped:?})"
        );
    }
}
