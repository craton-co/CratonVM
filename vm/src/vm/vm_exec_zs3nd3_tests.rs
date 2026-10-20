// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s3/nd3 tests (ZGC round zs, wave s3, lane nd3), a child module of
//! `vm_exec` (`#[path]`), so they reach the funnel and `NativeContextImpl`.
//!
//! 1. [`listing_tests`]: the first declared reflection listing of a VM that
//!    has not loaded `Field` / `Method` / `Constructor` answers a typed array
//!    (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md` §6).

use super::*;
use crate::config::VmConfig;

mod listing_tests {
    use super::*;
    use cratonvm_native_builtins::oom_restartable::restartable_bodies;

    // Indices into `restartable_bodies()` (frozen, in order, by the ratchet).
    const GET_DECLARED_FIELDS: usize = 0;
    const GET_DECLARED_METHODS: usize = 1;
    const GET_DECLARED_CONSTRUCTORS: usize = 2;

    /// The class name of the array the FIRST `idx` listing of a fresh VM
    /// answers for `of` (`java/lang/String` when loaded), or `None` when the
    /// precondition does not hold here (no JDK class to list, or `component`
    /// already loaded at boot: the defect cannot be reached).
    fn first_listing_array_class(
        component: &str,
        idx: usize,
        of: &str,
        kill: bool,
    ) -> Option<String> {
        let _flags = kill.then(|| {
            cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_REFLECTION_LISTING_LOADS_COMPONENT",
                    Some("0"),
                )]),
            )
        });
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(3301), "zs3nd3-listing");
        let mirror = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            // The listed class, loaded (not initialised) first: a fresh
            // unit-test VM has loaded almost nothing.
            let mirror = match ctx.load_class(of) {
                Ok(Some(Value::Object(Some(m)))) => m,
                other => {
                    eprintln!("zs3nd3 listing: {of} not loadable here ({other:?}); row skipped");
                    return None;
                }
            };
            if ctx.class_id_by_name(component).is_some() {
                eprintln!("zs3nd3 listing: {component} loaded before the listing; row skipped");
                return None;
            }
            mirror
        };
        let base = thread.native_pin_roots.len();
        thread.native_pin_roots.push(mirror);
        let out = safe_native_call(
            &shared,
            &mut thread,
            restartable_bodies()[idx],
            &[Value::Object(Some(mirror)), Value::Int(0)],
        );
        thread.native_pin_roots.truncate(base);
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        match out {
            Ok(Some(Value::Object(Some(arr)))) => {
                let cid = ctx.class_id_of_object(arr);
                Some(ctx.class_name_of_id(cid).unwrap_or_default())
            }
            other => panic!("{component} listing answered {other:?}"),
        }
    }

    fn check(component: &str, idx: usize, of: &str) {
        let Some(fixed) = first_listing_array_class(component, idx, of, false) else {
            return;
        };
        // A reference array's header names its COMPONENT class
        // (`class_id_of_object`), which `getClass()` turns into `[L..;`.
        assert_eq!(fixed, component, "first {component} listing");
        // Kill control: the by-name lookup reproduces the defect, so the
        // green arm above is not vacuous.
        let killed = first_listing_array_class(component, idx, of, true)
            .expect("the precondition held on the default arm");
        assert_ne!(killed, component, "kill arm: the old answer");
        eprintln!("zs3nd3 listing: {component}: fixed={fixed} kill={killed}");
    }

    #[test]
    fn the_first_get_declared_fields_answers_a_field_array() {
        check(
            "java/lang/reflect/Field",
            GET_DECLARED_FIELDS,
            "java/lang/String",
        );
    }

    #[test]
    fn the_first_get_declared_methods_answers_a_method_array() {
        check(
            "java/lang/reflect/Method",
            GET_DECLARED_METHODS,
            "java/lang/String",
        );
    }

    #[test]
    fn the_first_get_declared_constructors_answers_a_constructor_array() {
        check(
            "java/lang/reflect/Constructor",
            GET_DECLARED_CONSTRUCTORS,
            "java/lang/String",
        );
    }
}

/// zs s3/nd3: the forced-refusal sweep for a COLLECTING conversion
/// (`native_oom::forced_try_refusal`, the fallible-door twin of nd2's
/// one-attempt knob). A definer is not restartable, so nd2's replay sweep
/// cannot cover it; instead every fallible door a clean
/// `ClassLoader.defineClass1` enters is refused once in turn, and each run
/// must still answer the class it defined (`lang_system::defined_class_mirror`
/// collects and retries), never `OutOfMemoryError`.
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
        matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Zgc(_)).then_some(shared)
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
    #[test]
    fn the_kill_switch_takes_the_one_attempt_mirror() {
        let Some((shared, cb)) = setup() else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(3303), "zs3nd3-define-kill");
        let on = define(&shared, &mut thread, cb, "zs3nd3/K0", 0, 0);
        if on.answer != "zs3nd3/K0" {
            eprintln!(
                "zs3nd3 define: the clean define answered {}; skipped",
                on.answer
            );
            return;
        }
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_ZGC_DEFINE_MIRROR_COLLECT",
                Some("0"),
            )]),
        );
        let off = define(&shared, &mut thread, cb, "zs3nd3/K1", 0, 0);
        assert_eq!(off.answer, "zs3nd3/K1");
        assert_eq!(
            off.tries + 1,
            on.tries,
            "the mirror left the fallible doors"
        );
        assert_eq!(off.doors, on.doors + 1, "and joined the one-attempt doors");
        // The mirror is the LAST one-attempt door under the kill (it follows
        // the define). Refuse it.
        let refused = define(&shared, &mut thread, cb, "zs3nd3/K2", 0, off.doors);
        assert_eq!(
            refused.answer, "OOME",
            "kill arm: the mirror's refusal is the OOME"
        );
        let again = define(&shared, &mut thread, cb, "zs3nd3/K2", 0, 0);
        eprintln!(
            "zs3nd3 define kill: doors={} refused={} redefine={}",
            off.doors, refused.answer, again.answer
        );
        assert!(
            again.answer.contains("Linkage") || again.answer.contains("uplicate"),
            "the class was left defined: {}",
            again.answer
        );
    }

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
    #[test]
    fn zs7nd6_the_kill_switch_leaves_the_class_defined() {
        let Some((shared, cb)) = setup() else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(3305), "zs7nd6-define-reserved-kill");
        let clean = define(&shared, &mut thread, cb, "zs7nd6/K0", 0, 0);
        // rv5 (zs s8): a clean define that does not serve fails the row; it
        // used to pass by returning.
        assert_eq!(
            clean.answer, "zs7nd6/K0",
            "zs7nd6 define: the clean define serves"
        );
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_ZGC_DEFINE_MIRROR_RESERVED",
                Some("0"),
            )]),
        );
        // With every fallible door refused, the kill arm's mint falls
        // through to the one-attempt mirror door AFTER the define's own
        // one-attempt doors (`off.doors` of them on a clean run, where the
        // fallible mirror serves): refuse it, so the heap is spent at the
        // mirror.
        let off = define(&shared, &mut thread, cb, "zs7nd6/K2", 0, 0);
        assert_eq!(off.answer, "zs7nd6/K2");
        let (spent, retry) = spent_then_retry(&shared, &mut thread, cb, "zs7nd6/K1", off.doors + 1);
        eprintln!(
            "zs7nd6 define kill: spent={} retry={}",
            spent.answer, retry.answer
        );
        assert_eq!(spent.answer, "OOME");
        assert!(
            retry.answer.contains("Linkage") || retry.answer.contains("uplicate"),
            "kill arm: the class was left defined: {}",
            retry.answer
        );
    }

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

    /// zt t1/nd: the define-mirror census counts what the reservation did.
    /// A clean define reserves once and adopts that object; a spent ladder
    /// refuses (at least twice: the first attempt and the one after the
    /// ladder's collection) and adopts nothing; the retry reserves and adopts
    /// again. Kill control (`CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0`): nothing
    /// is counted, so a `refused=0` on a probe row under the default is a
    /// real zero.
    #[test]
    fn zt1nd_the_define_mirror_census_counts_the_reservation() {
        let Some((shared, cb)) = setup() else {
            return;
        };
        let mut thread = JvmThread::new(ThreadId(3306), "zt1nd-define-census");
        let before = census(&shared);
        let clean = define(&shared, &mut thread, cb, "zt1nd/C0", 0, 0);
        assert_eq!(clean.answer, "zt1nd/C0", "the clean define serves");
        assert_eq!(delta(census(&shared), before), [1, 0, 1], "reserved + adopted");
        let line = crate::vm::vm_object::zgc_define_mirror_line(&shared).expect("a ZGC line");
        assert!(line.starts_with("[GC] zgc-define-mirror: reserved="), "{line}");

        let before = census(&shared);
        let (spent, retry) = spent_then_retry(&shared, &mut thread, cb, "zt1nd/C1", 0);
        assert_eq!(spent.answer, "OOME");
        assert_eq!(retry.answer, "zt1nd/C1");
        let d = delta(census(&shared), before);
        assert!(d[1] >= 2, "the spent ladder refused before and after: {d:?}");
        assert_eq!((d[0], d[2]), (1, 1), "only the retry reserved and adopted: {d:?}");

        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_ZGC_DEFINE_MIRROR_RESERVED",
                Some("0"),
            )]),
        );
        let before = census(&shared);
        let off = define(&shared, &mut thread, cb, "zt1nd/C2", 0, 0);
        assert_eq!(off.answer, "zt1nd/C2");
        assert_eq!(delta(census(&shared), before), [0, 0, 0], "kill: nothing reserved");
    }
}

/// zs s8/nd7 (`zgc-zs7nd6-native-door-residuals-FIXED-20261019.md` §1): the definers
/// other than `defineClass1/2` reserve their mirror before the define too. For
/// each door the unit-test VM registers: with the WHOLE ladder spent
/// (`forced_try_refuse_all`) the define answers `OutOfMemoryError` before any
/// one-attempt door (nothing defined), and the same name defined again is a
/// class; under `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER=0` the
/// spent heap at the mirror leaves the class defined (the retry is a
/// duplicate-definition error). Hidden classes have no retry collision, so
/// their kill row pins the "refused after the define" half only.
mod zs8nd7_define_tests {
    use super::define_mirror_tests::{class_bytes, ladder_engagements, zgc_vm, Defined};
    use super::*;
    use crate::runtime::native_oom::{
        forced_door_arm, forced_door_count, forced_try_arm, forced_try_count, forced_try_refuse_all,
    };

    const DEFINE0_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;";
    const UNSAFE_DESC: &str = "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;";

    /// A define door and its argument layout.
    #[derive(Clone, Copy, Debug)]
    enum Door {
        /// `ClassLoader.defineClass0` (`Lookup.defineClass` / `defineHiddenClass`)
        /// with a null loader and lookup; `flags` 0x02 = hidden.
        DefineClass0 { flags: i32 },
        /// `Unsafe.defineClass[0]` on `class` with a null loader and domain.
        Unsafe {
            class: &'static str,
            method: &'static str,
        },
    }

    impl Door {
        fn find(self, shared: &SharedVm) -> Option<NativeCallback> {
            let (class, method, desc) = match self {
                Door::DefineClass0 { .. } => {
                    ("java/lang/ClassLoader", "defineClass0", DEFINE0_DESC)
                }
                Door::Unsafe { class, method } => (class, method, UNSAFE_DESC),
            };
            shared.natives.native_methods.find(class, method, desc)
        }

        fn hidden(self) -> bool {
            matches!(self, Door::DefineClass0 { flags } if flags & 0x2 != 0)
        }

        fn args(self, name_obj: ObjectRef, array: ObjectRef, len: usize) -> Vec<Value> {
            match self {
                Door::DefineClass0 { flags } => vec![
                    Value::Object(None),
                    Value::Object(None),
                    Value::Object(Some(name_obj)),
                    Value::Object(Some(array)),
                    Value::Int(0),
                    Value::Int(len as i32),
                    Value::Object(None),
                    Value::Int(0),
                    Value::Int(flags),
                    Value::Object(None),
                ],
                Door::Unsafe { .. } => vec![
                    Value::Object(None),
                    Value::Object(Some(name_obj)),
                    Value::Object(Some(array)),
                    Value::Int(0),
                    Value::Int(len as i32),
                    Value::Object(None),
                    Value::Object(None),
                ],
            }
        }
    }

    /// One define through `door` with the one-attempt knob at `door_k` (`0`:
    /// count only); `spent` refuses every fallible door.
    fn define(
        shared: &SharedVm,
        thread: &mut JvmThread,
        door: Door,
        cb: NativeCallback,
        name: &str,
        door_k: u32,
        spent: bool,
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
        let args = door.args(name_obj, array, bytes.len());
        forced_try_arm(0);
        forced_door_arm(door_k);
        forced_try_refuse_all(spent);
        let out = safe_native_call(shared, thread, cb, &args);
        forced_try_refuse_all(false);
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
                // A hidden class answers its mangled name: keep its base.
                .map(|n| n.split("/0x").next().unwrap_or(&n).to_string())
                .unwrap_or_else(|| "<no class>".to_string()),
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { .. },
            ))) => "OOME".to_string(),
            // A thrown Java exception: its class (the Unsafe doors build a
            // `java/lang/LinkageError` object for a duplicate).
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                let cid = shared.mem.heap.class_id_of(exc);
                format!("thrown {:?}", ctx.class_name_of_id(cid))
            }
            other => format!("{other:?}"),
        };
        Defined {
            answer,
            tries,
            doors,
            collections,
        }
    }

    /// One door's run: `(clean, spent, retry)`, or `None` when the door is
    /// not registered here or a clean define does not serve.
    fn run_door(door: Door, tag: &str, kill: bool) -> Option<(Defined, Defined, Defined)> {
        let shared = zgc_vm()?;
        let Some(cb) = door.find(&shared) else {
            eprintln!("zs8nd7 {door:?}: not registered here; row skipped");
            return None;
        };
        let mut thread = JvmThread::new(ThreadId(3801), "zs8nd7-define");
        let first_name = format!("zs8nd7/{tag}C");
        let first = define(&shared, &mut thread, door, cb, &first_name, 0, false);
        assert_eq!(
            first.answer, first_name,
            "{door:?}: the clean define serves"
        );
        let _off = kill.then(|| {
            cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER",
                    Some("0"),
                )]),
            )
        });
        // Under the kill the mint after the define falls through to the
        // one-attempt mirror door AFTER the define's own one-attempt doors
        // (`clean.doors` of them on a clean run, where the fallible mirror
        // serves): refuse it, so the heap is spent at the mirror.
        let clean_name = format!("zs8nd7/{tag}P");
        let clean = define(&shared, &mut thread, door, cb, &clean_name, 0, false);
        assert_eq!(clean.answer, clean_name, "{door:?}: a clean define serves");
        let door_k = if kill { clean.doors + 1 } else { 0 };
        let name = format!("zs8nd7/{tag}R");
        let spent = define(&shared, &mut thread, door, cb, &name, door_k, true);
        let retry = define(&shared, &mut thread, door, cb, &name, 0, false);
        eprintln!(
            "zs8nd7 {door:?} kill={kill}: clean tries={} doors={}; spent={} doors={} collections={}; retry={}",
            clean.tries, clean.doors, spent.answer, spent.doors, spent.collections, retry.answer
        );
        assert!(thread.native_pin_roots.is_empty(), "{door:?}: no pin left");
        Some((clean, spent, retry))
    }

    fn assert_reserved(door: Door, tag: &str) {
        let Some((clean, spent, retry)) = run_door(door, tag, false) else {
            return;
        };
        assert!(
            clean.tries >= 1,
            "{door:?}: the reservation is a fallible door"
        );
        assert_eq!(
            spent.answer, "OOME",
            "{door:?}: a spent ladder is the define's OOME"
        );
        assert_eq!(
            spent.doors, 0,
            "{door:?}: refused before the define (no one-attempt door)"
        );
        assert!(spent.collections >= 1, "{door:?}: the ladder ran");
        let name = format!("zs8nd7/{tag}R");
        assert_eq!(
            retry.answer, name,
            "{door:?}: nothing was defined, the retry defines it"
        );
    }

    fn assert_kill_leaves_it_defined(door: Door, tag: &str) {
        let Some((clean, spent, retry)) = run_door(door, tag, true) else {
            return;
        };
        assert_eq!(
            spent.answer, "OOME",
            "{door:?} kill: the mirror's refusal is the OOME"
        );
        // The refused door is the one after the define's own one-attempt
        // doors (`clean.doors`, often 0): the mint, after the define.
        assert_eq!(
            spent.doors,
            clean.doors + 1,
            "{door:?} kill: refused AFTER the define"
        );
        if !door.hidden() {
            assert!(
                retry.answer.contains("Linkage") || retry.answer.contains("uplicate"),
                "{door:?} kill: the class was left defined: {}",
                retry.answer
            );
        }
    }

    const DEFINE0: Door = Door::DefineClass0 { flags: 0 };
    const DEFINE0_HIDDEN: Door = Door::DefineClass0 { flags: 0x2 };
    const UNSAFE_INTERNAL: Door = Door::Unsafe {
        class: "jdk/internal/misc/Unsafe",
        method: "defineClass0",
    };
    const UNSAFE_SUN: Door = Door::Unsafe {
        class: "sun/misc/Unsafe",
        method: "defineClass",
    };

    #[test]
    fn zs8nd7_define_class0_fails_before_it_publishes() {
        assert_reserved(DEFINE0, "D0");
    }

    #[test]
    fn zs8nd7_define_class0_kill_leaves_the_class_defined() {
        assert_kill_leaves_it_defined(DEFINE0, "K0");
    }

    #[test]
    fn zs8nd7_hidden_define_class0_fails_before_it_publishes() {
        assert_reserved(DEFINE0_HIDDEN, "H0");
    }

    #[test]
    fn zs8nd7_hidden_define_class0_kill_refuses_after_the_define() {
        assert_kill_leaves_it_defined(DEFINE0_HIDDEN, "HK0");
    }

    #[test]
    fn zs8nd7_unsafe_define_class_fails_before_it_publishes() {
        assert_reserved(UNSAFE_INTERNAL, "U0");
        assert_reserved(UNSAFE_SUN, "S0");
    }

    #[test]
    fn zs8nd7_unsafe_define_class_kill_leaves_the_class_defined() {
        assert_kill_leaves_it_defined(UNSAFE_INTERNAL, "UK0");
        assert_kill_leaves_it_defined(UNSAFE_SUN, "SK0");
    }

    const DEFINE1_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;";

    /// `defineClass1(null, name, <bytes of bytes_name>, ..)`: the answer.
    fn define1(
        shared: &SharedVm,
        thread: &mut JvmThread,
        cb: NativeCallback,
        name: &str,
        bytes_name: &str,
    ) -> String {
        let bytes = class_bytes(bytes_name);
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
        let args = [
            Value::Object(None),
            Value::Object(Some(name_obj)),
            Value::Object(Some(array)),
            Value::Int(0),
            Value::Int(bytes.len() as i32),
            Value::Object(None),
            Value::Object(None),
        ];
        let out = safe_native_call(shared, thread, cb, &args);
        thread.native_pin_roots.truncate(base);
        let ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        match out {
            Ok(Some(Value::Object(Some(mirror)))) => ctx
                .class_id_from_mirror(mirror)
                .and_then(|c| ctx.class_name_of_id(c))
                .unwrap_or_else(|| "<no class>".to_string()),
            other => format!("{other:?}"),
        }
    }

    /// §2: a define that fails after the reservation (bytes naming another
    /// class) leaves its name out of the string pool; `kill` =
    /// `CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION=0`. Answers
    /// `(failed answer, pooled after the failure, pooled after a success)`,
    /// or `None` when the row cannot run here.
    fn failed_define_pools(tag: &str, kill: bool) -> Option<(String, bool, bool)> {
        let shared = zgc_vm()?;
        let Some(cb) = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            "defineClass1",
            DEFINE1_DESC,
        ) else {
            eprintln!("zs8nd7 pool: defineClass1 not registered here; row skipped");
            return None;
        };
        let mut thread = JvmThread::new(ThreadId(3802), "zs8nd7-pool");
        let clean_name = format!("zs8nd7/{tag}C");
        assert_eq!(
            define1(&shared, &mut thread, cb, &clean_name, &clean_name),
            clean_name,
            "zs8nd7 pool: the clean define serves"
        );
        let _off = kill.then(|| {
            cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION",
                    Some("0"),
                )]),
            )
        });
        let name = format!("zs8nd7/{tag}F");
        let pooled = |shared: &SharedVm| shared.mem.string_pool.read().contains_key(&name);
        assert!(!pooled(&shared), "precondition: {name} not pooled yet");
        let failed = define1(
            &shared,
            &mut thread,
            cb,
            &name,
            &format!("zs8nd7/{tag}Other"),
        );
        let after_failure = pooled(&shared);
        let ok = define1(&shared, &mut thread, cb, &name, &name);
        assert_eq!(ok, name, "the right bytes define the class");
        let after_success = pooled(&shared);
        eprintln!(
            "zs8nd7 pool kill={kill}: failed={failed} pooled_after_failure={after_failure} pooled_after_success={after_success}"
        );
        assert!(thread.native_pin_roots.is_empty(), "no pin left");
        Some((failed, after_failure, after_success))
    }

    #[test]
    fn zs8nd7_a_failed_define_leaves_no_pooled_name() {
        let Some((failed, after_failure, after_success)) = failed_define_pools("N0", false) else {
            return;
        };
        assert!(failed != "zs8nd7/N0F", "the wrong bytes fail the define");
        assert!(!after_failure, "a failed define pools no name");
        assert!(
            after_success,
            "the defined class's name is pooled at the adoption"
        );
    }

    /// rv5 (zs s8): under the pure-bytecode lever (`--jdk-only`) the mint
    /// leaves `Class.name` null so the JDK's `initClassName()` answers the
    /// binary name; the reservation must not park the internal name there.
    /// The lever is a process latch, so its answer is passed in; control arm:
    /// lever off parks the name.
    #[test]
    fn zs8nd7_the_pure_bytecode_lever_parks_no_name() {
        let Some(shared) = zgc_vm() else {
            eprintln!("zs8nd7 lever: no ZGC heap here; skipped");
            return;
        };
        let name = "zs8nd7/Lever";
        let parks = |keep_name_null: bool| {
            let mirror = crate::vm::vm_object::try_reserve_class_mirror_parking(
                &shared,
                Some(name),
                keep_name_null,
            )
            .expect("an empty heap holds the reservation");
            (0..shared.mem.heap.num_fields(mirror)).any(|i| {
                matches!(
                    shared.mem.heap.get_field(mirror, i),
                    Value::Object(Some(o))
                        if crate::vm::vm_object::read_java_string(&shared.mem.heap, o).as_deref()
                            == Some(name)
                )
            })
        };
        assert!(
            !parks(true),
            "lever on: no name parked in the reserved mirror"
        );
        assert!(parks(false), "control, lever off: the name is parked");
    }

    #[test]
    fn zs8nd7_the_name_kill_pools_a_failed_defines_name() {
        let Some((_, after_failure, after_success)) = failed_define_pools("NK0", true) else {
            return;
        };
        assert!(
            after_failure,
            "kill arm: the reservation pooled the name (nd6)"
        );
        assert!(after_success);
    }
}

/// zs s9/nd8 (`zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md` §1): a
/// NAMELESS define and a HIDDEN define park their name in the reserved
/// mirror, so the adoption allocates nothing after the define (where a
/// refusal left the class defined). Counted by
/// `vm_object::zs9nd8_adoption_name_allocs`; each kill switch is the control
/// (the name allocated after the define, the s8 behaviour).
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
        (answer, allocs)
    }

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
    fn zs9nd8_the_name_from_bytes_kill_allocates_after_the_define() {
        let (name, answer, allocs) = run(
            "NamelessK",
            false,
            Some("CRATONVM_ZGC_DEFINE_MIRROR_NAME_FROM_BYTES"),
        );
        assert_eq!(answer, name);
        assert_eq!(allocs, 1, "kill arm: the name String after the define (s8)");
    }

    #[test]
    fn zs9nd8_a_hidden_define_parks_its_mangled_name() {
        let (name, answer, allocs) = run("Hidden", true, None);
        assert_eq!(answer, name, "the hidden define serves its class");
        assert_eq!(allocs, 0, "the adoption reused the parked unpooled name");
    }

    #[test]
    fn zs9nd8_the_hidden_name_kill_allocates_after_the_define() {
        let (name, answer, allocs) = run(
            "HiddenK",
            true,
            Some("CRATONVM_ZGC_DEFINE_MIRROR_HIDDEN_NAME_PARKED"),
        );
        assert_eq!(answer, name);
        assert_eq!(allocs, 1, "kill arm: the name String after the define (s8)");
    }
}

/// zs s3/nd3 (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md`: the sweep's
/// unswept rows): the thread natives and `StringBuilder.getValue` under both
/// knobs. For each listed body: nd2's replay sweep (every ONE-ATTEMPT door
/// refused in turn, the funnel re-runs the body, answer and argument digest
/// equal the clean run's) and this lane's fallible-door sweep (every
/// FALLIBLE door refused in turn, the body's own ladder retries; same
/// equality). Rows the unit-test VM cannot build are reported by name.
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
        matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Zgc(_)).then_some(shared)
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
