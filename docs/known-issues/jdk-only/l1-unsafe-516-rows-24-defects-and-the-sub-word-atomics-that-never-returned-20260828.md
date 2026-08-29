# L1 `Unsafe`: 516 rows, 24 defects, and the sub-word atomics that never returned

**Lane L1 of `HANDOFF-20260828-SCOPE.md`** — `jdk/internal/misc/Unsafe` (120
bridge-with-code rows) and `sun/misc/Unsafe` (102). Worktree
`/data/cvm-l1u-20260828`, branch `claude/l1-unsafe-20260828`.

Oracle: HotSpot **25.0.4+7** (Temurin, the Linux build host's `jdk-25`), the
same image CratonVM was pointed at with `--java-home`. Every row was run in
**both** CratonVM modes.

---

## 1. Result

| probe | rows | HotSpot vs CratonVM (both modes) | mode drift |
| --- | ---: | --- | ---: |
| `UnsafeShadowSweep` | 457 | **10 residual rows**, all adjudicated in §4 | 0 |
| `UnsafeNullArgProbe` | 33 | **10 residual rows**, all adjudicated in §4 | 0 |
| `UnsafeSubwordProbe` | 26 | **0 differences** | 0 |
| | **516** | 20 residual rows, 0 unexplained | **0** |

**24 defects found, 24 fixed.** Nothing was left un-repaired that had a
defensible repair; every remaining row has a record in §4 naming the
measurement and the reason.

The lane started at **206 differing lines** on its first complete run.

---

## 2. The one sentence that explains almost every defect

**The retirement surface *is* the JDK's argument-validation layer.**

`jdk.internal.misc.Unsafe.allocateMemory(long)` is bytecode:

```java
public long allocateMemory(long bytes) {
    bytes = alignToHeapWordSize(bytes);   // IAE on overflow
    allocateMemoryChecks(bytes);          // IAE on a negative size
    if (bytes == 0) return 0;             // the zero rule
    long p = allocateMemory0(bytes);      // <- the only native
    if (p == 0) throw new OutOfMemoryError(...);
    return p;
}
```

A native registered for `allocateMemory` replaces **all four steps** and keeps
only the fourth. That is not a special case: `objectFieldOffset`,
`staticFieldOffset`, `staticFieldBase`, `arrayBaseOffset`, `arrayIndexScale`,
`ensureClassInitialized`, `getLoadAverage`, `allocateInstance` and
`invokeCleaner` are all wrapper-plus-`0`-suffixed-native pairs, and CratonVM
registers a native for **both tiers of every one of them**. Nineteen of the 24
defects are a check that lives in the wrapper and nowhere else.

The `0`-suffixed twins are already registered and point at the *same Rust
functions*, so the check has one home and both tiers get it.

### The two spellings do not share one contract

Stage 1 added the record-component refusal to `objectFieldOffset` and turned a
false negative into a false positive, because:

```text
sun.misc.Unsafe.objectFieldOffset(a record component)          UnsupportedOperationException
jdk.internal.misc.Unsafe.objectFieldOffset(the same Field)     answers an offset
```

The same native serves both classes. The receiver (`args[0]`) is the only thing
that says which door the call came through, and three refusals —
record, hidden class, and `getUnsafe`'s `SecurityException` — belong to the
deprecated spelling alone.

---

## 3. The findings

### 3.1 The sub-word atomics did not work at all — and two of them never returned

This is the largest finding of the lane and it is not a contract detail.

`Unsafe.getAndBitwiseOrByte` and `Unsafe.getAndSetByte` **do not return** on
CratonVM. `compareAndSetByte` with the right witness answers **false**.
`compareAndExchangeByte` answers **0** where the field holds 10.

The mechanism: the JDK implements the whole byte/short/char/boolean atomic
family in *bytecode*, by masking the 32-bit word that contains the sub-word:

```java
long wordOffset = offset & ~3;
int  shift      = (int)(offset & 3) << 3;
...  getIntVolatile(o, wordOffset)  ...  weakCompareAndSetInt(o, wordOffset, ...)
```

That arithmetic is only meaningful when `offset` is a **byte offset into an
object**. CratonVM's `objectFieldOffset` returns a **slot index**. So
`offset & ~3` names a *different field*, the masked compare never matches, and
every caller built on it either lies or spins forever.

Measured, one call per process behind a timeout
(`probes/UnsafeSubwordProbe.java`):

| call | HotSpot 25.0.4+7 | CratonVM (before) |
| --- | --- | --- |
| `compareAndSetByte` right witness | `true`, b=11 | **`false`**, b=10 |
| `compareAndExchangeByte` | `10`, b=11 | **`0`**, b=10 |
| `getAndSetByte` | `10`, b=12 | **NEVER RETURNED** |
| `getAndBitwiseOrByte` | `10`, b=15 | **NEVER RETURNED** |
| `getAndAddByte` — *a registered native* | `10`, b=11 | `10`, b=11 |

**The control is what identifies the mechanism.** `getAndAddByte` sits in the
same family, is registered as a native here, and is correct. Its neighbours
differ only in going through the JDK's word-masking bytecode instead of a
native. That rules out "the byte width is broken" and leaves "the offset model
is incompatible with the JDK's own emulation".

**The fix is four registrations, not four implementations.** The bodies are the
existing `int` CAS and compare-exchange unchanged — a byte field's slot holds
`Value::Int`, and the caller widens `B` to `Int` at the call boundary, so the
comparison is already width-correct. What was missing was a registration:

```text
jdk/internal/misc/Unsafe.compareAndSetByte       (Ljava/lang/Object;JBB)Z
jdk/internal/misc/Unsafe.compareAndSetShort      (Ljava/lang/Object;JSS)Z
jdk/internal/misc/Unsafe.compareAndExchangeByte  (Ljava/lang/Object;JBB)B
jdk/internal/misc/Unsafe.compareAndExchangeShort (Ljava/lang/Object;JSS)S
```

Four are enough for the whole family because everything above the CAS layer
delegates rather than re-deriving the offset: `weakCompareAndSetByte*` →
`compareAndSetByte`; `compareAndSetBoolean` → `compareAndSetByte`;
`compareAndSetChar` → `compareAndSetShort`; `getAndSet*` and `getAndBitwise*` →
`weakCompareAndSet*`. **That delegation chain is verified, not assumed** — the
probe asks char, boolean, weak, `getAndSet` and `getAndBitwise` at every width,
on fields and on array elements, and all 26 rows are 0-diff after the four
registrations.

**Consequence for Phase 2.** These four registrations, and the `getAndAddByte`
/ `getAndAddShort` pair that was already there, are **unretirable by
construction**. The bytecode they shadow cannot work on this VM's offset model.
Any retirement pass that reasons only from "the real method has Code" will
propose them; the answer is no, and this is why.

### 3.2 A Java caller could abort the VM

```text
Unsafe.allocateMemory(Long.MAX_VALUE)
  HotSpot 25.0.4+7   IllegalArgumentException
  CratonVM           memory allocation of 9223372036854775807 bytes failed
                     SIGABRT -- the whole VM
```

`ArenaStore::allocate` was `vec![0u8; size]`, which is **infallible**: on
failure Rust runs the allocation-error hook, which aborts the process. A method
whose documented outcomes are "return 0" and "throw `OutOfMemoryError`" must
never be able to take the runtime down from Java.

`try_allocate` / `try_reallocate` now use `Vec::try_reserve_exact`, and the
native maps `None` to `OutOfMemoryError`.

The IAE/OOME boundary was **measured**, not recalled
(`probes/AllocBoundary.java`, 17 sizes):

```text
0                       returns 0
1 .. 2^30               allocated
2^40 .. 2^62-1          OutOfMemoryError
Long.MAX_VALUE - 1, MAX IllegalArgumentException
negative               IllegalArgumentException
```

The IAE at the very top is not a separate check in the JDK:
`alignToHeapWordSize` rounds up to a multiple of 8 *first*, and above
`Long.MAX_VALUE - 7` that wraps negative, so the negative test catches it.

### 3.3 The two off-heap doors named different storage — for float and double only

`Unsafe.getFloat(long)` is literally `getFloat(null, address)` in the JDK, so a
write through one spelling must be visible through the other. It was not:

```text
putFloat(null, addr, 1.5f) then getFloat(addr)    HotSpot 1.5   CratonVM 0.0
putFloat(addr, 2.5f) then getFloat(null, addr)    HotSpot 2.5   CratonVM 0.0
```

The null-base arm read a private **static-field side map keyed by the address**,
which round-trips perfectly *within that door* and shares no storage with the
arena the 1-arg form uses. That is why a round-trip test inside one door reports
the family clean.

int, long, byte, short and char already had `_mb` handlers routing a null base
through `copy_from_native_memory`. **Float and double were the two widths that
never got one** — the same population as
the 2026-08-24 null-base record (`bug-...-null-base`, memory
`a-null-base-unsafe-access-means-off-heap-not-a-static-field`), which fixed the ONE-ARG forms and left the two-arg null-base forms behind.

The probe now asks the aliasing question directly, in both directions, for all
six widths, plus a byte-wise readback of an int write.

### 3.4 A guard placed after the dispatch it covers

`copyMemory(byte[], base, byte[], base, -1)` kept reporting
`IllegalStateException` after the negative-length check was added, because the
check sat *below* the heap↔heap delegation:

```rust
if !src_null && !dst_null { return crate::native_unsafe_copy_memory(ctx, args); }
...                                   // the check was here
```

The heap arm never reached it. A negative length is `IllegalArgumentException`
on every arm, so the check belongs above the dispatch.

The `IllegalStateException` itself came from `bytes as usize` turning `-1` into
2⁶⁴-1, which then tripped the 256 MiB size cap. **The size cap and the argument
check are two different contracts and were sharing one exception.**

### 3.5 `copySwapMemory` off-heap was a silent no-op

```text
copySwapMemory(null, src, null, dst, 8, elemSize=2) over [1..8]
  HotSpot     [2, 1, 4, 3, 6, 5, 8, 7]
  CratonVM    [0, 0, 0, 0, 0, 0, 0, 0]
```

The implementation returned `Ok(None)` for a null base with the comment "an
off-heap operation which we do not support". A silent no-op is the failure mode
that reads as success: the destination was already zero. Now routed through the
arena's bounds-checked `copy_out` / `copy_in`, and an address the arena does not
recognise is refused rather than dereferenced.

### 3.6 The rest

| # | call | HotSpot 25.0.4+7 | CratonVM (before) |
| ---: | --- | --- | --- |
| 1 | `objectFieldOffset(null)` — both spellings | NPE | `0` |
| 2 | `staticFieldOffset(null)` — both | NPE | `0` |
| 3 | `staticFieldBase(null)` — both | NPE | `null` |
| 4 | `staticFieldOffset(an instance field)` — both | IAE | forwarded to `objectFieldOffset` |
| 5 | `staticFieldBase(an instance field)` — both | IAE | the declaring class's mirror |
| 6 | `arrayIndexScale(null)`, `arrayBaseOffset(null)` | NPE | `1` and `16` |
| 7 | `sun` `objectFieldOffset(a record component)` | UOE | an offset |
| 8 | `sun` `objectFieldOffset(a hidden class's field)` | UOE | an offset |
| 9 | `ensureClassInitialized(null)` | NPE | returned quietly |
| 10 | `throwException(null)` | *SIGSEGV* | returned quietly → now NPE |
| 11 | `allocateInstance(null)` | *SIGSEGV* | `null` → now NPE |
| 12 | `allocateInstance(interface / abstract / primitive / void / array)` | `InstantiationException` | allocated something |
| 13 | `getLoadAverage(null, 1)` | NPE | `0` |
| 14 | `getLoadAverage(new double[1], 3)` | AIOOBE | silently clamped |
| 15 | `allocateMemory(0)` | `0` | a live handle |
| 16 | `reallocateMemory(addr, 0)` | frees, returns 0 | a handle |
| 17 | `setMemory` / `copyMemory` with a negative length | IAE | `IllegalStateException` |
| 18 | `invokeCleaner(a slice or duplicate of a direct buffer)` | IAE | accepted |
| 19 | `sun.misc.Unsafe.getUnsafe()` from the app loader | `SecurityException` | handed over `theUnsafe` |

Item 4 is the exact **mirror** of the `objectFieldOffset(a static)` defect fixed
on 2026-08-26: that fix closed one polarity of a two-sided confusion and left
the other open. Item 6 closed an asymmetry where three of four sibling doors
were wrong and the fourth was right *for a reason unrelated to the check* — the
`jdk.internal` spelling of `arrayBaseOffset` shadows nothing on this image, so
the JDK's own bytecode ran and threw.

Items 10 and 11 are not "match the oracle": **HotSpot has no answer** there. It
SIGSEGVs inside `Unsafe_ThrowException` and `Unsafe_AllocateInstance`. Returning
quietly is the one option that is certainly wrong — a caller who wrote
`throwException(x)` expects control not to reach the next line — so both now
throw `NullPointerException`.

---

## 4. Residual rows — what was measured and NOT changed

### 4.1 `objectFieldOffset(Class, String)` on a missing field name (1 row)

```text
HotSpot    InternalError
CratonVM   answers a minted synthetic offset
```

**Deliberate and load-bearing.** The registrar's own history says so: returning
`0` here aliased slot 0 of the receiver and livelocked the caller's CAS loop —
WildFly's `Class$Atomic.casReflectionData` and the Spring Boot
`AbstractClassLoaderValue.putIfAbsent` watchdog hangs. The minted offset routes
through a per-object side store with self-consistent load/CAS/store semantics.

Matching HotSpot's `InternalError` would turn those hangs into hard failures.
Not changed here; it needs the side store's population measured first, which is
a different lane's question.

### 4.2 A reference array's `arrayIndexScale` is 8, not 4 (3 rows)

`Object[]`, `String[]` and `int[][]` all report 8 where HotSpot reports 4.
HotSpot is running with **compressed oops**; CratonVM is not. A scale is an
implementation token: what must hold is that `base + i*scale` is
self-consistent, and it is — the sweep round-trips every width through
`base + i*scale` and every row agrees.

**Legal disagreement, not a defect.** Recorded because a future reader diffing
this family will meet it again.

### 4.3 `arrayIndexScale` / `arrayBaseOffset` on a NON-array (6 rows)

```text
HotSpot    NoClassDefFoundError: java/lang/InvalidClassException
CratonVM   1  and  16
```

Already adjudicated on 2026-08-26 in
[`unsafe-objectfieldoffset-accepted-a-static-and-the-jdk-refusal-that-is-itself-broken-20260826.md`](unsafe-objectfieldoffset-accepted-a-static-and-the-jdk-refusal-that-is-itself-broken-20260826.md).
Read the exception name: `InvalidClassException` lives in **`java.io`**.
HotSpot's deprecation shim names it in the wrong package, so the throw fails to
link and the caller gets a `NoClassDefFoundError`. The intent is a refusal; the
mechanism is a JDK bug, and it is still there in 25.0.4+7.

This lane adds four rows to that record — `arrayIndexScale(int.class)` and
`arrayIndexScale(Iface.class)` answer `1` too, so the catch-all covers
primitives and interfaces as well as ordinary classes.

**The prior adjudication stands.** Returning `0` (what the long-standing
`sun.misc` javadoc specifies, and what callers guard on with
`if (scale == 0) throw`) is defensible and is what I would change it to — but it
is a behaviour change whose blast radius across JCTools-shaped consumers is
unmeasured, the oracle cannot referee it, and a prior session weighed the same
evidence and declined. Changing an adjudicated decision without new evidence is
not a repair.

### 4.4 Where HotSpot dies and CratonVM does not (3 rows)

| case | HotSpot | CratonVM |
| --- | --- | --- |
| `allocateInstance(null)` — both spellings | SIGSEGV | NPE |
| `throwException(null)` | SIGSEGV | NPE |

CratonVM is **better** here and stays that way. These are the rows that forced
the null-argument probe to run one call per process: inside the 457-row sweep
they truncated it, and `diff` reports a missing tail as ordinary `<` lines.

### 4.5 A null base with an address the arena does not know (5 rows)

```text
getInt(null, 12)               HotSpot SIGSEGV     CratonVM 0
putInt(null, 12, 1)            HotSpot SIGSEGV     CratonVM no-throw
compareAndSwapInt(null, 12, 0, 1)  HotSpot SIGSEGV CratonVM TRUE
getObject(null, 12)            HotSpot SIGSEGV     CratonVM null
getReference(null, 12)         HotSpot SIGSEGV     CratonVM null
```

`compareAndSwapInt` returning **`true`** is the row that actively lies: it
claims a CAS succeeded that wrote nowhere a reader can see, which is exactly
what a lock-free algorithm must not be told.

**Mechanism, precisely:** with a null base and an offset that is neither a known
static-field offset nor an arena handle, the natives fall through to a private
`static_int_store` map keyed by the offset, which invents the slot and succeeds.

**Not changed, and this is the lane's judgement call.** That fallback is the
same one that unblocked the `ConcurrentHashMap.initTable` livelock and the
WildFly/Spring Boot lazy-init hangs (§4.1 is its sibling). Refusing an
unrecognised null-base offset would turn every one of those into a hard failure
instead of a slow one, and the set of offsets that legitimately reach it has not
been counted. The lane brief's instruction for exactly this shape — *"a defect
you find here may not be safe to fix in isolation; prefer recording a measured
finding over a speculative repair, and say which you did"* — is why this is a
record.

**What the fix would look like, for whoever takes it:** classify the offset in
the null-base arm — arena-tagged, known static, synthetic, or none of those —
and refuse the fourth case with the `IllegalArgumentException` that
`setMemory`/`copyMemory` at address 0 already produce (rows 27 and 28 of the
null probe, where CratonVM is already correct and HotSpot SIGSEGVs). The
prerequisite is a count of what reaches the fallback on a real workload.

### 4.6 Nine registrations for methods this JDK image does not declare

The sweep's reflective section asks the image directly. On 25.0.4+7:

```text
ABSENT: sun.misc.Unsafe          defineClass, ensureClassInitialized,
                                 shouldBeInitialized
ABSENT: jdk.internal.misc.Unsafe monitorEnter, monitorExit,
                                 defineAnonymousClass, getReferencePlain,
                                 putReferencePlain, weakCompareAndSetObject
```

CratonVM registers a native for all nine. They shadow nothing here and no caller
on this image can name them.

**Not retired.** `WORKER-3-NOTE-2-the-multi-image-method-sweep-says-192-not-342`
found that a substantial fraction of exactly this population is alive on an
older JDK image, and retiring on one image's evidence is how a multi-image
regression gets shipped. Recorded as a nine-row candidate list for whoever runs
the multi-image sweep next.

---

## 5. What PASSED — this is where the work is not

A record that lists only failures tells the next person nothing about where to
stop looking. All of the following were asked at their contract edges and were
already correct, in both modes:

* **every width × every base.** `boolean byte char short int long float double
  Object` against a heap object field, a static field (through
  `staticFieldBase` + `staticFieldOffset`), an array element, and an off-heap
  address — read, write, and read-back, at `MIN_VALUE`, `0xFF`, `NaN` and
  `-0.0` (compared as raw bits, so the two zeros do not compare equal by
  accident).
* **narrow writes do not disturb neighbours.** `putByte`/`putShort`/`putChar`
  into a `Holder` with nine adjacent fields, and into a filled `byte[]`, and
  off-heap into a `0xFF`-filled block read back as a `long`. This is the check
  that catches a `putByte` implemented as a slot-wide store; nothing was.
* **the unaligned family, both endiannesses.** `get/put{Char,Short,Int,Long}Unaligned`
  at offsets 1 and 3 into a `byte[]`, with and without the `bigEndian` flag,
  including `BE == reverseBytes(LE)` and the exact bytes each write left behind.
* **volatile / acquire / release / opaque / plain** at every width, and the
  three fences.
* **the CAS family.** Right and wrong witness on int, long and reference; the
  slot left alone on failure; `compareAndExchange*` returning the witness on
  success and the current value on failure, including the Acquire/Release
  spellings; identity rather than equality (an *equal but distinct* `String`
  witness must not match — it does not); a null witness on a null slot; CAS to
  null; the twelve `weakCompareAndSet*` variants converging; CAS on an array
  element and on a static through `staticFieldBase`.
* **`getAndAdd` / `getAndSet` return the OLD value**, including byte and short
  wrapping in their own width (127+1 → -128).
* **the bitwise family** at int, long, byte, boolean.
* **`copyMemory` in all four directions** — off-heap↔off-heap, heap→off-heap,
  off-heap→heap, heap→heap — plus overlapping copies behaving as `memmove`, and
  `setMemory` on a heap array.
* **a `putObject` into a `String[]` is NOT covariance-checked.** That is the
  documented difference from `aastore`, and a VM that *adds* the check here is
  as wrong as one that drops it from `aastore`. CratonVM stores the `Integer`,
  reads it back as an `Integer`, and so does HotSpot.
* **`allocateInstance` does not run the constructor** and does force
  `<clinit>`; `shouldBeInitialized` flips across `ensureClassInitialized`.
* **`park` / `unpark`** — `unpark(null)`, unpark-then-park returning, a relative
  timeout, an absolute deadline in the past.
* **`invokeCleaner`** on a heap buffer (IAE) and on a fresh direct buffer.
* **the whole reflective section** — which of 45 named methods this image
  declares, and whether each is `native` or bytecode — is byte-identical
  between the two VMs.

---

## 6. Method notes, each learned by getting it wrong

**The oracle crashed first.** `sun.misc.Unsafe.allocateInstance(null)` SIGSEGVs
HotSpot 25.0.4+7 inside `Unsafe_AllocateInstance`. It was row 12 of a 457-row
sweep, and it truncated the HotSpot transcript at 391 lines — every later
section then read as a difference. **Null and out-of-bounds arguments now live
in a separate probe that runs one call per process behind a `timeout`**, so a
crash costs exactly its own row and the exit status is printed: "the VM died",
"the VM never returned" and "the VM threw" are three different transcripts
instead of the same silence.

**HotSpot writes its fatal-error summary to STDOUT.** Inside that per-process
loop it turned 33 rows into 205 and made the diff unreadable. Only lines
matching `^[0-9]+ ` are kept from each row's output.

**A hang is a truncation too.** Adding the byte-width bitwise rows to the sweep
made CratonVM run to the 900-second timeout at row 310, and the run reported the
same shape as the earlier abort. `rc=124` from `timeout` is what tells the two
apart; the runner prints it.

**A nested `p` inside a `t` costs thirty diff lines.** The six non-array
`arrayIndexScale` rows were written as `t(tag, () -> p("  value", ...))`, so the
answering VM emitted two lines per row and the throwing VM one. Six residual
rows became a thirty-line diff in which every later `SECTION ... at N` count was
shifted. Rewritten as `tv(tag, () -> ...)` — **one row, value or throwable** —
the same residual is six lines.

**Check `owns_slot` before editing.** `sun/misc/Unsafe.allocateMemory(J)J` has
**four** registrations across three files and only the one at
`unsafe_natives.rs:1082` owns the slot; `staticFieldBase` is registered twice in
the same file and the `unsafe_natives_ext.rs` copy does *not* win. The four new
sub-word registrations were re-dumped after the build to confirm they own their
slots — and `compareAndSetByte` shows `invocations: 1` on the probe run, which
is what proves the edit was not inert.

**A cross-VM stdout diff must capture the same streams on both sides.** Kept
from the 2026-08-26 record: HotSpot prints four `sun.misc.Unsafe` deprecation
warnings to stderr, and `2>&1` on one arm only invents four differences.

---

## 7. Reproduce

```bash
# host: azureuser@20.80.105.49, worktree /data/cvm-l1u-20260828
bash /data/l1u-probes/l1run.sh all      # sweep + null-arg + sub-word, 3 arms each
```

Probes: `probes/UnsafeShadowSweep.java`, `probes/UnsafeNullArgProbe.java`,
`probes/UnsafeSubwordProbe.java`, `probes/AllocBoundary.java`.

```bash
cratonvm --java-home "$JDK" --jdk-only \
  --add-exports java.base/jdk.internal.misc=ALL-UNNAMED \
  -cp probes/out UnsafeShadowSweep
```

The `--add-exports` is needed on **both** VMs and at `javac` time; the
`jdk.internal.misc.Unsafe` instance is read from `sun.misc.Unsafe`'s
`theInternalUnsafe` field, which `jdk.unsupported` opens to the unnamed module,
so no `--add-opens` is required.

---

## 8. Two vectors this lane cleared that were not its own

The landing merge of `origin/dev` at `d17feaad2` turned **`RExceptions`** and
**`RJdkFailure`** red — in the core arm as well as the strict one, so 72/72
became 71/72.

**Checked against pristine `origin/dev` before blaming the merge.** A detached
worktree at `d17feaad2`, built from scratch, fails both on its own:

```text
pristine origin/dev d17feaad2   ONLY="RExceptions RJdkFailure"
  RExceptions  FAIL     RJdkFailure  FAIL     0 passed, 2 failed
```

Both are one defect, and it is in a change that landed hours earlier:

```text
Class.forName("[Lcom.cratonvm.absent.NoSuchClass20260812;")
  HotSpot 25    ClassNotFoundException msg="com.cratonvm.absent.NoSuchClass20260812"
  dev d17feaad2 ClassNotFoundException msg="[Lcom.cratonvm.absent.NoSuchClass20260812;"
```

`c6ccccbc8` (lane L5) added an array-descriptor branch to
`native_class_for_name` so that `Class.forName("[I")` resolves without
consulting a loader — correct, and it fixed three of its own rows. Its refusal
path passes `&dotted_name`, the **descriptor**, as the exception message.
HotSpot resolves the descriptor down to the element and reports the resolution
that actually failed, which is the only name a caller can act on: `[Lp.X;` is
not something anything can be asked for again.

Both vectors were already in the tree asserting exactly this, each with the
measured JDK 25 behaviour written into its own comment
(`RExceptions.java:371-383`, `RJdkFailure.java:168`). Cleared by naming the
element: strip the leading `[`s, unwrap `L…;`, and fall back to the descriptor
when there is no element name to report (`[I`, or a malformed spelling).

Recorded here rather than as its own page because it is one line and nothing is
left open — but recorded, because a red vector on `dev` blocks every lane, and
the next worker to meet it should not have to re-derive that it is not theirs.

---

## 9. The residuals, measured — 2026-08-29

§4 named four residual categories whose adjudication rested on a missing
measurement. This section is those measurements. Two categories close; two
stay open with a number instead of a hypothesis.

### 9.1 The multi-image census — §4.6 CLOSES, and it reverses three rows

§4.6 listed nine registrations for methods JDK 25 does not declare, and said
the prerequisite for retiring them was the multi-image sweep. That sweep is
run: `probes/UnsafeImageCensus.java` asks a running image whether it declares
each of CratonVM's **212 distinct `Unsafe` registration triples**, and whether
each is `native` or bytecode. Pure reflection — `getDeclaredMethod` is a lookup
and opens nothing — so one `--release 17` build runs on every image with no
flags.

Images: `/data/jdkimages/jdk{17,21,25}-linux`, i.e. **17.0.20.1+1**,
**21.0.12+8**, **25.0.4+7**.

```text
                 absent   native   bytecode
  JDK 17            7       70       133
  JDK 21            7       68       135
  JDK 25           10       68       132
```

Every row that is absent somewhere, or that disagrees across images:

| class | method | 17 | 21 | 25 |
| --- | --- | --- | --- | --- |
| `jdk.internal.misc.Unsafe` | `defineAnonymousClass` | ABSENT | ABSENT | ABSENT |
| `jdk.internal.misc.Unsafe` | `getReferencePlain` | ABSENT | ABSENT | ABSENT |
| `jdk.internal.misc.Unsafe` | `putReferencePlain` | ABSENT | ABSENT | ABSENT |
| `jdk.internal.misc.Unsafe` | `monitorEnter` | ABSENT | ABSENT | ABSENT |
| `jdk.internal.misc.Unsafe` | `monitorExit` | ABSENT | ABSENT | ABSENT |
| `jdk.internal.misc.Unsafe` | `park(Object,long)` | ABSENT | ABSENT | ABSENT |
| `sun.misc.Unsafe` | `defineClass` | ABSENT | ABSENT | ABSENT |
| **`jdk.internal.misc.Unsafe`** | **`weakCompareAndSetObject`** | **bytecode** | **bytecode** | ABSENT |
| **`sun.misc.Unsafe`** | **`ensureClassInitialized`** | **bytecode** | **bytecode** | ABSENT |
| **`sun.misc.Unsafe`** | **`shouldBeInitialized`** | **bytecode** | **bytecode** | ABSENT |
| `jdk.internal.misc.Unsafe` | `loadFence` | native | bytecode | bytecode |
| `jdk.internal.misc.Unsafe` | `storeFence` | native | bytecode | bytecode |

**Three of my nine candidates are alive on 17 and 21.** Retiring
`weakCompareAndSetObject`, `sun` `ensureClassInitialized` and `sun`
`shouldBeInitialized` on JDK 25's evidence would have removed the only
implementation those two images have. That is exactly the failure
[`WORKER-3-NOTE-2`](WORKER-3-NOTE-2-the-multi-image-method-sweep-says-192-not-342-20260821.md)
recorded, reproduced here on the family it warned about.

The other seven are absent from all three supported images. **They are still
not retired in this commit**, and the reason is narrower than before: the
synthetic-JDK mode is a fourth image this census does not cover, and
`classloading/src/class_manager.rs` fabricates these two classes for it. What
it fabricates is only `<clinit>`, so on the evidence a retirement looks safe —
but "looks safe from reading" is not the standard this campaign uses, and the
synthetic-JDK arm was not run. The seven are handed over as a measured,
adjudicated list rather than a guess.

**A by-product worth its own line.** `native-builtins/src/deprecated_verify.rs`
tags `sun/misc/Unsafe.defineClass` as `ImageStatus::Declared`, whose own doc
says *"At least one supported image declares the triple … and MUST stay"*. The
census says ABSENT on 17, 21 and 25. The tag is wrong, and nothing could catch
it: the test asserts the REGISTRATION is present for a `Declared` row and never
checks the claim against an image, so the `Declared` half is unfalsifiable
while the `AbsentFromAllSupportedImages` half is enforced. By that file's own
rule the row should be retagged and its registration removed. Left for whoever
owns T8, with the measurement attached.

### 9.2 `arrayIndexScale` on a non-array — §4.3 CLOSES, and the value changed

§4.3 declined to change the catch-all `1` because the blast radius across
JCTools-shaped consumers was unmeasured. It is measured now, over the whole
117-vector corpus in both modes:

```text
  arrayIndexScale ASKED       161 (compatible) / 175 (strict), in ALL 117 vectors
  ... with a NON-ARRAY class    0                  0
```

The path is thoroughly exercised and the non-array arm is never reached. Blast
radius zero, so the specified answer is free: **a non-array now scales 0**,
which is what the `sun.misc` javadoc says and what makes a caller's
`if (scale == 0) throw` guard fire instead of handing it a plausible basis for
address arithmetic over a class with no elements.

The row still differs from the oracle — HotSpot's refusal is the broken one
recorded on 2026-08-26 — but it now differs by being *correct* rather than by
being a different kind of wrong.

### 9.3 The null-base fallback — §4.5 stays OPEN, with a number

The fix §4.5 specified needs to know what reaches the fallback. Instrumented at
all 21 null-base fallback sites and run over the same corpus:

```text
  null-base fallback REACHED        5 warn-lines, 1 vector (RChmKeySetView)
  ... offset UNCLASSIFIED           0             0 vectors
```

(The warn is rate-limited to the first occurrence and then powers of two, so
five lines is at least sixteen calls in that one process. A zero, by contrast,
is exact: the first occurrence always warns.)

So the fallback is genuinely exercised, and every offset that reached it was
an arena handle, a synthetic offset or a registered static field. The refusal —
an `IllegalArgumentException`, matching what `setMemory`/`copyMemory` already
do at address 0 — would cost nothing on this corpus.

**It is still not taken, and the measurement is what sharpens the reason.**
The corpus does not contain the three definition-of-done workloads, and this
family's sibling fallback is documented as existing precisely for WildFly and
Spring Boot — which are not in it either (§4.1, and `objectFieldOffset1` minted
**0** synthetic offsets across the same 117 vectors, in both modes: its rescue
path is rare, not routine, and the corpus cannot see it). A zero measured on a
corpus that excludes the workloads a path was written for is weak evidence
about that path.

**What would close it:** run the same instrument under L7's three
definition-of-done workloads. The instrument is left in the tree for that
purpose — `note_unsafe_side_store_offset` in `unsafe_natives_ext.rs`, one
relaxed `fetch_add` on a branch every classified access returns before
reaching. If `UNCLASSIFIED-NULL-BASE` is silent there too, the refusal is
licensed and is a four-line change.

### 9.4 Method note: my own instrument lied to me twice

Both worth carrying, because neither is about the VM.

**The instrument was mute, and only the positive control said so.** The
`arrayIndexScale` counter went into the `_` arm of the match — and an early
`if (bytes.first() != Some(&b'['))  { return 1; }` stands in front of it, so
the arm only ever sees a name that starts with `[` and has an unrecognised
second byte, which nothing produces. The first corpus run reported a clean zero
from a counter that could not fire. The control — the sweep, which asks
`arrayIndexScale(String.class)` three times — is what caught it.
`a-cheap-check-in-front-of-an-informative-one-hides-its-zero`, in the
instrument this time rather than in the code under test.

**The denominator's message contained the numerator's name.** The denominator
warn read *"…DENOMINATOR for UNCLASSIFIED-NULL-BASE"*, and the grep that
counted hits matched that string, so every denominator line was also counted as
a hit — reporting 5 unclassified and 175 non-array where the true counts are 0
and 0. The tell was that the two columns were *exactly equal* in both modes.
An instrument that names the thing it is a denominator for cannot be read by
grep.

And a third, from the run before those: a bare `javac src/*.java` over the
corpus gives 58 errors, because the vectors depend on a named module the suite
assembles first. Every counter read zero because no vector ran. **A zero from a
run that did not happen is not a zero** — the count now reuses `run.sh`'s own
build and prints how many vectors it actually executed.

---

## 10. The probes are no longer in the tree — where they went, 2026-08-29

`3b2901531` *"major doc consistency update before the realeas"* (the repo
owner, pre-release) **removed the whole `probes/` directory: 867 files,
110,716 lines.** That took this lane's four probes with it, along with every
other lane's.

This section exists so the next reader does not conclude the measurements were
never made, or go looking for a directory that was deliberately retired.

| probe | what it measures |
| --- | --- |
| `UnsafeShadowSweep.java` | the 457-row differential sweep, both modes |
| `UnsafeNullArgProbe.java` | 33 null / out-of-bounds rows, one call per process |
| `UnsafeSubwordProbe.java` | 26 sub-word atomic rows, behind a timeout |
| `AllocBoundary.java` | the `allocateMemory` IAE/OOME boundary, 17 sizes |
| `UnsafeImageCensus.java` | the multi-image declaration census of §9.1 |
| `unsafe-l1-residual-counts.sh` | the §9.2 / §9.3 counters over the corpus |

They are recoverable from git history — `git show 0d6033a86 -- probes/` for the
first four, `git show 8087d63aa -- probes/` for the last two — and the working
copies are on the Linux build host under `/data/l1u-probes/`.

The instruments that stayed are the ones in the VM itself:
`note_unsafe_side_store_offset` in `native-builtins/src/unsafe_natives_ext.rs`
is still wired to all 21 null-base fallback sites, so §9.3's open question can
be answered by anyone who runs a workload — no probe needed, just stderr.

Re-adding the probe sources was **not** the resolution taken here. The removal
is a deliberate release decision by the repo owner, and a merge that quietly
resurrects three files of a directory somebody just retired is the wrong kind
of conflict resolution.
