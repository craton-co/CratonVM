# Native Promotion Review — Criteria and Evaluation Gate

| | |
|---|---|
| **Status** | Active. This is the evaluation gate every native must pass to run under real-JDK mode (`--real-jdk`). |
| **Normative source** | [`real-jdk-mode.md`](real-jdk-mode.md) §1, §4, §7 |
| **Input** | `target/jdk-only-audit/stub-review.tsv`, produced by `scripts/jdk-only-census.sh` |
| **Related** | [`native-dispatch-architecture.md`](native-dispatch-architecture.md) |

Reviewing a registered native means answering one foundational question: **why does Rust code run here instead of the JDK's own bytecode, and is that reason still valid?**

Three answers are legitimate:

- **Bridge** — the behaviour cannot be expressed as ordinary Java bytecode in this VM (e.g. an OS syscall, a VM metadata operation, memory mapping, or class-definition service). The real method is `ACC_NATIVE`.
- **Intrinsic** — the real method has concrete bytecode, and the native replacement is observationally equivalent *and* measurably faster. Equivalence is proven, not asserted.
- **Neither** — remove it from the strict path and execute the real JDK bytecode.

A fourth classification, **CompatibilityShim**, represents compatibility shims that stand in for absent classes or methods; shims are strictly forbidden under `JdkOnly` by contract §1.

> **Ambient category premise:** The native method registry's category is ambient — `NativeMethodRegistry::set_category` / `with_category` in `native-api/src/registry.rs` — and defaults to `SyntheticStub`. A `register()` call outside an explicit category scope is tagged a stub by omission rather than intent. Promotion review audits and classifies each native explicitly.

---

## The Ordered Questions

Answer these questions in order. Record every answer in the PR so reviewers can verify the reasoning.

### 1. Does the real class and method exist in the selected boot image?

```bash
"$JAVA_HOME/bin/javap" -p -c 'java.util.function.Function' | grep -n 'identity'
```

Verify across the supported JDK feature matrix (17 / 21 / 25), as methods and classes may vary across JDK versions.

- **No** → The registration stands in for something that does not exist in the JDK image. It is a **CompatibilityShim**. In Real-JDK mode, it must be refused with a specification-consistent error (e.g. `ClassNotFoundException` / `NoSuchMethodError`).
- **Yes** → Continue to Question 2.

### 2. Is the real method `ACC_NATIVE`?

```bash
"$JAVA_HOME/bin/javap" -p 'java.lang.System' | grep 'arraycopy'   # -> native
```

- **Yes** → A native implementation is required. Continue to Question 4; candidate for **Bridge**.
- **No** → The JDK ships concrete bytecode for this method. The native is either an intrinsic or redundant. Continue to Question 3.

### 3. Does it have concrete bytecode?

`javap -c` inspects the `Code:` attribute. Abstract or interface methods without defaults have none.

- **Yes** → Real bytecode is the authoritative baseline. Any replacement native must beat it on proven equivalence and performance.
- **No, and not `ACC_NATIVE`** → Nothing implements it. Strict disposition is `MissingImplementation`, not a synthetic stub.

### 4. Is the callback complete?

A callback must not simply return the expected value on the happy path. Verify all key dimensions:

| Dimension | What to verify |
|---|---|
| **Exceptions** | Match the exact exception class for every failure condition documented or produced by HotSpot. Match exception messages where specified. Preserve check ordering (e.g. null check before index bounds check). Differential-test throwing paths. |
| **Synchronization** | If the real method is `synchronized` or acquires an internal monitor, the callback must acquire the identical monitor. Omitting synchronization produces concurrency defects under multi-threaded contention. |
| **Memory visibility** | Java Memory Model effects must be preserved: volatile reads/writes, `final` field freeze semantics, and happens-before edges. A plain relaxed Rust store where the JDK requires release semantics breaks memory visibility. |
| **Side effects** | Field mutations, cached-value updates, listener notifications, GC-observable allocations, `Cleaner` registrations, and resource tracking must be honoured. Incomplete mutations leave objects inconsistent for subsequent bytecode execution. |
| **Field layout** | Never access object fields by assumed numerical slot indices. Use named field lookup or VM side tables so layout changes in real classes do not corrupt memory. |

### 5. Is it required by the JVM specification or an OS boundary?

Genuine bridge territory includes: threading primitives, file descriptors, network sockets, process control, system time sources, `Unsafe` memory access, dynamic class definition, JNI entry points, memory mapping, and entropy sources.

- **Yes** → **Bridge**. Continue to Question 7.
- **No** → Not required as a bridge. Continue to Question 6.

### 6. Is it measurably faster?

A native replacement for concrete bytecode is legitimate only as an **Intrinsic**, requiring both parity proof from Question 4 and reproducible benchmark measurements. Without benchmark data, it is an unverified re-implementation.

- **Yes, parity is proven, and speedup is measured** → **Intrinsic**.
- **Yes, but unmeasured** → Remove from the strict path. Re-propose with benchmarks.
- **No** → Remove from the strict path; let authentic JDK bytecode run.

### 7. Does any real-mode test invoke it?

```bash
jq -r '.natives[] | select(.class=="java/lang/System" and .name=="arraycopy")
       | {kind, invocations, registered_by}' \
  target/jdk-only-audit/registry-real.json
```

Run the census over regression and differential suites. If `invocations: 0` across all workloads and tests, it is a **dead registration** and should be removed.

### 8. What breaks when it is removed?

Test removal comprehensively:

```bash
cargo test -p cratonvm-native-builtins --lib -- --nocapture
cargo test -p cratonvm-vm --test synthetic_diff -- --nocapture
CV="$PWD/target/release/cratonvm" JDK="$JAVA_HOME" bash regression-suite/run.sh
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
  gate --jdk "$JAVA_HOME" --corpus difftest/seeds
```

---

## Disposition Matrix

```text
ACC_NATIVE + complete VM/OS implementation        -> Bridge
Concrete bytecode + proven equivalent + measured  -> Intrinsic
Concrete bytecode + incomplete replacement         -> Delete from strict path
No real method/class + compatibility behaviour     -> CompatibilityShim (refuse under JdkOnly)
Never invoked, unreferenced in tests              -> Delete
Stands in for a runtime-generated artifact         -> Implement in generated-class service
```

| Disposition | `NativeKind` | Strict Mode (`--real-jdk`) | Compatible Mode (`--compatible`) |
|---|---|---|---|
| **Bridge** | `Bridge` | Registered and invoked | Registered and invoked |
| **Intrinsic** | `Intrinsic` | Registered; may override bytecode | Registered; may override bytecode |
| **Delete from strict path** | — | Not registered; real bytecode runs | Unchanged (registered in compatible mode) |
| **CompatibilityShim** | `SyntheticStub` | Refused at registration; violation logged | Unchanged (registered) |
| **Dead registration** | — | Deleted outright | Deleted outright |
| **Generated artifact** | — | Becomes `ClassOrigin::Generated*` | Becomes `ClassOrigin::Generated*` |

---

## Promotion Checklist

Each promotion PR must verify:

- [ ] The real declaring class is loaded from the JDK image (across supported JDK versions).
- [ ] Real method access flags (`ACC_NATIVE`) and bytecode presence are verified.
- [ ] Implementation is a genuine VM/OS bridge or a measured, proven intrinsic.
- [ ] Exception types, messages, and ordering match HotSpot on all failure paths.
- [ ] Synchronization monitors and Java Memory Model visibility effects are preserved.
- [ ] No object field is accessed by hard-coded synthetic slot index.
- [ ] Differential tests against HotSpot pass.
- [ ] JIT and interpreter share identical dispatch decisions via `resolve_dispatch`.
- [ ] Strict registry count and invocation count for `SyntheticStub` remain zero.
- [ ] Compatible mode behavior remains intact or changes are documented.
- [ ] Ratchet baselines are updated when stubs are retired.

---

## Review Discipline

- **One coherent subsystem per PR:** Keep promotions focused to make regressions readily attributable and auditable.
- **Include census and diff:** Provide before/after `--dump-native-registry` fragments, invocation counts, and differential test results.
- **Never raise ratchet baselines to mask stubs:** Baselines ensure stubs do not silently accumulate. Removing a stub requires lowering the baseline in the same change.
- **Prefer real bytecode:** Do not add a `SyntheticStub` when an authentic bytecode path, bridge, or intrinsic can fulfill the contract.
