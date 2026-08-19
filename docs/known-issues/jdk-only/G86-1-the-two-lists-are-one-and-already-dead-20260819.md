# G86-1 — the "two lists that disagree" are one list, and it is already dead under `--jdk-only`

**Status:** MEASURED. No code changed. The P0 *Duplicate dispatch
implementations* row rests on a premise that was fixed on 2026-08-04 and a
mechanism that strict mode already disables.
**Provenance:** `vm/src/vm/vm_exec.rs` and
`vm/src/runtime/interpreter/native_override.rs` read directly, 2026-08-19;
registry composition from `--dump-native-registry` in both modes.

---

## 0. What the row says, and what the tree says

The row's evidence column describes *"two independent override gates"* that
*"disagree"*, with comments in each *"asking that they be kept in sync by
hand"*, and names `java/util/StringJoiner` as the class one lists and the other
deliberately omits.

The code at the site says that was true and has been fixed:

> This was an inline `matches!` maintained by hand alongside a second copy in
> `real_protected_stub_class`, and the two had drifted: this one listed
> `java/util/StringJoiner`, the other deliberately omitted it — so a
> `SyntheticStub` native's yield-to-real-bytecode verdict depended on how many
> times its call site had executed. **The copies were centralised into one list
> plus one stated exception, and the exception was retired on 2026-08-04** once
> the defect that forced it was measured not to reproduce. **Both paths now
> call the one predicate.**
>
> Do NOT re-inline a copy here. The divergence this replaced is exactly what
> the contract §7 centralisation exists to prevent.

One predicate, `real_protected_stub_class`, twelve entries, each with its own
rationale — several carrying `JDK-ONLY-WAVE2` markers and dated working notes.
The row's "hand-maintained, kept in sync by hand" characterisation no longer
describes it.

## 1. And under `--jdk-only` the list cannot fire at all

The guard is:

```rust
let synthetic_stub_native = native_kind == NativeKind::SyntheticStub;
let real_protected_stub = synthetic_stub_native
    && real_protected_stub_class(effective_class);
```

`SyntheticStub` is not `allowed_in(JdkOnly)`, so strict mode registers none —
measured, `--dump-native-registry` under `--jdk-only`: **0 synthetic-stub of
10710 registrations** (`G83-1` §2). `synthetic_stub_native` is therefore never
true, `real_protected_stub` never true, and the allow-list never consulted.

The code predicts exactly this, three lines further on:

> What must ultimately replace the list: `NativeKind` alone — under
> `--jdk-only` a `SyntheticStub` never dispatches, so no class needs
> "protecting" from one and **the whole allow-list becomes dead**.

It is already dead there. What keeps the list alive is `--real-jdk`, the
DEFAULT mode, where the 1330 synthetic stubs do register and do dispatch.

## 2. What this does and does not mean

**It does not close the row.** The row is wider than the two lists:

* the `bytecode_available: false` in `resolve_step1_native` that made the
  `java/lang/String` copies inert is still open, by that row's own text;
* the JIT is a THIRD location — seven "thin direct call" ladders in
  `jit/src/lib.rs` baking VM-side reimplementations into emitted code, plus
  five JIT-reachable paths that bypass `resolve_dispatch` and
  `record_invocation` entirely. Nothing here touches those, and the strict
  report's `jit_direct_native_binds: 0` only says the probe run did not hit
  them;
* the required resolution is a single policy-aware resolver used by
  interpreter, JIT, reflection, JNI and method handles, with a lint forbidding
  direct registry lookup outside it. That does not exist.

**It does mean the row's stated evidence needs re-deriving before anyone plans
against it** — the same finding as `G79-1` (counts), `G83-1` (a red ratchet and
a `157` that matches nothing), and `Function.identity()` described as `Bridge`
when the dump says `synthetic-stub`. Four rows now, all stale in the same
direction: the tree moved and the row did not.

## 3. NOMINATIONS

**N1 — delete the allow-list under `JdkOnly` explicitly, rather than relying on
it being unreachable.** §1 shows it cannot fire in strict mode. Making that
structural — the predicate returning `false` under `JdkOnly` by construction,
or the call sites not reaching it — turns an emergent property into a stated
one, and would let the row's remaining work be about `--real-jdk` only, which
is a much smaller question.

**N2 — MEASURED (`G87-1`).** The JIT third location under `--jdk-only`: 0
direct native binds, 0 inline-cache natives, and 38 by-name fast-path
admissions REFUSED — measured on `RJitStringLayout`, a run with 1.75M intrinsic
invocations, so the JIT was thoroughly exercised. Same shape as the allow-list:
the mechanism is live in `--real-jdk` and neutralised in strict. The row's
remaining content is therefore the DEFAULT mode plus the missing single
resolver, not the strict path.

**N3 — re-derive every P0 row's evidence before planning against it.** This is
the fourth stale row found by reading the tree instead of the table. A row that
cites a fixed defect is worse than a row that cites nothing, because it directs
effort at work already done.
