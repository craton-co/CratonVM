# JDK-only mode — native promotion review

| | |
|---|---|
| **Status** | Active. This is the gate every `SyntheticStub` must pass to survive into strict mode. |
| **Normative source** | [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md) §1, §4, §7 |
| **Input** | `target/jdk-only-audit/stub-review.tsv`, produced by `scripts/jdk-only-census.sh` |
| **Related** | [`synthetic-vs-real-explained.md`](synthetic-vs-real-explained.md) (the existing change policy) · [`jdk-only-runtime-services.md`](known-issues/jdk-only/runtime-services-blocker-inventory.md) |

Reviewing a stub means answering one question: **why does Rust code run here
instead of the JDK's own bytecode, and is that reason still true?**

Three answers are legitimate:

- **Bridge** — the behaviour cannot be expressed as ordinary Java bytecode in
  this VM. An OS syscall, a VM metadata operation, a class-definition service.
  The real method is `ACC_NATIVE`.
- **Intrinsic** — the real method has concrete bytecode, and our replacement is
  observationally equivalent *and* measurably faster. Equivalence is proven, not
  asserted.
- **Neither** — delete it from the strict path and run the real bytecode.

A fourth answer, "it is a compatibility shim", is a valid *classification* and an
invalid *destination*: shims are forbidden under `JdkOnly` by contract §1.

> **Start from the premise that the tag is wrong.** The registry's category is
> ambient — `NativeMethodRegistry::set_category` / `with_category` in
> `native-api/src/registry.rs` — and it **defaults to `SyntheticStub`**. A
> `register()` call outside a `with_category(...)` block is tagged a stub by
> omission, not by judgement. `vm/src/vm/vm_init.rs` records the consequence:
> a global drop of all `SyntheticStub` natives was once reverted the
> same day because JMX and `Function$Identity` are permanent bridges wearing the
> wrong tag. Promotion review exists to find those, not only to delete fakes.

---

## The ordered questions

Answer in order. Record every answer in the PR — a reviewer must be able to
check your reasoning without re-deriving it.

### 1. Does the real class and method exist in the selected boot image?

```bash
"$JAVA_HOME/bin/javap" -p -c 'java.util.function.Function' | grep -n 'identity'
```

Also check across the declared matrix (17 / 21 / 25), because a method can be
present in one image and absent in another.

- **No** → the registration is standing in for something that does not exist.
  It is a **CompatibilityShim**. Strict disposition: refuse, with a
  specification-consistent error. Skip to §"Disposition".
- **Yes** → continue.

### 2. Is the real method `ACC_NATIVE`?

```bash
"$JAVA_HOME/bin/javap" -p 'java.lang.System' | grep 'arraycopy'   # -> native
```

- **Yes** → this is the **only** shape in which a native is *required*. Continue
  to question 4; a bridge is the expected outcome.
- **No** → the JDK ships bytecode for this. Our native is either an intrinsic or
  redundant. Continue.

### 3. Does it have concrete bytecode?

`javap -c` shows a `Code:` attribute. An abstract or interface method without a
default has none.

- **Yes** → real bytecode is the baseline. Anything we register must beat it on
  *proven* equivalence, not on convenience.
- **No, and not `ACC_NATIVE`** → nothing implements it. Strict disposition is
  `MissingImplementation`, not a stub.

### 4. Is the callback complete?

This is where most promotions fail. "It returns the right value for the happy
path" is not completeness. Check all four:

| Dimension | What to verify |
|---|---|
| **Exceptions** | Same exception *class* for every documented failure. Same message where the message is specified or stable. Same ordering when several checks could fire (e.g. null check before bounds check). Differential-test the throwing paths, not just the returning ones. |
| **Synchronization** | If the real method is `synchronized`, or acquires a lock internally, the callback must acquire the same monitor. A native that skips the monitor is a correctness bug that only shows up under contention. |
| **Memory visibility** | Java Memory Model effects must survive: volatile reads/writes, `final` field freeze semantics, the happens-before edges the real implementation publishes. A plain Rust store where the JDK does a release store is a bug you will not reproduce on x86. |
| **Side effects** | Field mutations, cached-value population, listener/observer notifications, allocation observable via GC, `Cleaner` registration, resource ownership. A native that returns the right value but skips a field write leaves the object in a state the *next* real bytecode call will read. |

**Layout is a fifth, repository-specific dimension.** If the callback reaches
object fields by assumed slot index — the `synthetic_stub_fields` /
`AnonymousObject` family in `classloading/src/class_manager.rs` — it is
assuming a fabricated layout. The moment the real class loads instead, those
indices address the wrong words. Convert to named-field lookup or a VM side
table **before** promoting, not after.

### 5. Is it required by the JVM specification or an OS boundary?

Genuine bridge territory: threading primitives, file descriptors, process
control, time sources, `Unsafe`, class definition, JNI entry points, memory
mapping, entropy.

- **Yes** → bridge. Continue to 7.
- **No** → it is not required. Continue to 6.

### 6. Is it merely faster?

Be honest here. "Merely faster" is a legitimate reason to keep a native — as an
**intrinsic** — but only with the parity proof from question 4 *and* a
measurement. Without a measurement it is not an intrinsic, it is an untested
reimplementation of JDK behaviour.

- **Yes, and parity is proven, and the speedup is measured** → intrinsic.
- **Yes, but unmeasured** → delete from the strict path. Re-propose with numbers.
- **No** → delete from the strict path.

### 7. Does any real-mode test invoke it?

```bash
# invocations comes from the schema-v2 census; see jdk-only-audit.md §3.4.
jq -r '.natives[] | select(.class=="java/lang/System" and .name=="arraycopy")
       | {kind, invocations, registered_by}' \
  target/jdk-only-audit/registry-real.json
```

Run the census over the regression suite and the differential corpus, not just
`HelloWorld`. `invocations: 0` across all of them, with no test naming it, is a
**dead registration** — the cheapest possible deletion, and the one most likely
to be sitting in the 157.

### 8. What breaks when it is removed?

Do not answer from reading. Remove it and run:

```bash
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
cargo test -p cratonvm-vm --test synthetic_diff -- --nocapture
CV="$PWD/target/release/cratonvm" JDK="$JAVA_HOME" bash regression-suite/run.sh
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
  gate --jdk "$JAVA_HOME" --corpus difftest/seeds
```

A removal that "looks safe" and a removal that *is* safe are different claims.
That reverted global drop is the standing counter-example.

---

## Disposition

```text
ACC_NATIVE + complete VM/OS implementation      -> Bridge
Concrete bytecode + proven equivalent + measured -> Intrinsic
Concrete bytecode + incomplete replacement       -> Delete from the strict path
No real method/class + compatibility behaviour   -> CompatibilityShim (refuse under JdkOnly)
Never invoked, no test names it                  -> Delete
Stands in for a runtime-generated artifact       -> Move to the generated-class service
```

| Disposition | `NativeKind` | Strict mode | Compatible mode |
|---|---|---|---|
| **Bridge** | `Bridge` | registered and invoked | unchanged |
| **Intrinsic** | `Intrinsic` | registered; may beat bytecode | unchanged |
| **Delete from strict path** | — | not registered; real bytecode runs | unchanged (still registered) |
| **CompatibilityShim** | `SyntheticStub` | refused at registration; violation recorded with its `#[track_caller]` site | unchanged |
| **Dead registration** | — | deleted outright | deleted outright; lower the ratchet baseline in the same change |
| **Generated artifact** | — | not a native; becomes a `ClassOrigin::Generated*` class | same |

"Move to the generated-class service" covers the `Function$Identity` shape: a
named stand-in class whose real counterpart is a lambda. The fix is a real
generated implementation with a legitimate origin, not a better stub.

---

## Checklist

Copy this into the PR body and tick it.

```text
[ ] The real declaring class is loaded from the JDK image (all declared feature versions).
[ ] The real method's access flags and bytecode presence were recorded, not assumed.
[ ] The implementation is a genuine VM/OS bridge or a measured, proven intrinsic.
[ ] Exception type, message and ordering match HotSpot on every failure path.
[ ] Synchronization and Java Memory Model effects are preserved.
[ ] No object field is accessed by assumed synthetic slot index.
[ ] Tested on every declared JDK/platform combination.
[ ] JIT and interpreter reach the same dispatch decision (one resolver, not two gates).
[ ] Strict registry count and strict invocation count for SyntheticStub are zero for this subsystem.
[ ] Compatible-mode behaviour is unchanged, or the migration is documented.
[ ] The ratchet baseline was lowered in this same change if a stub was removed.
```

---

## PR discipline

**One coherent subsystem per PR.** This is not a style preference — it is the
mitigation for that failure mode, where a global change made the
regression unattributable and the only recovery was a full revert.

Every classification PR includes:

- before/after registry fragments (`--dump-native-registry`, diffed);
- invocation counts from at least one program that actually exercises the
  subsystem;
- HotSpot differential results;
- a strict-mode test;
- a compatible-mode regression test proving nothing moved;
- an updated ratchet count in `native-builtins/tests/stub_ratchet.rs`.

**Do not raise the ratchet baseline to make a build green.** The baseline is
committed slack-free precisely so that a new application-visible stub fails CI.
If a new stub is genuinely unavoidable, say so in the PR body and expect the
question. Removing a stub requires lowering the baseline in the same change.

**Do not add a `SyntheticStub` when a bridge, intrinsic or real bytecode path
can implement the contract.** That rule predates this document — see
[`synthetic-vs-real-explained.md`](synthetic-vs-real-explained.md) — and JDK-only
mode does not relax it.
