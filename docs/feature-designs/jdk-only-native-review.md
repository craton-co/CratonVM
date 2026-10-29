# Native Method Classification and Promotion Review

## Purpose and Status

**Active standard for classifying and promoting native registrations.**

In CratonVM, authentic OpenJDK class libraries are authoritative. Every native method registered with the virtual machine must have an explicit, justified classification. The native method registry categories are:

- **`Bridge`**: Connects Java declarations marked `ACC_NATIVE` to VM internals or OS system calls (e.g., thread management, file I/O, process handling, `Unsafe`).
- **`Intrinsic`**: Replaces a method that has concrete Java bytecode with an optimized native routine that is provably equivalent and measurably faster.
- **`SyntheticStub`**: Temporary or compatible-mode-only shims that simulate Java behavior without running authentic class libraries. Prohibited under foundational JDK-only execution.

> **Registry Scope Principle**: Because `NativeMethodRegistry::register` adopts an ambient category that defaults to `SyntheticStub`, registrations made outside an explicit `with_category` or `set_category` scope default to stubs by omission. Native classification reviews ensure every native method is intentionally classified.

---

## The Ordered Review Questions

When reviewing or promoting a native method, answer each question in sequence:

### 1. Does the real class and method exist in the target OpenJDK runtime image?

Verify using `javap` across supported JDK versions (JDK 17, 21, 25):

```bash
"$JAVA_HOME/bin/javap" -p -c 'java.lang.System' | grep -n 'identityHashCode'
```

- **No**: The method exists only in a synthetic or compatibility layer. Disposition: **CompatibilityShim** (prohibited in JDK-only mode).
- **Yes**: Proceed to Step 2.

### 2. Is the real method marked `ACC_NATIVE`?

```bash
"$JAVA_HOME/bin/javap" -p 'java.lang.System' | grep 'arraycopy'   # flags: ACC_PUBLIC, ACC_STATIC, ACC_NATIVE
```

- **Yes**: A native implementation is required because no bytecode exists. Proceed to Step 4 to evaluate completeness as a **Bridge**.
- **No**: OpenJDK provides concrete bytecode. Proceed to Step 3.

### 3. Does it contain concrete bytecode?

Inspect the method using `javap -c`.
- **Yes**: Real bytecode is the authoritative baseline. An intrinsic replacement is admitted only if equivalence is strictly proven and performance gains are measured. Proceed to Step 4.
- **No (and not `ACC_NATIVE`)**: The method is abstract or an un-defaulted interface method. Disposition: `MissingImplementation`.

### 4. Is the native callback functionally complete?

A native implementation must achieve complete observational parity across all five dimensions:

| Dimension | Verification Requirement |
|---|---|
| **Exceptions** | Matches the exact exception class, exception message (where specified), and check ordering (e.g., null checks before index bounds checks). |
| **Synchronization** | If the bytecode method is `synchronized` or locks on an object internally, the native callback must acquire identical monitor locks. |
| **Memory Visibility** | Preserves Java Memory Model (JMM) guarantees: volatile loads/stores, atomic operations, and release barriers for final fields. |
| **Side Effects** | Updates all relevant internal state, triggers listeners/cleaners, and maintains allocation observability. Omitting internal field updates leaves objects in an inconsistent state for subsequent bytecode. |
| **Layout Independence** | Must **not** assume hard-coded field offset indices or synthetic field layouts. All field access must use canonical symbolic names or registered VM offsets. |

### 5. Is it required by JVM specification or an OS boundary?

Evaluates genuine bridge requirements: threading, raw memory access, file descriptors, network sockets, time sources, security crypto seeds, or class definition.
- **Yes**: Disposition is **Bridge**. Proceed to Step 7.
- **No**: Proceed to Step 6.

### 6. Is it demonstrably faster?

- **Yes, with proven parity and reproducible benchmarks**: Disposition is **Intrinsic**.
- **Unmeasured or parity unproven**: Remove native from the strict path and execute authentic OpenJDK bytecode.

### 7. Is the native reached in test suites and benchmarks?

Audit invocation metrics via `--dump-native-registry` across regression and differential suites. A registration with zero invocations across all workloads is a candidate for dead code elimination.

### 8. What breaks if the native is removed?

Test removal against strict differential and regression suites:

```bash
cargo test -p cratonvm-native-builtins --lib
cargo test -p cratonvm-vm --test synthetic_diff
cratonvm --jdk-only -cp app.jar com.example.Main
```

---

## Dispositions

| Disposition | `NativeKind` | JDK-Only Mode | Compatible Mode |
|---|---|---|---|
| **Bridge** | `Bridge` | Registered and invoked | Registered and invoked |
| **Intrinsic** | `Intrinsic` | Registered; supersedes bytecode | Registered; supersedes bytecode |
| **Bytecode Fallback** | — | Real bytecode executed | Registered as fast stub (if beneficial) |
| **CompatibilityShim** | `SyntheticStub` | Refused at registration | Registered in compatible mode |
| **Dead Registration** | — | Deleted completely | Deleted completely |
| **Generated Artifact** | — | Converted to runtime generated class (`ClassOrigin::Generated*`) | Converted to runtime generated class |

---

## Promotion Checklist

Each pull request modifying or promoting native methods should satisfy:

- [ ] Declaring class is resolved from authentic OpenJDK `jimage` runtime modules.
- [ ] Real method `ACC_NATIVE` access flag and bytecode presence are confirmed via `javap`.
- [ ] Callback is verified as a complete VM/OS `Bridge` or a measured, parity-proven `Intrinsic`.
- [ ] Exception types, messages, and validation order match OpenJDK HotSpot.
- [ ] Synchronization, monitors, and JMM memory ordering are preserved.
- [ ] No object fields are accessed via synthetic slot indices or assumed layouts.
- [ ] Tested across target JDK versions (JDK 17, 21, 25).
- [ ] JIT compiler and interpreter dispatch resolve to the identical implementation.
- [ ] Compatible mode behavior remains correct and non-regressed.
