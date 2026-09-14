# W8-C15-1 — the atomic ARRAY family had no bounds check at all, and a 224-line fixture said so was fine

**Status: FIXED in `native-builtins/src/util_concurrent_ext.rs` and
`native-builtins/src/phases_early.rs` (lane C15, 2026-08-12). Unbuilt and
unrun by this lane — every "after" below is marked PREDICTED. One
NOMINATION, against `regression-suite/src/RAtomicArray.java`, which this lane
does not own.**

## 1. What was measured

`AtomicReferenceArray<String>` over `{"a","b","c"}`, `AtomicIntegerArray` over
`{11,22,33}`, `AtomicLongArray` over `{111,222,333}` — HotSpot 25.0.3+9
(Microsoft OpenJDK, `scratchpad/c15/P1Bounds.java`, full transcript below) vs a
CratonVM binary of this wave:

| call | HotSpot | CratonVM (before) |
|---|---|---|
| `ARA.get(-1)` / `get(-5)` / `get(3)` / `get(100)` | `ArrayIndexOutOfBoundsException` each | `null` each |
| `AIA.get(-1)` / `get(3)` | `ArrayIndexOutOfBoundsException` | `0` |
| `ARA.set(-1,"x")` | `ArrayIndexOutOfBoundsException` | silent no-op |
| `AIA.compareAndSet(-1,0,9)` | `ArrayIndexOutOfBoundsException` | returned **true**, wrote nothing |
| `new AtomicIntegerArray(-1)` | `NegativeArraySizeException: -1` | `usize::MAX`-sized allocation request |

HotSpot's message is `Index -1 out of bounds for length 3` for every one of the
36 (accessor × index) pairs in the probe, on all three classes, and it covers
the ordering-mode aliases (`getPlain`, `getAcquire`, `setRelease`,
`weakCompareAndSetPlain`) and the functional forms (`updateAndGet`,
`getAndUpdate`, `accumulateAndGet`, `compareAndExchange`) identically.

**This is a missing-exception defect, not a memory-safety defect, and the
distinction is load-bearing.** The orchestrator's sentinel-neighbour sweep over
indices `-32..63` returned no nonzero value from any out-of-range read on
either VM, and the code says why: `VmHeap::get_array_element` returns
`Err(index)` past the end and `VmHeap::set_array_element` writes nothing, so
the suppression is real and happens below the native. What `vm_exec.rs`'s
`NativeContext` impl does is DISCARD those results — and its own doc comment
says so, and says whose job the check is:

> `get_array_element` ends in `.unwrap_or(...)` and `set_array_element` in
> `let _ = ...`. … The caller range-checks; `native-builtins`'s
> `vh_array_index` is the worked example.
> — `vm/src/vm/vm_exec.rs:11249-11265`

Every atomic-array native was a caller that never did.

It is still serious, and the write half is the worse half. A read that answers
`null`/`0` for an off-by-one produces a `NullPointerException` somewhere else,
later, with a stack that does not name the index. A **write** that silently
does nothing produces no exception at all — the datum is simply absent, and the
program is wrong at an arbitrary distance from the defect. The sharpest form is
`compareAndSet`, which returned **`true`** out of range: it read `null`,
compared `null == null`, "stored", and reported success. That is the exact
idiom H2's `TestFileSystem.testConcurrent` uses as a spin lock, and the reason
these natives were rewritten once already (see the 2026-07-27 comment above
`atomic_array_rmw`).

## 2. Cause

`let idx = match args.get(1) { Some(Value::Int(v)) => *v as usize, _ => 0 };`,
repeated ~30 times. Two things are wrong in one line:

1. `as usize` erases the sign. `-1` becomes `18446744073709551615`, and the
   only thing between that and a wild read is the heap's own
   `index >= array_length` test — the one whose verdict is then thrown away.
2. There is no length comparison anywhere, for any index, in any of the three
   classes.

`AtomicReferenceArray` is registered in `phases_early.rs`
(`register_atomic_reference_array_natives`, 9 triples);
`AtomicIntegerArray` and `AtomicLongArray` in `util_concurrent_ext.rs`
(26 triples each). `--dump-native-registry` confirms all 61 rows are
`owns_slot=true, overwrote=None` — no shadowing, these bodies are the answer.

## 3. Fix

A funnel, not per-accessor checks, for the reason `vh_array_index`'s doc gives
for the identical decision on the VarHandle side: with 26 registered triples per
class, a check added to twenty-five of them is a silent hole in the
twenty-sixth.

* `util_concurrent_ext.rs` — new `atomic_array_index(ctx, arr, idx: i32)`
  returning `Err(RuntimeError::aioobe(idx, len))`, which produces HotSpot's
  exact text; `atomic_array_raw_index(args)` which reads the index as `i32` and
  does **not** widen it; `atomic_array_new_length(len: i32)` for the
  constructor's `NegativeArraySizeException`; and `atomic_array_slot(ctx,args)`
  which resolves `(backing array, checked index)` in one call.
  `ala_target` is now a thin alias for it, so its five callers gained the check
  without five edits.
* Every `native_aia_*` / `native_ala_*` body now goes through
  `atomic_array_slot`. The "no receiver / no backing array" arms keep their
  historical typed defaults; only the out-of-range case is new.
* `phases_early.rs` — `ara_slot`, the same helper against the by-NAME `array`
  field, used by `get` / `set` / `lazySet` / `getAndSet` / `compareAndSet` /
  `weakCompareAndSet` / `weakCompareAndSetPlain`, plus the constructor check.

`AtomicReferenceArray`'s `updateAndGet` / `getAndUpdate` / `accumulateAndGet` /
`compareAndExchange` are **not** registered; in real-JDK mode they run the real
bytecode, which calls the registered `get` first — so they inherit the throw
rather than needing their own. That is why the fix is 7 registrations wide and
the contract is 13 methods wide.

PREDICTED after: `--only=bounds` green; `RAtomicArray` still green (it never
indexed out of range, which is the point of §5).

## 4. NOMINATION N1 — `regression-suite/src/RAtomicArray.java`

Not owned by this lane. `RAtomicArray` is 224 lines, PASSES on the binary where
`ARA.get(-1)` returns `null`, and contains **zero** negative indices, **zero**
`catch` blocks and no `IndexOutOfBounds` expectation. It covers `length()`,
`compareAndSet`, `getAndIncrement` and `get`/`set` round trips under contention
— thoroughly — and its bounds contract not at all.

The extension is written, compiled and run on HotSpot 25.0.3+9 at
`scratchpad/c15/RAtomicArray.java`: it adds 2 044 checks (`CK bounds 2264`,
`PASS RAtomicArray (2264 checks)`) covering 33 accessors × 6 bad indices ×
3 classes, the three negative-length constructors, an in-range control so the
method cannot pass by making everything throw, and a neighbour-integrity check
after all the out-of-range WRITES.

Mutation-checked as the standing lesson requires — one op changed from
`ara.get(i)` to `ara.get(0)`:

```
Exception in thread "main" java.lang.AssertionError:
  RAtomicArray: ARA.get(-1) did not throw ArrayIndexOutOfBoundsException
rc=1
```

**Edit 1**, exact:

old
```java
        referenceArrayCas();
```

new
```java
        referenceArrayCas();
        boundsContract();
```

**Edit 2**: insert the `Op` interface, `aioobe(...)` helper and
`boundsContract()` method verbatim from `scratchpad/c15/RAtomicArray.java`
(lines from `// ---- 5. the BOUNDS contract` to the end of `boundsContract()`)
immediately before the final closing brace of the class. The file compiles and
runs green as delivered; it uses only the existing `check(boolean, String)` and
`checks` members.

## 5. The lesson, which is about how this VM gets tested

`RAtomicArray` was written by someone thinking about what the class DOES.
`RJdkIntrinsics2`'s `bounds` family was generated from the NATIVE REGISTRY —
it drives triples because they are registered, not because anyone remembered
they had a contract. That difference is the whole finding:

> **Registry-derived coverage finds the contracts nobody remembered.
> Hand-written coverage finds the behaviour everybody remembered.**

Both fixtures are about `AtomicReferenceArray`. One is 224 lines of careful
concurrency work and is silent on the defect; the other found it in a family it
enumerated mechanically. A green hand-written fixture over a class is evidence
about the half of the class its author had in mind, and this project keeps
reading it as evidence about the class. (`[gate=FR]`, `[reach≠defect]`, and now
this.)

The same shape appeared twice more in this lane's own inbox, both in
`phases_early.rs` and both now retired: `String.codePointAt` implemented as
`chars().nth(idx)` (a code-POINT index for a code-UNIT argument) and
`String.chars()` yielding code points. Right for every wholly-BMP string,
silently wrong for anything else, with nothing in the type system to notice —
and, because `register()` is last-write-wins and these ran LATER than the
correct `lang_string.rs` bodies, they were the live answer in synthetic-jdk
mode while `--dump-native-registry` showed the good body owning the slot in
real-JDK mode. Two dumps agreeing that the right body wins is not proof it wins
everywhere; the dumps were both `mode=compatible`.

## 6. Transcript — HotSpot 25.0.3+9, `scratchpad/c15/P1Bounds.java`

```text
ARA.get(-1)                 -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.set(-1)                 -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.lazySet(-1)             -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.getAndSet(-1)           -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.compareAndSet(-1)       -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.weakCompareAndSet(-1)   -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.updateAndGet(-1)        -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.getAndUpdate(-1)        -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.accumulateAndGet(-1)    -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.compareAndExchange(-1)  -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.getPlain(-1)            -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.getAcquire(-1)          -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
ARA.setRelease(-1)          -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 3
AIA.get(-1) ... AIA.accumulateAndGet(-1)   — same exception, same wording
ALA.get(-1) ... ALA.compareAndSet(-1)      — same exception, same wording
   ... and identically for -5, 3, 100, Integer.MIN_VALUE, Integer.MAX_VALUE ...
new ARA(-1)                 -> java.lang.NegativeArraySizeException: -1
new AIA(-1)                 -> java.lang.NegativeArraySizeException: -1
new ALA(-1)                 -> java.lang.NegativeArraySizeException: -1
new ARA((Object[])null)     -> java.lang.NullPointerException: Cannot read the array length because "array" is null
new AIA((int[])null)        -> java.lang.NullPointerException: Cannot invoke "[I.clone()" because "array" is null
neighbours intact: abc 123 123
```

The last line is the memory-safety control: after every out-of-range write in
the sweep, all nine in-range elements still hold their birth values on HotSpot,
and the same assertion is now in the fixture for CratonVM.

## 7. Residual

* The array-copy constructors `AtomicIntegerArray(int[])`,
  `AtomicLongArray(long[])`, `AtomicReferenceArray(E[])` are not registered and
  their `null` contract (`NullPointerException`, two different messages —
  see §6) is unmeasured against CratonVM.
* `AtomicReferenceArray`'s ordering-mode aliases (`getPlain`, `getAcquire`,
  `setRelease`, `setOpaque`, `compareAndExchange*`,
  `weakCompareAndSetAcquire/Release`) are deliberately left unregistered.
  In real-JDK mode they reach the VarHandle path, where `vh_array_index`
  already throws; in synthetic-jdk mode they have no body at all. Registering
  them is a separate decision with its own risk, not a bounds fix.
* Nothing in the tree tests any of these natives under contention **at a bad
  index**. `RAtomicArray`'s concurrency half and its new bounds half do not
  intersect.
