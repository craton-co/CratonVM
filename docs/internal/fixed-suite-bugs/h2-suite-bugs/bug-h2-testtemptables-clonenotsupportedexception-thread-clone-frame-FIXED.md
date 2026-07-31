# `TestTempTables` `CloneNotSupportedException` via a `java.lang.Thread.clone` frame — array receivers dispatched through their COMPONENT class

## Status
**FIXED** (2026-07-31, branch `fix/h2-temptables-clone-frame-20260731`).

The original report's central claim — that the two innermost frames "don't
form a plausible real call chain" and are therefore evidence of a
stack-trace-construction bug — is **wrong**. Both frames are genuine JDK 25
code, and the trace reads exactly as a real call chain. What is broken is
**method dispatch on array receivers**: CratonVM stores an array's
*component* class id in the object header, and three dispatch paths in the
interpreter consulted that class id without first checking that the receiver
is an array. That routes `someArray.m()` onto the **component class's**
method body — and when the component class is `java.lang.Thread`, whose
`clone()` unconditionally throws, the result is precisely this report's
`CloneNotSupportedException` raised from a `java.lang.Thread.clone` frame.

## Correcting the original analysis

The report asserted that `java.lang.Thread` "neither implements `Cloneable`
nor declares its own `clone()` override", and that `java.util.Arrays.copyOf`
"does not call `java.lang.Thread.clone`". Checked against the JDK 25 on the
test host (`$JAVA_HOME/lib/src.zip`, `javap`):

* **`Thread.java:1037` is real, and it is the exact line in the trace.**

  ```java
  // java.base/java/lang/Thread.java:1028-1038
  /**
   * Throws CloneNotSupportedException as a Thread can not be meaningfully cloned.
   */
  @Override
  protected Object clone() throws CloneNotSupportedException {
      throw new CloneNotSupportedException();      // <-- line 1037
  }
  ```

  `javap -c java.lang.Thread` confirms the whole body is
  `new CloneNotSupportedException / dup / invokespecial / athrow`.

* **`Arrays.java:3617` is real, and it is an array `clone()` call site.**

  ```java
  // java.base/java/util/Arrays.java:3615-3624
  public static long[] copyOf(long[] original, int newLength) {
      if (newLength == original.length) {
          return original.clone();                 // <-- line 3617
      }
      ...
  ```

  ```
  public static long[] copyOf(long[], int);
       0: iload_1
       1: aload_0
       2: arraylength
       3: if_icmpne     14
       6: aload_0
       7: invokevirtual #319   // Method "[J".clone:()Ljava/lang/Object;
      10: checkcast     #320   // class "[J"
      13: areturn
  ```

* The H2 frames above it are equally genuine:
  `VersionedBitSet.java:25` is `bits = BitSetHelper.flip(other.bits, bitToFlip);`
  and `BitSetHelper.java:34` is
  `bits = Arrays.copyOf(bits, Math.max(length, wordIndex) + 1);`.

* **Frame order.** CratonVM's own exception printer emits frames
  **outermost-first** (H2's `printStackTrace` output in the same suite is
  innermost-first, and the `org/h2/...` slash-separated names in the report
  are CratonVM's internal form). So `java/lang/Thread.clone` is the
  *innermost* frame, invoked from `Arrays.copyOf` at pc 7 — i.e. the
  `original.clone()` on a `long[]`-typed receiver dispatched into
  `Thread.clone()`.

* **No Rust code throws `CloneNotSupportedException`.** `grep -r
  CloneNotSupported --include=*.rs` finds only three *registration* lists
  (`class_manager.rs`, `lang_misc.rs`, `lib.rs`) — no construction site.
  The exception can therefore only have come from real bytecode, and
  `Thread.clone` is the only JDK method on that stack whose body throws it.

So the trace is not fabricated, not stale, and not misattributed: an
`invokevirtual …clone()` with an array receiver really did enter
`java.lang.Thread.clone()`.

## Root cause

`Anewarray` stores the **component** class id in the array's object header
(`vm/src/runtime/interpreter.rs`, `Instruction::Anewarray`), and `Newarray`
stores `ClassId::new(0)` — which resolves to `java/lang/Object`. So

```
class_id_of(Foo[])   == class_id_of(Foo)
class_id_of(long[])  == ClassId(0) == class_id of java/lang/Object
```

Per JVMS §4.4.1 an array type's method table comes from `java.lang.Object`,
so **every** dispatch path that keys on `heap.class_id_of(receiver)` must
first check `heap.kind_of(receiver) == ObjectKind::Array` and route through
`Object`.

Three paths already did:

* `execute_invokevirtual_vtable_fast` (`invoke.rs`) — explicit `kind_of ==
  Array → CacheMiss`;
* the JIT's MIC/PIC helper (`jit/helpers.rs`, `receiver_is_plain_object`,
  and `virtual_dispatch_target_cached`'s "KC26" short-circuit);
* the machine-code inline caches (`jit/src/x64.rs`, the
  `CMP BYTE [recv + OBJECT_KIND_OFFSET], Object` guard).

Three did **not**:

1. **`execute_invokevirtual_cached` — the interpreter's inline-cache *hit*
   path** (`vm/src/runtime/interpreter/invoke.rs`). Its
   `VirtualBytecode`, `VirtualNative` and `Intrinsic` arms validated a cached
   entry with a bare `actual_class_id != receiver_class_id` comparison. The
   *population* side skips arrays (`receiver_class_id = None`), but the *hit*
   side did not, so a `Foo[]` receiver matched an entry installed for a plain
   `Foo` and ran `Foo`'s body with the array as `this`. **This is the live,
   reproducible defect** (see "Reproduction" below).
2. **`invoke_or_native`'s NoSuchMethod receiver-class-chain rescue**
   (`vm/src/vm/vm_exec.rs`): when the CP dispatch class collapsed to
   `Object`, it walked `class_id_of(receiver)`'s superclass chain for a
   native or bytecode body — the component class's chain, for an array.
3. **`invoke_on_class_shared_inner`'s virtual retarget**
   (`vm/src/vm/vm_exec.rs`): retargets dispatch onto `class_id_of(args[0])`
   when the nominal class is an interface/abstract — again the component
   class for an array receiver (reachable e.g. via a reflective invoke of a
   `Cloneable`/`Serializable`-typed method on an array).

## Reproduction (of the mechanism)

`ArrayReceiverDispatchProbe` (embedded in the regression test) warms one
`invokevirtual java/lang/Object.toString()` / `.hashCode()` / `.equals()`
call site on a plain `Foo`, then hands the same site a `Foo[]`:

| | `Foo[].toString()` | `Foo[].hashCode()` |
|---|---|---|
| HotSpot jdk-25 | `[LFoo;@7ad041f3` | identity hash |
| CratonVM (before) | `Foo-toString-v0` | `0x5eed0000` (Foo's override) |
| CratonVM (after)  | `[LFoo;@52` | identity hash |

`Foo-toString-v0` is `Foo.toString()` reading array *element 0* as *field 0*.
Reproduces identically with `--nojit` and with the JIT on, in seconds.

The H2 report's shape is the `clone()` instance of the same thing: an array
receiver whose header class id names a class with a bytecode `clone()` body.
`java.lang.Thread` is the worst possible such class, because its `clone()`
does nothing but `throw new CloneNotSupportedException()` — exactly the
observed failure. (Producing that specific frame additionally requires the
`long[]` in `VersionedBitSet.bits` to carry `Thread`'s class id in its
header; with the guards below, an array receiver can no longer select *any*
component-class body regardless of what its header says.)

## Fix

`vm/src/runtime/interpreter/invoke.rs` — `execute_invokevirtual_cached`:
add, in each of the three receiver-validating arms, immediately after the
GC-forwarding refresh and *before* `class_id_of`:

```rust
if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
    return Ok(CachedCallResult::CacheMiss);
}
```

`vm/src/vm/vm_exec.rs`:
* the receiver-class-chain rescue skips array receivers (`recv_is_array`);
* `invoke_on_class_shared_inner`'s retarget yields `None` for array
  receivers instead of their component class id.

All three cede to the slow path, which already routes array receivers
through `java/lang/Object` and hence to `native_object_clone`'s
`ObjectKind::Array` branch for `clone()`.

Cost: one byte load from the object header that is already in cache (the
same header the very next line reads `class_id` from). Array-receiver sites
never populated the inline cache in the first place, so the only newly-missing
hits are sites genuinely shared between array and non-array receivers.

## Regression test

`vm/tests/array_receiver_dispatch.rs` —
`array_receiver_dispatches_through_object_not_component_class`. Compiles an
embedded probe on the fly, runs it under `--nojit` and with the JIT, and
asserts `toString`/`hashCode`/`equals` on `Foo[]`, `int[]` and the reverse
(array first, then `Foo`) plus `long[].clone()` via `Arrays.copyOf` and
`Foo[].clone()`. Differential-verified: **FAILS** on the pre-fix binary
(`got 'Foo-toString-v0'`), **PASSES** on the fixed one.

## Verification

* `org.h2.test.db.TestTempTables` — **7/7 clean runs** on the *pre-fix*
  binary at `origin/dev` @ `a31a8a93f` (1 + 3 `--nojit` + 3 JIT-on, each a
  full `testFromMain` including `testLotsOfTables`' 100 000 create/drop
  cycles), all with a diagnostic armed to dump any bytecode `clone()` frame
  entered with an array receiver, and any `java/lang/Thread.clone` frame at
  all. **Neither fired, and the reported exception did not recur** — the
  original report's own "reproduced once … not yet confirmed deterministic"
  holds; the H2-level symptom is rare, the underlying dispatch defect is
  100 % deterministic and is what the fix removes.
* `org.h2.test.synth.TestMultiThreaded` (the report's second, unconfirmed
  data point) — run 4× (2 `--nojit`, 2 JIT-on) with the same diagnostic; no
  `CloneNotSupportedException`, no diagnostic hit. That occurrence remains
  unattributed; the class is randomized and its failures vary run to run.
* `cargo test --release -p cratonvm-vm --no-fail-fast` on the fixed tree:
  138 test binaries, 3 failing targets —
  `class_loader_unload_regression` (2 tests),
  `threadpoolexecutor_prestart_regression` (1, `javac` unavailable on the
  build host), `wp4_6_chm_basic` (2). All three were re-run on the *pristine*
  tree (same worktree, both source files reverted, same binary rebuild) and
  fail identically, test-for-test: **pre-existing, not introduced here.**
* `org.h2.test.db.TestTempTables` re-run on the fixed binary
  (`--nojit` and JIT-on): clean.

## Original report

The superseded write-up is preserved in git history at
`docs/known-issues/h2/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame.md`
(added 2026-07-31, `c67c7dbea`).

## Related

* `vm/src/jit/helpers.rs` — the "KC26 `array.clone()` bug" comment records
  the JIT-side instance of this same family (`Enum.clone() →
  CloneNotSupportedException` for enum-array clones), and the
  `ResolvableType[]` / `ResolvableType` Spring Boot `ClassCastException`
  behind the machine-code `OBJECT_KIND_OFFSET` guard. This doc is the
  interpreter half of that family.
* `vm/src/runtime/interpreter/invoke.rs`'s `try_stackless_invoke` "T15"
  comment (array class names rewritten to `java/lang/Object`).

## Possible residual observed 2026-07-31, after this fix landed

Seen while quantifying a separate H2 throughput problem
(`docs/known-issues/h2/bug-h2-testmultithread-concurrent-insert-throughput-timeout.md`)
on a binary built from `dev` @ `4a48f12cb6`, i.e. one that **contains** this
fix: a 25-thread × 1000-row JDBC insert/commit probe had **all 25 threads** fail
with

```
org.h2.jdbc.JdbcSQLNonTransientException: General error:
  "java.lang.CloneNotSupportedException"; SQL statement:
COMMIT [50000-249]
```

i.e. the same `CloneNotSupportedException`, on the same H2 `COMMIT` →
`TransactionStore` → `VersionedBitSet` path this doc covers, ~348 s into the run.

It is **intermittent and thread-count-dependent**, which is why it did not show
up in this doc's own verification:

* 25 threads × 1000 rows — all 25 threads failed.
* 25 threads × 200 rows, same binary, same probe — `failed=0`.
* `org.h2.test.db.TestMultiThread` itself (25 threads × 1000 rows) on the same
  binary — no `CloneNotSupportedException` at all; it failed on H2's own
  5-minute future timeout instead.
* `org.h2.test.db.TestCompatibility` on the same binary — passes (`rc=0`) both
  JIT-on and `--nojit`, where before this fix it failed with exactly this
  exception under JIT-on.

So the fix is clearly effective for the deterministic cases; something in the
same dispatch path still slips through when many threads are active. No stack
was captured (the probe only recorded `toString()` on the failing run, and the
re-run with stack printing did not reproduce). Worth a targeted rerun with
`printStackTrace` at 25 × 1000 before assuming it is the same mechanism.

Repro used:
`docs/internal/repros/h2-insert-scale-20260731/H2InsertScaleProbe.java`, invoked
as `H2InsertScaleProbe <abs-dir> 25 1000`.
