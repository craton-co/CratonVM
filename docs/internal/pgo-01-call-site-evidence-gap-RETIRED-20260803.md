# PGO-01 — the inlining policy had no per-call-site evidence to read — IMPLEMENTED

**Status: first increment shipped 2026-08-03.** `MethodProfile::call_sites`
now has real callers: every `invokestatic`/`invokespecial` the interpreter
dispatches records through the existing `record_call_site_borrowed` API.
Verified end-to-end with a real interpreted run (not just the profile
store's own unit tests, which never exercised the interpreter and so could
not have caught this gap in the first place), plus the full 924-test
`interpreter_tests.rs` suite and the jit crate's 83 profile/PGO tests, both
zero regressions. Original finding preserved below for context.

## What shipped

* **Every interpreter dispatch point for `invokestatic`/`invokespecial` now
  records call-site evidence**, in `vm/src/runtime/interpreter/invoke.rs`:
  * `execute_invokestatic` and `execute_invokestatic_cached` — every
    `invokestatic`, cached-fast-path and slow-path.
  * `execute_invoke_kind` and `execute_invokevirtual_cached`, both gated on
    `is_special` — every `invokespecial`. These two functions are ALSO the
    dispatch path for `invokevirtual`/`invokeinterface`, which are
    deliberately NOT recorded here (they already have coverage via the
    `receivers` map; recording both would double-count one call site
    against two evidence sources).
  * Full census: 9 call sites in `vm/src/runtime/interpreter.rs` (the
    raw-byte fast loop and the decoded slow-path fallback each have their
    own copy of the invokevirtual/invokespecial/invokestatic/invokeinterface
    dispatch) plus 1 internal delegation in `invoke.rs`, all needed a
    `pc: usize` parameter threaded through — `execute_invokevirtual_cached`
    already had one (`site_pc`), which is why receivers recording never
    needed this step.
* **Recording placement**: once per function, right after resolution has
  succeeded and every early "give up, no dispatch happened"
  return/eviction/cache-miss path has already been passed — not scattered
  across every one of the many downstream successful-dispatch branches
  (native, JIT, bytecode-push, lambda proxy...) each of these functions has.
  Explicitly a heuristic hotness signal for the inliner, not a correctness
  input (see the doc's own "state the concurrency contract explicitly"
  ask) — the placement can over-count by one in a narrow redefinition-
  eviction race, which is an acceptable trade against hunting down every
  branch in a ~500-2000 line function and risking missing one (the
  undercounting failure mode the doc explicitly warned about).
* **Test**: `vm/tests/pgo01_call_site_evidence.rs` +
  `vm/tests/resources/cratonvm/PgoCallSiteEvidence.java`. Two positive
  controls (a static-call loop, a constructor-call loop — see below for why
  constructor calls, not a private-method call) asserting
  `call_sites` is populated with the exact loop count, and two negative
  controls: a virtual-only method (`call_sites` must stay empty — that
  evidence lives in `receivers` only) and the same static-call shape with
  profiling disabled (`call_sites` must stay empty — the gate must actually
  gate). All four assertions run in one `#[test]` fn to avoid the
  process-global `enable_profiling` flag racing across parallel test
  threads in the same binary.
* **Premise correction found while building the test**: a same-class
  **private instance method** call does **not** compile to `invokespecial`
  on this host's javac — confirmed with `javap`, it's `invokevirtual`, a
  JDK 11+ (JEP 181, nestmate access control) behavior change. The fixture's
  invokespecial positive control uses `new Foo()`'s `<init>` call instead
  (unconditionally invokespecial per JVMS, unaffected by the nestmate
  change). Left as a documented trap in the fixture's own doc comment for
  whoever next needs a guaranteed invokespecial site in a Java fixture on
  this JDK.
* `MethodProfile`'s own doc comment (`jit/src/profile.rs`) updated to name
  the actual recording sites instead of describing `call_sites` as
  hypothetically fed "whatever the interpreter calls it from."

## What did not ship

* **Speculation on this evidence is explicitly out of scope** — the doc's
  own "what to refuse" section. That is `pgo-02`, gated on a guard/deopt
  pairing that does not exist yet. This lane only records data; nothing
  reads `call_sites` for a compilation decision yet (the inliner in
  `jit/src/lib.rs` — `plan_inline`, `classify_receiver_shape` — was not
  touched).
* Coverage is scoped to the doc's named files
  (`vm/src/runtime/interpreter/invoke.rs`, `vm/src/runtime/interpreter.rs`,
  `jit/src/profile.rs`) — the interpreter's *own* dispatch. Any
  invokestatic/invokespecial-shaped call that happens inside
  JIT-compiled code (a completely different mechanism — direct/helper
  calls emitted by the backend, not interpreter dispatch) is untouched, by
  the doc's own file-ownership boundary, not an oversight.

## The original finding (2026-07, still accurate as history)

`MethodProfile::record_call_site` and `record_call_site_borrowed`
(`jit/src/profile.rs`) had **zero callers anywhere in `vm/` or `jit/`**,
verified by grep, twice, in two different waves — confirmed still true at
the start of this session by the same grep. Consequences that were all true
before this shipped:

* `CallSiteEvidence::Direct` was unreachable outside tests.
* `MethodProfile::call_sites` was always empty in a real run.
* There was no per-call-site hotness evidence for `invokestatic` or
  `invokespecial` at all — the receiver-type profile covers virtual and
  interface sites; the static and special sites had nothing.

So the inlining policy in `jit/src/lib.rs` was reading a data source that a
real run never populated for a whole class of call sites. That policy is
tested and correct; it was starved. It still is, for `invokestatic`/
`invokespecial` specifically, until `pgo-02` teaches the inliner to read
`call_sites` — this lane only makes the data exist.

### What had already landed (2026-08-01), so this lane didn't redo it

The profile's *integrity* was fixed separately: a `u32` overflow at ~43M
observations at one call site that panicked in debug builds inside a JIT
profile read, a hash-order tie-break that let two compiles of one profile
seed different inline caches, a non-saturating counter that wrapped to zero
and turned the majority receiver into the rarest, and two branch predicates
with the same overflow. That work also disproved a report premise worth
remembering: a monomorphic-looking site has **not** overflowed its type
slots, because the live receiver table is uncapped — don't design around a
truncation that only exists in the unwired `pgo.rs` copy.
