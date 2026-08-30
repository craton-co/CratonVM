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

## 10. WITHDRAWN — the probes came back, and this section was stale within hours

**Do not read the rest of this section as current.** It said the probes
were gone from the tree and told the next reader to recover them from git
history. `probes/` was RESTORED on `dev` the same day, and L7 committed
its own eleven probes back with a commit that explicitly withdrew the
same claim elsewhere (`0675e40a5`). This lane's probes are back in
`probes/` too, as of this commit:

```text
probes/UnsafeShadowSweep.java              the 457-row differential sweep
probes/UnsafeNullArgProbe.java             33 null/OOB rows, one call per process
probes/UnsafeSubwordProbe.java             26 sub-word atomic rows, behind a timeout
probes/AllocBoundary.java                  the allocateMemory IAE/OOME boundary
probes/UnsafeImageCensus.java              the multi-image declaration census
probes/SegmentClassProbe.java              the FFM interface-class rows
probes/unsafe-l1-run.sh                    the three-arm runner
probes/unsafe-l1-residual-counts.sh        the R3/R5 counters
probes/unsafe-l1-invocation-census.py      the retirement invocation census
probes/unsafe-registrations.txt            the 212 triples the census reads
```

**The lesson is the section, not the probes.** A page that records the
state of the tree rather than the state of a DEFECT rots at the speed of
the tree — this one was wrong within hours of being written, in a
directory whose own index warns that its snapshots rot. What was worth
keeping is below and still true: an instrument that lives in the VM
(`note_unsafe_side_store_offset`) outlives any directory somebody can
delete, and §12 is the proof — it answered R5 from another lane's corpus
logs, with no probe involved at all.

<details><summary>The withdrawn text, kept for the record</summary>

### (withdrawn) The probes are no longer in the tree — 2026-08-29

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


</details>

---

## 11. The seven retired, and the manifest row that could not be wrong — 2026-08-29

§9.1 left seven registrations as "an adjudicated list, not retired". They are
retired now, and so is the `deprecated_verify.rs` row §9.1 flagged.

```text
jdk/internal/misc/Unsafe  defineAnonymousClass  (Ljava/lang/Class;[B[Ljava/lang/Object;)Ljava/lang/Class;   x2 registrars
jdk/internal/misc/Unsafe  getReferencePlain     (Ljava/lang/Object;J)Ljava/lang/Object;
jdk/internal/misc/Unsafe  putReferencePlain     (Ljava/lang/Object;JLjava/lang/Object;)V
jdk/internal/misc/Unsafe  monitorEnter          (Ljava/lang/Object;)V
jdk/internal/misc/Unsafe  monitorExit           (Ljava/lang/Object;)V
jdk/internal/misc/Unsafe  park                  (Ljava/lang/Object;J)V
sun/misc/Unsafe           defineClass           (Ljava/lang/String;[BIILjava/lang/ClassLoader;...)
```

### 11.1 The evidence, in the order this campaign requires it

1. **Absent from every supported image** — JDK 17.0.20.1+1, 21.0.12+8 and
   25.0.4+7, per §9.1's census of all 212 triples. Not read off one image:
   three OTHER rows of that same census went the other way.
2. **Absent from the fourth image too.** The synthetic-JDK mode fabricates
   these classes through `synthetic_stub_ctor_methods`, a fixed per-class list
   that gives `Unsafe` only `<clinit>`. There is no on-demand method
   fabrication — `fabricate_class` mints classes for a constant-pool reference,
   never methods — so nothing can name these there either.
3. **No `javac` at any supported release can emit a call to them**, because the
   class does not declare them. The only caller shape left is bytecode compiled
   against JDK 8, which this VM does not support.
4. **0 invocations across 118 corpus vectors, in BOTH modes**, with live
   controls in the same runs: `objectFieldOffset1` 964/1044 in all 118 vectors,
   `compareAndSetInt` ~35k, `arrayIndexScale` 166/182 in all 118 — and the
   tightest control available, **`park(ZJ)V` at 781 invocations in 16 vectors
   while the `park(Ljava/lang/Object;J)V` retired here is 0**. Two rows
   differing only by descriptor, one live and one dead.
5. **Registrar history**: every one traces to the initial open-source commit.
   No diagnosed defect is behind any of them.

The handler functions are left in place — the crate allows `dead_code` — so a
future image that declares one can be served by re-registering a line rather
than by rediscovering a body. `native_unsafe_park_with_blocker` is retained for
a second reason: a unit test calls it directly.

**One premise died on the way.** `defineAnonymousClass`'s registration was
justified in-source as *"JDK 8 surface, ByteBuddy still emits"*. Even if
ByteBuddy does emit it, no supported image declares the method, so the call
fails resolution against the real class whatever is registered here. A guard
scoped by a stated premise is only as good as the premise.

### 11.2 The manifest row that nothing could falsify

`deprecated_verify.rs` tagged `sun/misc/Unsafe.defineClass` as
`ImageStatus::Declared` — whose own doc reads *"At least one supported image
declares the triple … and MUST stay"*. The census says ABSENT on all three.

**The tag could not be wrong in any way the test could detect.** For a
`Declared` row the test asserts only that the REGISTRATION is present; it never
checks the claim the tag makes about the images. The
`AbsentFromAllSupportedImages` half IS enforced — it asserts the registration is
gone. So one direction of this manifest was a live assertion and the other was
decoration.

Retagged, which turns the row into a live assertion and makes the removal the
manifest's own instruction rather than a judgement of mine.

### 11.3 What the retirement moved, and the three tests that had to move with it

A retirement is never only a deletion here. Four other places recorded the old
shape, and each is a gate that would otherwise have gone red on `dev` for
somebody else:

* `registrar_drift.rs` — `defineAnonymousClass` was a recorded DRIFT pair (two
  registrars, two bodies). It no longer drifts, but **not** for the usual
  reason: both bodies are gone, so the triple is registered nowhere.
  Deliberately NOT moved to `FIXED_NOT_DRIFTING`, which asserts a surviving
  body serves the triple in both modes — there is no surviving body. The row is
  deleted and `BASELINE_TOTAL_DRIFT` / `BASELINE_TOTAL_PAIRS` move 1225→1224
  and 1358→1357.
* `registrar_reachability.rs` — a companion gate cross-checks the drift
  baseline against a per-family exposure count and said
  *"recorded 2, measured 1"*. Its own message names the condition under which
  updating the number is the whole fix: the drift baseline moved in the same
  commit. 2→1.
* `deprecated_internal.rs` — two unit tests asserted
  `sun/misc/Unsafe.defineClass` *"should be registered"*. Their subject is the
  CAFEBABE validation, not the spelling, and the `jdk.internal.misc` spelling —
  declared on all three images and still registered — runs the same body, so
  the coverage was **retargeted rather than deleted**. `H11-3` recorded the
  opposite outcome, a unit test that blocked a retirement outright; this is the
  case where it does not have to.

All five gate sets and all three suite arms are green afterwards
(113/113, 113/113, 73/73).

### 11.4 Method note: a failed write truncated a 2310-line source file

`io.open(path, "w", ...)` **truncates the target before it validates its other
arguments**. A patch script passed a malformed `newline=` value; the
`ValueError` fired *after* the file was emptied, and
`native-builtins/src/deprecated_internal.rs` went to zero bytes. It looked like
a grep failure first — text I had read minutes earlier was suddenly not there.

Recovered with `git checkout --` and redone through a temp file plus
`os.replace`. Every patch script here now writes that way. This is the second
time in this session that an `open(..., "w")` destroyed its target before
failing; the first cost a probe source.

---

## 12. R5 CORRECTED — the fix I specified would have broken a DoD workload

§9.3 measured 0 unclassified null-base offsets across 118 corpus vectors, said
the specified refusal was "free on this corpus", and declined to take it because
the corpus excludes the three definition-of-done workloads. **That caution was
right, and this section is why.**

L7 completed on 2026-08-29: all three DoD workloads now run under `--jdk-only`,
and the H2 corpus — 218 test classes — is checked out on the build host with a
per-vector runner that keeps stderr. Its strict run is dated 13:18–13:22, after
this lane's instrument landed at ~11:20, so the answer was already sitting in
those logs. **The marker string exists only in this lane's code, so its presence
in the logs is self-verifying: that binary carried the instrument.**

```text
UNCLASSIFIED-NULL-BASE, H2 strict corpus, 218 classes
  org.h2.test.db.TestFullText      11 warn lines, occurrence reached 513   PASS rc=0  57s
  org.h2.test.unit.TestRecovery     6 warn lines, occurrence reached  17   PASS rc=0  22s
  the other 216 classes             none
  every hit:  offset = 0x0
```

The warn fires on the first occurrence and then at powers of two, so occurrence
513 means **between 513 and 1024 calls in one passing vector**.

**So the refusal §9.3 specified — an `IllegalArgumentException` for an
unclassified null-base offset — would have thrown five hundred times inside a
vector that currently passes.** It is not "licensed but untaken". It is
measured unsafe, and the 0-of-118 that made it look free was the corpus being
blind to the workload the path serves. This is the second time in this lane that
widening the input made a counter confess; the first was the instrument that
could not fire at all (§9.4).

**The shape is narrower than R5 assumed, and that is the useful part.** Every
hit is `offset = 0x0` — a null base at absolute address zero, not a scattered
range of unrecognised offsets. On HotSpot that is a read or write of address 0
and a SIGSEGV; here it lands in the side store and the caller continues. A
future fix has one specific case to explain rather than a category:

* what calls `Unsafe.<get/put/CAS>(null, 0)` hundreds of times in H2's full-text
  and recovery paths, and is it H2's own code, a JDK class, or one of this VM's
  internal callers? The instrument records the offset but not the caller, so
  this is the next measurement, not a conclusion.
* if those calls are writes whose values are never read back, the side store is
  absorbing a no-op and the correct fix may be at the producer rather than here.

**What is NOT claimed.** The compatible arm of that corpus run has only 5 of 218
logs, so its zero is an unrun arm, not a measurement — the same trap this record
already recorded once. And 216 clean classes do not make the other two rare;
they make them specific.

R5 stays OPEN, with a stronger reason than before: not "unmeasured", but
"measured, and the obvious fix is refuted".

---

## 13. R1 on the workload it was written for — 0, and what that is worth

§4.1 declined to match HotSpot's `InternalError` for
`objectFieldOffset1` on a missing field name, because the minted synthetic
offset is documented as the thing that unblocked WildFly's
`Class$Atomic.casReflectionData` and **Spring Boot's
`AbstractClassLoaderValue.putIfAbsent`** — workloads the regression corpus does
not contain. §9.3 measured 0 mints across 118 corpus vectors and said so.

Spring Boot is now reachable: L7's `probes/dod-arms.sh` runs it, and the arm was
re-run here against this lane's instrumented binary.

```text
--jdk-only, this lane's binary, L7's DoD arms
  sbsimple   Spring Boot, full context refresh, 55 beans   rc=0  DOD RESULT OK
             "minting synthetic offset"   0
             UNCLASSIFIED-NULL-BASE       0
  tcssl      embedded Tomcat over HTTPS, 297 beans,
             three real HTTPS requests                     rc=0  DOD RESULT OK
             "minting synthetic offset"   0
             UNCLASSIFIED-NULL-BASE       0
```

So the mint path is **dormant on the workload whose name is in its own
justification**. Across four independent populations it has now fired zero
times: 118 regression vectors, 218 H2 classes, a Spring Boot context refresh,
and a servlet container serving HTTPS.

**That is not a licence to remove it, and the reason is specific.** The rescue
has two named consumers and only one has been measured. WildFly — the other, and
the one whose `casReflectionData` hang the registrar comment actually cites — is
**not checked out on this host** (`apps/wildfly*` does not exist), so its
verdict is absent rather than negative. A rescue with two consumers, one silent
and one unmeasured, is not a rescue that has been shown to be unnecessary.

**R1 therefore stays OPEN, with its remaining question reduced to one name.**
Anyone with a WildFly checkout can settle it: run it under `--jdk-only` with a
binary carrying the existing `objectFieldOffset1` warn — it is already in the
tree and needs no probe — and grep stderr for `minting synthetic offset`.

### 13.1 Method note: the first attempt answered 0 from runs that never ran

The first pass at this reported `UNCLASSIFIED=0 mint=0` for both arms — from
runs that had died **in one second with zero output lines**. This lane's own
worktree has no compiled `DodSpringApp` driver (`probes/out`), so the arm never
reached Spring at all, and the stderr it left carried a `--jdk-only` policy
violation that I briefly mistook for a regression on `dev`.

Two things stopped it becoming a false result, and both were somebody else's
design rather than my care:

* **L7's runner prints `lines=0` and `NO-DOD-RESULT-LINE`.** A runner that
  reports how much output a vector produced turns "it failed instantly" into a
  visible fact instead of a zero.
* **Isolating the variable before believing the conclusion.** Running L7's
  binary with L7's classes (OK), then *this lane's* binary with L7's classes
  (OK) showed the binary was never the problem. Had I stopped one step earlier I
  would have filed a `dev` regression that does not exist.

Third instance in this lane of the same shape — §9.4's mute instrument, §12's
corpus blindness, and now this. **A zero is a claim about a run, and the run has
to be shown to have happened first.**

---

## 14. R1 CLOSED — the mint is unreachable through its own documented consumers

§13 left R1 open on one name: WildFly, which is not checked out on this host.
It is closed now, and not by finding WildFly — by characterising the consumer
surface instead of sampling it.

The mint fires only when `Unsafe.objectFieldOffset(Class, String)` cannot find
the named field. Its registrar comment names two consumers. Asked of JDK 25
directly, with `javap`:

* **`java.lang.Class$Atomic`** resolves exactly **three** names, all on
  `java.lang.Class`, in a single `<clinit>`:
  `reflectionData`, `annotationType`, `annotationData`. That `<clinit>` runs in
  any VM that touches reflection at all.
* **`jdk.internal.loader.AbstractClassLoaderValue`** references `Unsafe`
  **zero times** on JDK 25. **That half of the citation is stale** — whatever it
  did when the comment was written, it does not reach this path now.

So the documented surface is three field lookups on one class, once per process
— not a workload-dependent population. **WildFly reaches the mint only through
the same `<clinit>`, which every measured workload already runs.** Its absence
stopped mattering once the surface was characterised rather than sampled.

Measured with `probes/MintReachProbe.java`, which leads with a positive control
because this lane has been caught three times by a zero from an instrument that
could not fire:

```text
                                          compatible   --jdk-only
CONTROL objectFieldOffset(X, "noSuchField20260829")   mints      mints
CONTROL a second bogus name, distinct offset          mints      mints
Class.reflectionData   resolves non-zero                yes        yes
Class.annotationType   resolves non-zero                yes        yes
Class.annotationData   resolves non-zero                yes        yes
two passes of getDeclaredFields / getDeclaredMethods /
getAnnotations over seven classes
  additional mints                                        0          0
  TOTAL mint warns                                        2          2
```

Two warns, both the controls. The instrument fires exactly when it should and
never otherwise.

**Adjudicated: deliberate, and unreachable through its documented consumers.**
Matching HotSpot's `InternalError` is now known to be a no-op on every measured
path, so it is *available* to anyone who wants contract fidelity — but it should
not be taken alone. The registrar comment names a second scenario the probe
cannot reach: `ConcurrentHashMap.table` *"when the populator failed to wire the
`rj_slot` metadata"*. That is a VM fragility the mint papers over, not a caller
error, and removing a rescue before showing the fragility is gone is the mistake
§12 records. **Precondition stated; row closed.**

---

## 15. R5 diagnosed to the caller and the native — and two hypotheses died

§12 named the next measurement: the instrument records the offset but not the
caller. It does now, and the answer took two wrong guesses on the way.

```text
org.h2.test.db.TestFullText, --jdk-only, PASSES rc=0
  11 warn lines, occurrence reaching 513, every one:
      offset = 0x0
      caller = org/apache/lucene/store/MappedByteBufferIndexInputProvider
      native = getIntVolatile   x10
               compareAndSwapInt x1
```

`org.h2.test.unit.TestRecovery` is the same caller and offset, 6 lines.
**One caller, one offset, two natives, across both classes.**

### 15.1 Two hypotheses that died, and why saying so matters

* **"Lucene is unmapping through `invokeCleaner`."** `javap` on Lucene 9.7's
  `MappedByteBufferIndexInputProvider` shows exactly that — `unmapHackImpl()`
  looks up `sun.misc.Unsafe`, `theUnsafe` and `invokeCleaner` and builds a
  MethodHandle. It is the obvious answer and it is not this one: the natives
  actually reached are `getIntVolatile` and `compareAndSwapInt`.
* **"This VM answers a mapped buffer's address as 0, and Lucene reads through
  it."** That was the §12 hypothesis, and it is REFUTED —
  `probes/MappedAddrProbe.java` reports a non-zero address for a
  `FileChannel.map` buffer, its duplicate and its slice, and for
  `allocateDirect`, **identically on HotSpot and CratonVM in strict mode**. It
  went into §12 as a suggestion; it would have gone into this section as a fact.

### 15.2 What the evidence actually says

`getIntVolatile(null, 0)` followed by `compareAndSwapInt(null, 0, ...)` is the
shape of a **CAS on a field whose offset resolved to 0**, with the base null —
not of an unmapping call. It is the same "offset 0 aliases slot 0" hazard the
`objectFieldOffset1` comment describes, on the static path, being silently
rescued by the side store. The side store is what makes that CAS loop terminate,
which is why H2 passes and why refusing here would hang or break it.

**A caveat on the attribution, stated because it changes what the name means.**
This VM's MethodHandle dispatch does not create intermediate frames, so the
innermost Java frame is the nearest *real* frame, not necessarily the literal
caller. `MappedByteBufferIndexInputProvider` is where to start looking, not
proof that Lucene's own bytecode issues the call.

### 15.3 What would close it

The remaining question is one field: **which field's offset resolved to 0.**
Method-level attribution (the frame walk already added here returns only the
class) or a targeted Lucene repro would name it. The fix then belongs at
whatever produced the 0 — and NOT at the fallback, which §12 established and
this section confirms: 513+ rescued calls in a vector that passes.

Both instruments stay in the tree (`note_unsafe_side_store_offset` now reports
`caller` and `site_line`), so the next measurement costs a run and no probe.

## 16. R5 CLOSED: it was never an offset that resolved to 0

§15.3 named the remaining question as *"which field's offset resolved to 0."*
That question had a false premise, and finding that out took four instruments —
each of which killed the one before it.

### 16.1 The frame walk was reading the wrong end of the stack

§15's attribution came from `frame_class_ids`, whose doc says **"innermost
(most recent call) first"**. Adding method names meant switching to
`capture_stack_trace`, which carries `method_name` and `byte_code_index`. Its
first three entries came back as

```text
org/h2/test/db/TestFullText.main+6
  <- org/h2/test/TestBase.testFromMain+8
    <- org/h2/test/db/TestFullText.test+77
```

for **every** call site — and `main` *calls* `testFromMain` *calls* `test`. A
second case on another thread agreed: `Task.run+1 <- TestFullText$1.call+163 <-
JdbcPreparedStatement.execute+161`.

**`capture_stack_trace` returns OUTERMOST-first; `frame_class_ids` returns
innermost-first.** Neither doc mentions the other, and nothing in the tree
depends on both. Reading the front of one gave the stack's floor, which is
identical for every call site in a vector and therefore says nothing.

Reversed, with the `Unsafe` skip removed (the skip hid three of four frames),
the chain is exact:

```text
sun/misc/Unsafe.invokeCleaner+18
  -> beforeMemoryAccess+19    -> isMemoryAccessWarned+9
                                   -> getIntVolatile(null, 0)
  -> beforeMemoryAccessSlow+104 -> trySetMemoryAccessWarned+11
                                   -> compareAndSetBoolean+15
                                   -> compareAndSwapInt(null, 0, ...)
```

Not Lucene. **JDK 25's own `sun.misc.Unsafe` deprecation-warning latch**, on
the path of every legacy Unsafe memory access — which is why a full-text vector
reached it 513+ times.

### 16.2 Two more hypotheses died on the way

* **"The arguments were truncated by MethodHandle dispatch."** bci 19 of
  Lucene's cleaner lambda is `unmapper.invokeExact(buffer)`, and
  `unsafe_obj(args, 1)` cannot distinguish an absent argument from a null one,
  nor `unsafe_offset(args, 2)` an absent one from a real 0. Logging
  `args.len()` settled it: **`nargs=3` for `getIntVolatile` and `nargs=5` for
  `compareAndSwapInt` — full arity.** The arguments arrived; the null and the 0
  are genuine.
* **"MethodHandle dispatch is the producer."** `UnmapHackProbe.java`
  reproduces Lucene 9.7's unmap hack in 40 lines with **no Lucene and no H2**,
  and pairs it with the control that isolates the axis: the same call made
  directly. It fires on **both** arms (bci 89 through the MethodHandle, bci 149
  through reflection). The MethodHandle is incidental.

### 16.3 The cause: two `static final` fields that were never assigned

`javap` on JDK 25's `sun.misc.Unsafe` shows both latch methods reading
`MEMORY_ACCESS_WARNED_BASE` (an `Object`) and `MEMORY_ACCESS_WARNED_OFFSET` (a
`long`) and passing them straight to `getBooleanVolatile(Object,J)` /
`compareAndSetBoolean(Object,JZZ)`. `<clinit>` computes that pair at bci
185/195 from `staticFieldBase`/`staticFieldOffset` of the static `boolean
memoryAccessWarned`.

`StaticBaseProbe.java` tested the obvious suspect and **acquitted it** — for an
ordinary class this VM is byte-identical to HotSpot: non-null base, distinct
non-zero offsets, a working false→true latch. So nothing resolved to 0.

`WarnLatchProbe.java` asked the other question, with its own control in the
same run:

| | HotSpot | CratonVM |
| --- | --- | --- |
| `MEMORY_ACCESS_WARNED_BASE` non-null | true | **false** |
| `MEMORY_ACCESS_WARNED_OFFSET` non-zero | true | **false** |
| control: `staticFieldBase(memoryAccessWarned)` non-null | true | true |
| control: `staticFieldOffset(memoryAccessWarned)` non-zero | true | true |

**`null` and `0` are the DEFAULT values of two never-assigned fields**, while
the mechanism that should have filled them works perfectly. A zero read as an
answer when it is really an uninitialised field — the same shape as
[`a-consumer-count-cannot-explain-its-own-zero`].

## 17. The bigger defect R5 was standing in front of

`ClinitProbe.java` walks `sun.misc.Unsafe.<clinit>` by bci, checking a field
written before, during and after the region of interest:

| bci | field | HotSpot | CratonVM |
| --- | --- | --- | --- |
| 34/40 | `theUnsafe`, `theInternalUnsafe` | set | set |
| 157 | `ARRAY_OBJECT_INDEX_SCALE` | non-zero | **0** |
| 166 | `ADDRESS_SIZE` | non-zero | **0** |
| 185/195 | latch `BASE`/`OFFSET` | set | **default** |
| 214 | `MEMORY_ACCESS_OPTION` | set | set |

**Every public constant on the legacy `sun.misc.Unsafe` spelling was zero.**

This is not a new failure mode — it is the one `post_clinit_fixup`'s
`jdk/internal/misc/Unsafe` arm already exists to repair, root-caused in that
arm's own comment (ES-FAIL-FAMILY-20260710): `<clinit>` computes these through
natives not yet registered this early in boot, and **an unregistered native
silently returns its return type's zero rather than throwing**, so the
`static final` latches at 0 for the life of the process.

`sun.misc.Unsafe` has its own 18 copies (`<clinit>` bci 43..157 reads
`jdk/internal/misc/Unsafe.ARRAY_*`) plus `ADDRESS_SIZE` at bci 166 — and its
fixup arm repaired only `MEMORY_ACCESS_OPTION`. It copies from the sibling
*before* the sibling's own arm has run, so it copies zeros.

The consequence is the one that arm already spells out: any library following
the documented `offset = ARRAY_<T>_BASE_OFFSET + index` protocol **through the
legacy spelling** gets an offset short by 16 and reads the wrong bytes, with no
exception thrown.

### 17.1 Fixed, with the sibling arm's own values

The `sun/misc/Unsafe` arm now backfills all nineteen — 16 for every
`ARRAY_*_BASE_OFFSET`, the per-type `INDEX_SCALE`s, and `ADDRESS_SIZE = 8`
(`size_of::<usize>()`, matching `native_unsafe_address_size` and the
`ADDRESS_SIZE0` backfill already in that file). The repair writes only a field still
holding 0, so a correctly-initialised future implementation stays
authoritative -- see §18.2, which is where that became true: it was NOT true
as originally written, and this sentence was a false claim about
`set_static_by_name` until `set_static_if_zero` was added.

After the fix, the `ClinitProbe` diff against HotSpot loses both rows: bci 157
and bci 166 now match.

### 17.2 What is deliberately NOT fixed

`MEMORY_ACCESS_WARNED_BASE`/`_OFFSET` (bci 185/195) still hold their defaults,
so the R5 warn still fires. Backfilling them needs the class mirror and this
VM's own static-offset encoding, which `set_static_by_name` cannot synthesise —
a different mechanism, not a longer list.

**Its severity is now known rather than assumed, which is the point of leaving
it visible.** The latch is the JDK's once-only deprecation-warning flag. With
the pair at `(null, 0)` the read and the CAS both land in the private side
store, consistently — so the latch still latches, `H2 TestFullText` and
`TestRecovery` both pass (`rc=0`), and the only observable effect is on when
that deprecation warning prints. This is the same reason §12 and §15 give for
NOT refusing the fallback: 513+ rescued calls in a vector that passes.

## 18. Accepting the fix — and a claim of mine that was false

### 18.1 "Non-zero" is not "correct"

`ClinitProbe` only asked *is it zero*. That was enough to FIND the defect and
is not enough to ACCEPT the repair: **a backfill writing the wrong constant is
also non-zero**, and would read as repaired.

Diffing the values across VMs cannot serve as the oracle either — the right
answers legitimately differ. CratonVM uses a uniform 16-byte header and no
compressed oops, so `ARRAY_OBJECT_INDEX_SCALE` is 8 here and 4 on a
compressed-oops HotSpot; a value diff would flag a correct answer as a defect.

`UnsafeConstAgree.java` uses a **VM-independent invariant** instead. By the
JDK's own construction the legacy spelling's `<clinit>` copies the internal
constant, which the native computes, so all three are one number:

```text
sun.misc.Unsafe.ARRAY_<T>_BASE_OFFSET
  == jdk.internal.misc.Unsafe.ARRAY_<T>_BASE_OFFSET
  == theUnsafe.arrayBaseOffset(<T>[].class)
```

**0 disagreements on both VMs**, across all 9 types × {base, scale} plus
`ADDRESS_SIZE` — and the access that was silently short by 16, a `byte[]` read
through `ARRAY_BYTE_BASE_OFFSET + 2 * ARRAY_BYTE_INDEX_SCALE`, returns the byte
actually stored there.

**A probe artefact that read as a defect, recorded because it nearly became
one.** The first version read the internal constants with `setAccessible`.
That threw `InaccessibleObjectException` on CratonVM and succeeded on HotSpot —
but only because the HotSpot command line carried `--add-opens` and the
CratonVM one did not. It printed as a sentinel, which looks exactly like a
missing field, which would have been a definition-of-done finding.
`InternalFieldProbe.java` separated the two by reporting the *throwable class*
rather than a value: `declared-but-InaccessibleObjectException`, so the fields
exist and the class is the real one. (CratonVM does accept `--add-opens` and
`--add-exports`; referencing the constants directly removes the artefact
entirely.)

### 18.2 The repair's own count was not a measurement. Now it is.

**§17.1 said `set_static_by_name` "writes only a field still holding 0". That
is false** — it writes unconditionally, returning true when it locates the
field and the value fits the descriptor. Two consequences, both mine:

* the arm's `19/19` meant *"nineteen fields found and written"*, not
  *"nineteen were broken"* — so it was not evidence for what it was being cited
  as evidence for;
* the arm would overwrite a correct value if the underlying ordering were ever
  fixed, which is the opposite of what I claimed.

`set_static_if_zero` makes the claim true rather than retracting it: it reads
the slot first and writes only a zero. The count is now a measurement —
**`19/19` means all nineteen really were zero**, and a future `0/19` means the
ordering has been fixed upstream and this arm is dead weight that can be
deleted.

Applied to **this lane's arm only**. The sibling `jdk/internal/misc/Unsafe`
arm is another lane's, has its own history, and changing *when* it writes is a
behaviour change I have no measurement for.

### 18.3 The family, sized

The fixup arms' own log lines from a single run:

| arm | count | is the count a measurement? |
| --- | --- | --- |
| `jdk/internal/misc/Unsafe` ARRAY_* | 18/18 | no — unconditional, "found" |
| `jdk/internal/misc/UnsafeConstants` | 5/5 | no — unconditional, "found" |
| `sun/misc/Unsafe` ARRAY_*/ADDRESS_SIZE | **19/19** | **yes** — conditional |

Nineteen zeroed constants are measured; the other twenty-three are *repaired*
but their counts do not establish that they were broken. `ClinitProbe`
independently measured two of the nineteen (bci 157, bci 166) against HotSpot
before the fix, which is what turned the inference into a finding in the first
place.

### 18.4 An empty instrument that is NOT an absence proof

`--dump-missing-natives` and `--dump-missing-natives-grouped` both come back
empty on a run that demonstrably suffers the defect. That is **not** evidence
that no natives were missing: nothing here shows those dumps can fire, and the
latching happens during early boot, before the point a workload-level dump
describes. Recorded as un-adjudicated rather than counted as a clean result —
a zero from an instrument with no positive control is not a zero.

## 19. R5 fully closed: the latch pair repaired, and the warns are gone

§17.2 left the latch pair unfixed on the grounds that it "needs the class
mirror and this VM's own static-offset encoding, which `set_static_by_name`
cannot synthesise — a different mechanism, not a longer list."

That was right about the mechanism and wrong about the difficulty, because it
assumed the repair had to *synthesise an encoding*. It does not. Reading
`native_unsafe_static_field_offset` shows what the call actually does:

```rust
let offset = synthetic_offset_for(&class_name, &format!("static:{field_name}"));
remember_unsafe_static_field_offset(offset, class_id, field_index);
```

**The registration is the load-bearing half.** The number alone is inert; it is
the `unsafe_static_field_targets` entry that later routes a null-base
`getBooleanVolatile`/`compareAndSetBoolean` to the real static slot instead of
into the private side store. Minting an offset without registering it would
have *moved* the defect while looking like a fix.

So the repair makes the same mint-and-register call `<clinit>` would have made,
through a new `register_static_field_offset` in `native-builtins`, and stores
the class mirror in `MEMORY_ACCESS_WARNED_BASE` — which is exactly what
`native_unsafe_static_field_base` answers for a static field on this VM, and a
shape `StaticBaseProbe` had already proved round-trips byte-identically to
HotSpot.

### 19.1 Result

```text
Post-clinit fixup: sun.misc.Unsafe memory-access latch repaired (2/2)
```

`2/2` through the conditional writer, so both really were at their defaults.

| probe | before | after |
| --- | --- | --- |
| `ClinitProbe` vs HotSpot | 2 rows differ | **no differences** |
| `WarnLatchProbe` vs HotSpot | 3 rows differ | **no differences** |
| `UnmapHackProbe` warns | 3 | **0** |
| `org.h2.test.db.TestFullText` warns | 11 (occurrence → 513) | **0** |
| `org.h2.test.unit.TestRecovery` warns | 6 | **0** |

Both H2 vectors still pass, `rc=0`.

### 19.2 The zero is a real zero

A fall to 0 is only evidence if the instrument can still fire, and this lane
has three times been caught reading a mute instrument as a clean result.
`NullBaseControl.java` makes the access the instrument exists to count — a null
base with an offset that is not an arena handle, not a synthetic offset and not
a registered static field:

```text
CONTROL warns: 2      offset=0x7654321 site_line=2924
                      offset=0x7654329 site_line=2623
```

Both sites fire. The zeros above are measurements, not silence.

**And the control also shows what was NOT fixed**, which is why it is worth
keeping in the tree: it prints `cas |true|` for a compare-and-swap that wrote
nowhere any reader can see. The null-base side store still invents a slot for a
genuinely unclassified offset. R5 is closed because nothing in the JDK reaches
that path any more — not because the path became safe. The instrument stays,
and it is now quiet enough that a future occurrence is a signal rather than
noise.
