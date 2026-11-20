use super::*;

/// The bail-list is per loaded class; these tests use id-less keys and unique
/// names instead.
const NO_ID: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0);

/// A name no production class can collide with, so these tests can mark the
/// bail-list without perturbing another test in the same binary.
fn unique(tag: &str) -> String {
    format!("cratonvm/test/compile_gate/{tag}")
}

/// Serialises every test that reads a process-global counter, does
/// something, and asserts the delta.
///
/// `unique()` keeps the *bail-list* keys from colliding, but `ADMISSIONS`
/// and `UNGATED_BACKEND_ENTRIES` are single counters shared by the whole
/// binary: three tests here call `admit(.., CompileDoor::Osr, None)` and three
/// call `note_backend_entry()`, so a read-then-assert pair in one sees
/// another's increment and the `+1` becomes `+2`. It reproduced as
/// `admissions_are_counted_per_door` failing in the full suite and passing
/// in isolation — the shape that reads as flakiness and is not.
///
/// A `>= before + 1` assertion would also make it pass, and would be
/// strictly worse: the point of the counter is that it moves by exactly the
/// number of admissions, and a `>=` cannot tell a double-count from a
/// correct one.
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn an_admission_opens_and_closes_the_thread_scope() {
    let _guard = COUNTER_LOCK.lock();
    assert!(!admission_is_open());
    let cls = unique("scope");
    let a = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &cls,
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("clean method admits");
    assert!(admission_is_open());
    assert_eq!(a.door(), CompileDoor::Osr);
    drop(a);
    assert!(!admission_is_open());
}

/// The nesting case: `callee_compiler` compiles a callee inside the
/// caller's compilation, so a flag would clear on the inner drop and make
/// the rest of the outer compile read as ungated.
#[test]
fn admissions_nest() {
    let _guard = COUNTER_LOCK.lock();
    let outer = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("nest-outer"),
        "m",
        "()V",
        CompileDoor::MethodEntry,
        None,
    )
    .expect("admits");
    let inner = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("nest-inner"),
        "m",
        "()V",
        CompileDoor::MethodEntry,
        None,
    )
    .expect("admits");
    drop(inner);
    assert!(
        admission_is_open(),
        "the outer compilation is still open after the callee's admission drops"
    );
    drop(outer);
    assert!(!admission_is_open());
}

/// The check `compile_osr_artifact` hand-copied. A bail-listed method is
/// refused at every door, not just the one that recorded the bail.
#[test]
fn a_bail_listed_method_is_refused_at_every_door() {
    let _guard = COUNTER_LOCK.lock();
    let cls = unique("bail-listed");
    crate::mark_jit_bail_listed(NO_ID, &cls, "m", "()V");
    for door in CompileDoor::ALL {
        assert_eq!(
            admit(
                crate::process_jit_verdicts(),
                NO_ID,
                &cls,
                "m",
                "()V",
                door,
                None
            )
            .err(),
            Some(CompileRefusal::PermanentlyBailListed),
            "{} must honour the bail-list",
            door.label()
        );
    }
}

/// Interpreter round i1 wave 11: the bail list is the COMPILING VM's. A
/// method bail-listed in one VM's registry is refused at every door of that
/// VM, and admitted by another VM's registry and by the no-VM one, under
/// the same class id and names (two VMs allocate `ClassId`s from 0, so they
/// routinely share both).
#[test]
fn a_bail_listed_method_is_refused_only_by_the_vm_that_listed_it() {
    let _guard = COUNTER_LOCK.lock();
    let id = cratonvm_types::ClassId::new(41);
    let cls = unique("per-vm-bail-list");
    let vm_a = crate::JitVerdictRegistry::default();
    let vm_b = crate::JitVerdictRegistry::default();
    vm_a.mark_bail_listed(id, &cls, "m", "()V");
    for door in CompileDoor::ALL {
        assert_eq!(
            admit(&vm_a, id, &cls, "m", "()V", door, None).err(),
            Some(CompileRefusal::PermanentlyBailListed),
            "{} must honour its own VM's bail-list",
            door.label()
        );
        assert_eq!(
            admit(&vm_b, id, &cls, "m", "()V", door, None).err(),
            None,
            "{} must not honour another VM's bail-list",
            door.label()
        );
        assert_eq!(
            admit(
                crate::process_jit_verdicts(),
                id,
                &cls,
                "m",
                "()V",
                door,
                None
            )
            .err(),
            None,
            "{}: a VM's bail must not reach the no-VM registry",
            door.label()
        );
    }
}

/// Interpreter round i1 wave 17: the admission opens the ADMITTING
/// registry's verdict witness. A permanent bail recorded by a compile of
/// VM A is kept while VM B redefines; a flush of VM A during the compile
/// still drops it (one more attempt, never a verdict about pre-flush
/// compile state).
#[test]
fn an_admission_witnesses_its_own_registrys_epochs() {
    let _guard = COUNTER_LOCK.lock();
    let id = cratonvm_types::ClassId::new(0x1753);
    let cls = unique("per-vm-verdict-witness");
    let vm_a = crate::JitVerdictRegistry::default();
    let vm_b = crate::JitVerdictRegistry::default();
    {
        let _admission = admit(&vm_a, id, &cls, "m", "()V", CompileDoor::MethodEntry, None)
            .expect("a clean method admits");
        vm_b.note_redefinition();
        vm_b.note_code_cache_flush();
        // Another VM's redefinition moves the process epoch as well. Set on
        // this thread's witness by hand rather than by moving the process
        // epoch, which would refuse a concurrent test's publication; the
        // admission's drop restores it.
        crate::COMPILE_INSTALL_EPOCH
            .with(|c| c.set(Some((crate::jit_install_epoch().wrapping_sub(1), 0))));
        vm_a.mark_bail_listed(id, &cls, "m", "()V");
    }
    assert!(vm_a.is_bail_listed(id, &cls, "m", "()V"));
    {
        let _admission = admit(&vm_a, id, &cls, "n", "()V", CompileDoor::Osr, None)
            .expect("a clean method admits");
        vm_a.note_code_cache_flush();
        vm_a.mark_bail_listed(id, &cls, "n", "()V");
    }
    assert!(!vm_a.is_bail_listed(id, &cls, "n", "()V"));
}

/// A permanent refusal and a transient one are different answers to
/// "should I ask again", and the callers act on that difference.
#[test]
fn only_the_cap_refusal_is_transient() {
    assert!(CompileRefusal::CodeCacheAtCapacity.is_transient());
    assert!(!CompileRefusal::PermanentlyBailListed.is_transient());
    assert!(!CompileRefusal::ForceInterpreted.is_transient());
    assert!(!CompileRefusal::JitDisabled.is_transient());
    assert!(!CompileRefusal::RuntimeDespeculated.is_transient());
}

/// **The N3 fix, stated as the difference between the two verdicts.**
///
/// The bail-list means *the backend cannot emit this method* and is
/// therefore honoured at all three doors — that is
/// `a_bail_listed_method_is_refused_at_every_door` above. A runtime
/// de-speculation verdict means *one call site in this method keeps
/// mis-speculating*, and the OSR door is not compiling that call site: it
/// is compiling a loop. Converting the second into the first is what let
/// one cold `assert x : "msg " + y;` under `-ea` — a non-bridged
/// `invokedynamic`, an unconditional reason-8 trap, `MakeNotCompilable` on
/// the FIRST occurrence — deny every unrelated hot loop in the method its
/// OSR entry for the rest of the process.
///
/// Written as a per-door table rather than as "OSR admits", because the
/// half that is easy to lose in a later edit is that the other two doors
/// still refuse: a verdict nobody honours anywhere is the tomcat
/// `Response.toAbsolute()` recompile thrash coming back.
#[test]
fn a_runtime_despeculated_method_is_refused_everywhere_except_the_osr_door() {
    let _guard = COUNTER_LOCK.lock();
    let cls = unique("runtime-despec");
    let despec = RuntimeDespecRegistry::new_registered();
    assert!(!despec.is_not_compilable(NO_ID, &cls, "m", "()V"));
    despec.mark(NO_ID, &cls, "m", "()V");
    assert!(despec.is_not_compilable(NO_ID, &cls, "m", "()V"));

    for door in CompileDoor::ALL {
        let got = admit(
            crate::process_jit_verdicts(),
            NO_ID,
            &cls,
            "m",
            "()V",
            door,
            Some(&despec),
        )
        .err();
        let want = match door {
            // A loop in this method may be perfectly compilable.
            CompileDoor::Osr => None,
            _ => Some(CompileRefusal::RuntimeDespeculated),
        };
        assert_eq!(
            got,
            want,
            "{} answered the runtime de-speculation verdict wrongly",
            door.label()
        );
    }

    // And an unrelated method of the same class keeps every door.
    assert_eq!(
        admit(
            crate::process_jit_verdicts(),
            NO_ID,
            &cls,
            "other",
            "()V",
            CompileDoor::MethodEntry,
            Some(&despec)
        )
        .err(),
        None,
        "the verdict is per method, not per class"
    );

    // A door asked with NO registry cannot raise the verdict at all. That
    // is the fail-OPEN direction and it is deliberate: a caller with no VM
    // has no runtime in which this method could have mis-speculated.
    assert_eq!(
        admit(
            crate::process_jit_verdicts(),
            NO_ID,
            &cls,
            "m",
            "()V",
            CompileDoor::MethodEntry,
            None
        )
        .err(),
        None
    );
}

/// **The M7 fix, stated as the property the process global did not have.**
///
/// Two VMs in one process allocate `ClassId`s independently, from 0, so
/// `(ClassId(41), "com/x/Service", "handle", "()V")` names a different
/// method in each and the SAME key in a process-wide map. One VM reaching
/// `MakeNotCompilable` then shut the method-entry and callee-dispatch doors
/// on the other VM's method, which had never mis-speculated. The tuple key
/// does keep two LOADERS apart — `a_runtime_verdict_is_per_loaded_class_identity`
/// asserts that — but it never separated two VMs, and the note that claimed
/// it did was reasoning about the wrong axis.
#[test]
fn one_vms_runtime_verdict_does_not_reach_another_vm() {
    let _guard = COUNTER_LOCK.lock();
    let cls = unique("despec-two-vms");
    let first = RuntimeDespecRegistry::new_registered();
    let second = RuntimeDespecRegistry::new_registered();
    // The same class id on purpose: that is what two VMs in one process
    // routinely produce, and it is the state the old key could not tell
    // apart.
    first.mark(NO_ID, &cls, "m", "()V");

    assert!(first.is_not_compilable(NO_ID, &cls, "m", "()V"));
    assert!(
        !second.is_not_compilable(NO_ID, &cls, "m", "()V"),
        "a second VM must not inherit a verdict it never earned"
    );
    assert_eq!(
        admit(
            crate::process_jit_verdicts(),
            NO_ID,
            &cls,
            "m",
            "()V",
            CompileDoor::MethodEntry,
            Some(&second)
        )
        .err(),
        None,
        "and the door it reaches must still open"
    );
}

/// A class that is gone must not leave a runtime verdict behind for the
/// next class loaded under its recycled id or its name.
///
/// The stamp catches a redefinition on its own — the redefine epoch moves —
/// but an UNLOAD moves neither epoch, and `ClassId`s are recycled. This is
/// why the tiered manager calls
/// [`forget_runtime_not_compilable_for_class`] beside
/// `JitVerdictRegistry::forget_for_class` on both paths.
#[test]
fn forgetting_a_class_drops_its_runtime_despeculation_verdicts() {
    let _guard = COUNTER_LOCK.lock();
    let gone = unique("despec-unloaded");
    let kept = unique("despec-survivor");
    let despec = RuntimeDespecRegistry::new_registered();
    despec.mark(NO_ID, &gone, "m", "()V");
    despec.mark(NO_ID, &gone, "n", "()I");
    despec.mark(NO_ID, &kept, "m", "()V");

    assert_eq!(despec.forget_for_class(NO_ID, &gone), 2);
    assert!(!despec.is_not_compilable(NO_ID, &gone, "m", "()V"));
    assert!(!despec.is_not_compilable(NO_ID, &gone, "n", "()I"));
    assert!(
        despec.is_not_compilable(NO_ID, &kept, "m", "()V"),
        "another class's verdict must survive"
    );
    assert_eq!(despec.forget_for_class(NO_ID, &kept), 1);
}

/// Two same-named classes in different loaders keep separate verdicts, and
/// a verdict recorded against one copy does not refuse the other.
///
/// The same property `mark_jit_bail_listed` has, asserted here because this
/// registry is keyed by a whole tuple rather than by `JIT_VERDICTS`' hash
/// and so cannot inherit that store's test.
#[test]
fn a_runtime_verdict_is_per_loaded_class_identity() {
    let _guard = COUNTER_LOCK.lock();
    let cls = unique("despec-loaders");
    let app = cratonvm_types::ClassId::new(7001);
    let plugin = cratonvm_types::ClassId::new(7002);
    let despec = RuntimeDespecRegistry::new_registered();
    despec.mark(app, &cls, "m", "()V");
    assert!(despec.is_not_compilable(app, &cls, "m", "()V"));
    assert!(
        !despec.is_not_compilable(plugin, &cls, "m", "()V"),
        "the other loader's copy is different bytecode and keeps its doors"
    );
    // And forgetting by identity leaves the other copy alone.
    despec.mark(plugin, &cls, "m", "()V");
    assert_eq!(despec.forget_for_class(app, &cls), 1);
    assert!(despec.is_not_compilable(plugin, &cls, "m", "()V"));
    assert_eq!(despec.forget_for_class(plugin, &cls), 1);
}

/// The verdict stops counting once the install epoch moves, like an OSR
/// denial — a code-cache flush replaced every body it was recorded about.
#[test]
fn a_runtime_verdict_expires_when_the_install_epoch_moves() {
    let _guard = COUNTER_LOCK.lock();
    let cls = unique("despec-epoch");
    let despec = RuntimeDespecRegistry::new_registered();
    despec.mark(NO_ID, &cls, "m", "()V");
    assert!(despec.is_not_compilable(NO_ID, &cls, "m", "()V"));
    // The registry's own flush hook, which moves only this registry's
    // epoch (interpreter round i1 wave 13; the process-wide install epoch
    // this used to read could not be moved by a test without refusing an
    // unrelated concurrent test's `put`).
    let other = RuntimeDespecRegistry::new_registered();
    other.mark(NO_ID, &cls, "m", "()V");
    despec.note_code_cache_flush();
    assert!(
        !despec.is_not_compilable(NO_ID, &cls, "m", "()V"),
        "a flush retires the speculation the verdict was about"
    );
    assert!(
        other.is_not_compilable(NO_ID, &cls, "m", "()V"),
        "another VM's flush does not"
    );
    other.note_redefinition();
    assert!(!other.is_not_compilable(NO_ID, &cls, "m", "()V"));
    assert_eq!(
        admit(
            crate::process_jit_verdicts(),
            NO_ID,
            &cls,
            "m",
            "()V",
            CompileDoor::MethodEntry,
            Some(&despec)
        )
        .err(),
        None
    );
}

/// The per-door counter is what makes "the OSR path calls the gate" a
/// measurement. A gate whose counter did not move would let the VM-side
/// assertion pass vacuously.
#[test]
fn admissions_are_counted_per_door() {
    let _guard = COUNTER_LOCK.lock();
    let before = admissions(CompileDoor::Osr);
    let _a = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("counted"),
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("admits");
    assert_eq!(admissions(CompileDoor::Osr), before + 1);
}

/// The drift witness itself. Written as an explicit
/// with-token/without-token pair because a counter that never moves and a
/// counter that always moves both read as "zero ungated entries" from the
/// VM side.
#[test]
fn the_ungated_witness_fires_only_without_a_token() {
    let _guard = COUNTER_LOCK.lock();
    let before = ungated_backend_entries();
    note_backend_entry();
    assert_eq!(
        ungated_backend_entries(),
        before + 1,
        "a backend entry with no admission open must be counted"
    );
    let _a = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("witness"),
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("admits");
    note_backend_entry();
    assert_eq!(
        ungated_backend_entries(),
        before + 1,
        "a backend entry under an admission must not be counted"
    );
}

/// The escape hatch must not launder a backend entry.
///
/// `for_backend_test` exists because an integration test under `jit/tests/`
/// is a separate crate and cannot reach a `#[cfg(test)]` constructor. That
/// makes it a real bypass of the type-level gate, so the *runtime* witness
/// has to keep seeing through it — otherwise a production caller could
/// reach for it and both layers would go quiet at once.
#[test]
fn the_backend_test_token_is_still_counted_as_ungated() {
    let _guard = COUNTER_LOCK.lock();
    let before = ungated_backend_entries();
    let t = CompileAdmission::for_backend_test();
    assert!(
        !admission_is_open(),
        "the test token must not open the thread scope"
    );
    note_backend_entry();
    assert_eq!(
        ungated_backend_entries(),
        before + 1,
        "a backend entry under the test token must still be counted"
    );
    drop(t);
    assert!(!admission_is_open());
}

/// A fresh admission has NOT declared a policy, and says so.
///
/// Written as its own test because the whole instrument depends on
/// "undeclared" being distinguishable from "compatible": `H12-1` §3b's
/// sharpest fact is that `--real-jdk` and `--fast-boot` produced an
/// identical bind profile, i.e. the door was answering "compatible" to a
/// question nobody asked it.
#[test]
fn a_fresh_admission_has_not_declared_a_direct_call_policy() {
    let _guard = COUNTER_LOCK.lock();
    let a = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("undeclared"),
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("admits");
    assert_eq!(a.direct_call_policy(), None);
    a.declare_direct_call_policy(DirectCallPolicy::JdkOnly);
    assert_eq!(a.direct_call_policy(), Some(DirectCallPolicy::JdkOnly));
    a.declare_direct_call_policy(DirectCallPolicy::Compatible);
    assert_eq!(a.direct_call_policy(), Some(DirectCallPolicy::Compatible));
}

/// The admission rule, in all three states, against both kinds.
///
/// The `JdkOnly`/`Bridge` cell is the only `false` in the table, and it is
/// the cell the OSR door was MEASURED getting wrong 298 000 times.
#[test]
fn the_direct_bind_rule_refuses_exactly_one_cell() {
    let _guard = COUNTER_LOCK.lock();
    let a = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("rule"),
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("admits");

    // Undeclared admits both, deliberately — see `admits_direct_bind`.
    assert!(a.admits_direct_bind(|| true));
    assert!(a.admits_direct_bind(|| false));

    a.declare_direct_call_policy(DirectCallPolicy::Compatible);
    assert!(a.admits_direct_bind(|| true));
    assert!(
        a.admits_direct_bind(|| false),
        "compatible mode binds a Bridge shadow; that is what it is for"
    );

    a.declare_direct_call_policy(DirectCallPolicy::JdkOnly);
    assert!(
        a.admits_direct_bind(|| true),
        "a reviewed Intrinsic is 1.4's exception and still binds under strict"
    );
    assert!(
        !a.admits_direct_bind(|| false),
        "a Bridge shadow must defer to the real JDK bytecode under --real-jdk"
    );
}

/// `Compatible` must not be reachable by forgetting to answer.
#[test]
fn the_policy_witness_has_no_default() {
    assert_eq!(
        DirectCallPolicy::from_jdk_only(true),
        DirectCallPolicy::JdkOnly
    );
    assert_eq!(
        DirectCallPolicy::from_jdk_only(false),
        DirectCallPolicy::Compatible
    );
    // The rule, stated once, asserted here so a future edit to
    // `admits_shadow_bind` has to come past a test.
    assert!(DirectCallPolicy::Compatible.admits_shadow_bind(false));
    assert!(!DirectCallPolicy::JdkOnly.admits_shadow_bind(false));
}

/// The bypass witness: rows under an undeclared admission are counted, rows
/// under a declared one are not.
///
/// The with/without pair is written out for the reason
/// `the_ungated_witness_fires_only_without_a_token` gives: a counter that
/// never moves and one that always moves both read as "zero" from the VM.
#[test]
fn undeclared_direct_binds_are_counted_and_declared_ones_are_not() {
    let _guard = COUNTER_LOCK.lock();
    let before = undeclared_direct_bind_rows(CompileDoor::Osr);

    let a = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("rows-undeclared"),
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("admits");
    note_direct_binds(&a, 7);
    assert_eq!(
        undeclared_direct_bind_rows(CompileDoor::Osr),
        before + 7,
        "a door that never declared a policy has its rows counted"
    );

    a.declare_direct_call_policy(DirectCallPolicy::JdkOnly);
    note_direct_binds(&a, 5);
    assert_eq!(
        undeclared_direct_bind_rows(CompileDoor::Osr),
        before + 7,
        "a door that declared is not counted"
    );

    // A compile with no direct calls must not move it either way.
    note_direct_binds(&a, 0);
    let b = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("rows-empty"),
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("admits");
    note_direct_binds(&b, 0);
    assert_eq!(undeclared_direct_bind_rows(CompileDoor::Osr), before + 7);
}

/// Every door that builds a `direct_calls` plan owes the question, and
/// names the ladder that owes it.
///
/// The point is not the current answers — all three are `true` — but that
/// `builds_direct_calls` is an exhaustive `match`, so a fourth door cannot
/// be added without one. This test pins the ladder names beside them so a
/// door whose ladder moves file is a red test rather than a stale doc.
#[test]
fn every_door_answers_the_direct_call_question() {
    for door in CompileDoor::ALL {
        assert!(
            door.builds_direct_calls(),
            "{} builds direct calls today; if that changed, change this test \
             and say why in the record",
            door.label()
        );
        assert!(
            door.direct_call_ladder().contains(".rs"),
            "{} must name the file its ladder lives in",
            door.label()
        );
    }
    let mut ladders: Vec<&str> = CompileDoor::ALL
        .iter()
        .map(|d| d.direct_call_ladder())
        .collect();
    ladders.sort_unstable();
    ladders.dedup();
    assert_eq!(
        ladders.len(),
        CompileDoor::ALL.len(),
        "three doors, three distinct ladders — H12-2 1a's third reason a \
         grep could not produce the bind matrix"
    );
}

/// A door that never asks the String-intrinsic pin is COUNTED, by the
/// token's `Drop`, against that door.
///
/// This is the whole instrument. `fired=0` on the workload the pin governs
/// was read as "correctly inert" and meant "compiled through a door that
/// never asks"; nothing in the tree could tell those apart. Written as an
/// explicit asked/not-asked pair for the reason
/// `the_ungated_witness_fires_only_without_a_token` gives: a counter that
/// never moves and one that always moves both read as a zero from outside.
#[test]
fn a_door_that_never_asks_the_string_pin_is_counted() {
    let _guard = COUNTER_LOCK.lock();
    let before_asked = string_pin_asked(CompileDoor::Osr);
    let before_not = string_pin_not_asked(CompileDoor::Osr);

    // A door that holds an admission and never puts the question.
    drop(
        admit(
            crate::process_jit_verdicts(),
            NO_ID,
            &unique("pin-silent"),
            "m",
            "()V",
            CompileDoor::Osr,
            None,
        )
        .expect("admits"),
    );
    assert_eq!(string_pin_asked(CompileDoor::Osr), before_asked);
    assert_eq!(
        string_pin_not_asked(CompileDoor::Osr),
        before_not + 1,
        "a door that reached the backend without asking the pin must be a number"
    );

    // A door that asks. `invoke_ops` empty is the pin's `NoSite` — no
    // `invokevirtual`/`invokeinterface` could ever have been intrinsified —
    // so this asserts the ASKING, which is the fact the census was missing,
    // without depending on any env var or on a resolved String layout.
    let a = admit(
        crate::process_jit_verdicts(),
        NO_ID,
        &unique("pin-asks"),
        "m",
        "()V",
        CompileDoor::Osr,
        None,
    )
    .expect("admits");
    assert!(!a.string_intrinsic_pin_was_asked());
    assert!(!a.string_intrinsic_pin_declines(&[], None, None));
    assert!(a.string_intrinsic_pin_was_asked());
    assert_eq!(string_pin_asked(CompileDoor::Osr), before_asked + 1);
    // Asking twice must not double the ask count, or the partition
    // `asked + not-asked == admissions` stops holding.
    assert!(!a.string_intrinsic_pin_declines(&[], None, None));
    assert_eq!(string_pin_asked(CompileDoor::Osr), before_asked + 1);
    drop(a);
    assert_eq!(
        string_pin_not_asked(CompileDoor::Osr),
        before_not + 1,
        "a door that DID ask must not also be charged as silent"
    );
}

/// Every door answers whether it asks the pin, and today exactly one does.
///
/// The point is not the answers but that `asks_string_intrinsic_pin` is an
/// exhaustive `match`: a fourth door cannot be added without answering it.
/// Same shape, and the same reason, as
/// `every_door_answers_the_direct_call_question` above — that one was
/// written after a door bound a helper nobody knew it bound; this one after
/// a door compiled a whole population the pin never saw.
#[test]
fn every_door_asks_the_string_intrinsic_pin() {
    let asking: Vec<&str> = CompileDoor::ALL
        .iter()
        .filter(|d| d.asks_string_intrinsic_pin())
        .map(|d| d.label())
        .collect();
    assert_eq!(
        asking.len(),
        CompileDoor::ALL.len(),
        "a door stopped asking the String-intrinsic pin. This test was once \
         `exactly_one_door_asks_the_string_intrinsic_pin_today` and asserted \
         the OPPOSITE, because for a while exactly one door did: the pin \
         lived in `try_compile_inner`, which only `MethodEntry` reaches, and \
         the population that motivated the pin took the OSR door. That cost \
         five refuted hypotheses and was visible only as `fired=0` on the \
         very probe the pin exists for. If you are removing a door from this \
         set, the burden is to say why the counter it stops producing is not \
         the one somebody needs next. Asking: {asking:?}"
    );
}

#[test]
fn every_door_has_a_distinct_slot_and_label() {
    let mut labels: Vec<&str> = CompileDoor::ALL.iter().map(|d| d.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), CompileDoor::ALL.len());
    let mut idx: Vec<usize> = CompileDoor::ALL.iter().map(|d| d.index()).collect();
    idx.sort_unstable();
    assert_eq!(idx, vec![0, 1, 2]);
}
