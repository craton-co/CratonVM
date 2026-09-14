# `ClassLoader.defineClass0` has two registrations, and the init contract they disagree about — 2026-09-12

Two bodies serve one triple. They disagree about whether an initialization
failure is reported at all, and **which one is live depends on the arm**, not on
line order. One of them swallowed the failure; that is fixed here. A third
deviation on the same surface is measured below and deliberately left open.

Landed on `claude/l2-defineclass0-dedup-20260912`.

## 1. Who owns the slot, per arm — measured, not argued

`register` is last-write-wins and updates the slot in place, so ownership is
decided by which registrar runs last:

| arm | owner (`registered_by`) | body | init failure |
|---|---|---|---|
| real-JDK / `--jdk-only` / compatible | `native-builtins/src/lib.rs:17601` | `lang_system::native_classloader_define_class0` | reported |
| synthetic-JDK | `native-builtins/src/classloader.rs:4749` | `cl_define_class0` -> `define_class_via_full` | **swallowed** |

Printed by the new
`classloader_define_class0_owner_is_decided_by_the_arm` in
`native-builtins/tests/duplicate_registration_gate.rs`, in both feature arms.

**Line order gives the wrong answer here, which is why this needed measuring.**
`lib.rs`'s registration sits ~8k lines ABOVE the
`classloader::register_classloader_natives` call that displaces it, so reading
the file suggests the classloader copy always wins. It does not: that call is
inside `register_synthetic_overrides`, which is `#[cfg(feature =
"synthetic-jdk")]` and which `vm_init` invokes only when `use_synthetic_jdk` is
true at runtime. `register_classloader_natives` has **no call site anywhere in
`vm/src`**.

Note what `registered_by` names. `register` is `#[track_caller]`, so provenance
is the REGISTRATION SITE, not the file holding the body — the real-JDK owner
reports `lib.rs` while its body lives in `lang_system.rs`.

## 2. Why `duplicate_registration_gate.rs` did not catch it

It already knew. Blind spot #2 in that file's own header names this exact pair:

> `classloader::register_classloader_natives` is never called here, and it holds
> live shadows: … `ClassLoader.defineClass0/1/2` over the copies in
> `register_essential_natives_with_shims` … That is the exact defect this gate
> exists to catch, and it is invisible to it.

The scope is correct and the gap is real: the gate replays `vm_init`'s real-JDK
arm, and in that arm the second registrar genuinely never runs, so there is no
shadowed row to count. What was missing is that the OTHER arm inverts the answer.
The new test asserts both, so the prose is now a measurement.

## 3. The fix

`define_class_via_full` did this:

```rust
if initialize {
    if let Err(msg) = ctx.initialize_class(cid) {
        tracing::warn!("defineClass0 initialize: <clinit> for {name} failed: {msg}");
    }
}
```

`initialize = true` is a contract (JVMS 5.5, and HotSpot's
`JVM_LookupDefineClass` links and initializes before returning). This warned and
returned the mirror as though `<clinit>` had succeeded, so the caller held an
UNINITIALIZED class and the real failure surfaced later with no visible
connection to the define. It now propagates `MethodCallFailed` unchanged, which
carries the exception the initializer actually raised.

`NativeContext::initialize_class`'s own doc argues for exactly this: it returns
`MethodCallFailed` rather than a `String` specifically so that "ordinary
`<clinit>` exceptions" are not collapsed into unrecoverable internal errors. The
swallow discarded that distinction one layer up.

### Why option 3 of the three on the table, and not 1 or 2

The task that produced this work offered: (1) delete the dead registration,
(2) make `cl_define_class0` delegate to the live body, or (3) fix the swallow so
the shadowed path cannot become live later with the wrong contract.

**1 and 2 rest on a false premise.** The registration is not dead — §1 shows it
is the synthetic-JDK arm's winner. Deleting it would take that arm's cglib SEGV
guard, its `classData` side-table write and its defining-loader registration with
it; delegating would rewrite that arm's semantics wholesale (different argument
mapping, different guards). Option 3 is the only one that is not a behaviour
change to a working arm.

And "cannot become live LATER" understates it:

## 4. The swallow was reachable on the real-image path too

The task recorded that `define_class_via_full`'s other callers pass
`initialize = false`, making the swallow unreachable outside the synthetic arm.
That is true of `defineClass1`, `defineClass2`, the `Unsafe.defineClass` shim and
`shared_secrets_bridge.rs:950` — and **false** of
`shared_secrets_bridge.rs:1039`, `jla_define_class_hidden`, which computes

```rust
let initialize = matches!(args.get(6), Some(Value::Int(v)) if *v != 0);
```

from its caller's argument and is registered on the JavaLangAccess carrier
bridge, which is live on a real image. So a hidden-class define through
SharedSecrets asking for initialization could silently receive an uninitialized
class in every mode, not only under `--features synthetic-jdk`.

This also makes the corpus result load-bearing rather than decorative: the three
arms below exercise that path in real-JDK mode and do not move.

## 5. Still wrong, measured, and NOT fixed here

`probes/L2InitFail.java` (added by this change) builds a class whose `<clinit>`
throws with the `java.lang.classfile` API and asks
`Lookup.defineHiddenClass(bytes, initialize)` both ways. JDK 25.0.4+7:

| arm | `initialize=false` | `initialize=true` |
|---|---|---|
| HotSpot | defines, no throw | `ExceptionInInitializerError`, cause `RuntimeException` |
| CratonVM compatible | defines, no throw | `IllegalStateException`, cause `<none>` |
| CratonVM `--jdk-only` | defines, no throw | `IllegalStateException`, cause `<none>` |

So the failure IS reported on this route — it goes through `lookup_define.rs`,
not through §3's line — but with the wrong type and with the original exception
dropped. A caller that catches `ExceptionInInitializerError`, or reads
`getCause()`, gets neither. The message carries the truth as text
(`"…: java/lang/ExceptionInInitializerError"`) and nothing can act on it.

Left open deliberately: three sites branch on that message's TEXT —
`lookup_define.rs:636`, `lookup_define.rs:895`, `classloader.rs:10066` — so
changing the type is its own change with its own blast radius, and it belongs
with the sibling deviation in `lang_system.rs`, which wraps an init failure in
`ClassFormatError` where HotSpot propagates the `Error`. Both are the same bug
wearing two hats: the backend reports init failures as a STRING, and every
consumer re-derives an exception from it.

## 6. Verification

Gate set, `docs/contributing/jdk-only-lane-operations.md` §5, on the merged tree:
`cratonvm-types` 15 targets; `native-builtins` default / management /
synthetic-jdk 11 targets each; `cratonvm-vm --lib` 2673 passed;
`cratonvm-native-api --lib` 440 passed / 0 failed.

Corpus on the release build: `--jdk-only` 136/136, `SUITE=all` 136/136,
`SUITE=core` 95/95.

Six failures in that run are dev's, each naming a file this change does not
touch: `architecture_per_crate_loc_table_matches_reality` (the stale row is
`jit`, 5.11% over a 5% tolerance — `native-builtins` is at 1.52%),
`flag_inventory_surface_counts_are_current` (1,419 claimed vs 1,420 actual — one
new `CRATONVM_*` flag), `a_record_does_not_claim_a_fix_it_never_ran`
(`docs/known-issues/tomcat/tls-handshake-enforcement-gaps-20260912.md`),
`every_relocatable_doc_citation_points_at_the_page` (`classloading/src/class.rs:520`
citing a moved springboot page),
`unsafe_natives_ext::unsafe_unaligned_read_tests::the_byte_array_bulk_route_agrees_with_the_per_element_loop`
(lane 5's file, modified the same day), and
`ffm_group_layout_force_native_covers_member_layouts` (lane 4's, a force-native
gate entry). Attribution is by content — each names a foreign file — not by a
pristine-dev re-run.
