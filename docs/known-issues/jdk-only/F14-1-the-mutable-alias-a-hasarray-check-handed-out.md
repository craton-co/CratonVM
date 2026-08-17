# F14-1 — the mutable alias `hasArray()` handed out, and the supertype that makes half the family's assertions ornamental

> **RECONCILED 2026-08-17 (lane G40) — the mode/reachability argument in §"what
> the fixtures actually measure" is VOID. The mutable-alias defect and the
> supertype finding are untouched.**
>
> This record argues that because `CharBuffer`/`ByteBuffer` carry `Code` and
> neither class is in `force_native_over_real_jdk_bytecode` nor the
> `check_override` name chain, the fixtures measure field resolution in real-JDK
> mode and this lane's natives only under `--features synthetic-jdk`. **The force
> list is not the gate.** `G34-1` measured it on a real binary in both
> directions, cold and warm: under `--jdk-only`, registering a `Bridge` is **by
> itself sufficient** to preempt real JDK bytecode, because
> `try_stackless_invoke` step 1 answers *before* method resolution and passes
> `bytecode_available: false` unless `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed.
> The force list is a later, cache-shape-only override consulted by the vtable
> inline cache and the JIT. Which body runs is **site**-dependent, not
> triple-dependent.
>
> **What this banner does NOT claim.** It does not say the conclusion is wrong —
> only that the argument is void. Re-deriving it needs a registry dump and a
> probe on the current binary, from a lane that may edit Rust. That was not done.
> Note also that this record's §N1 was already falsified by measurement once
> (INDEX, third pass: `native_tb_array` never ran), which is the same failure
> mode: a reachability claim settled by reading. See `INDEX.md` §B.2 and §D.1.

**2026-08-13, lane F14.** Lands F5-1's NOMINATIONS 1, 3 and 4 — the
`native-io/src/lib.rs` copy of the accessible-backing-array contract, the
`servlet.rs` detail message, and the missing fixture cell — plus one defect
found while landing them (§5) and one correction to F5-1's own reasoning (§2).

Also folds in **F2-1 NOMINATION 3**, all three parts (§7).

**Provenance: MEASURED oracle, PREDICTED VM.** Every expected value is a pasted
transcript from `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build)
on this host. **This lane may not build or run CratonVM**, so every "after" is a
prediction and is labelled as one. Probes: the existing
`probes/BufferAccessibleArrayProbe.java`, plus `scratchpad/f14/`
{`F14BufferProbe.java`, `F14HexProbe.java`, `F14Mutation.java`}.

Files changed: `native-io/src/lib.rs`, `native-builtins/src/servlet.rs`,
`regression-suite/src/RJdkIntrinsics2.java`. All three are this lane's.

---

## 1. The contract, and the line numbers verified

JDK 25, `java.base/java/nio/ByteBuffer.java` L1490 / L1513 / L1541 — and the
byte-identical bodies in `CharBuffer.java` at the *same three line numbers*,
both generated from `X-Buffer.java.template`:

```java
public final boolean hasArray() { return (hb != null) && !isReadOnly; }

public final byte[] array() {
    if (hb == null)  throw new UnsupportedOperationException();
    if (isReadOnly)  throw new ReadOnlyBufferException();
    return hb;
}

public final int arrayOffset() {          // the identical split
    if (hb == null)  throw new UnsupportedOperationException();
    if (isReadOnly)  throw new ReadOnlyBufferException();
    return offset;
}
```

> **CORRECTION 2026-08-14 (F41-1 §2) — the `native_tb_array` half of this
> record fixed a body that NEVER RAN.** The line numbers below are right and
> the reasoning is right; the ownership is not. Measured with
> `--dump-native-registry` on a built binary:
>
> ```
> java/nio/IntBuffer     ()[I  by=servlet.rs:6581  owns_slot=true  overwrote=null
> java/nio/LongBuffer    ()[I  by=servlet.rs:6581
> java/nio/ShortBuffer   ()[I  by=servlet.rs:6581
> java/nio/FloatBuffer   ()[I  by=servlet.rs:6581
> java/nio/DoubleBuffer  ()[I  by=servlet.rs:6581
> ```
>
> Two defects in one row. The loop hardcodes `()[I` for five families with
> five different element types, so **four are phantom registrations** nothing
> can dispatch to; and for `IntBuffer` — the one whose descriptor happens to
> be right — `servlet.rs` is the **sole** registrant, so `native_tb_array`
> and every refusal added to it here were unreachable.
> `ByteBuffer.allocate(16).asIntBuffer().array().length` still raised
> NullPointerException where HotSpot throws UnsupportedOperationException,
> until `fdacf3a01`.
>
> The lesson is not that this lane was careless — it verified its line
> numbers, which is more than most. It is that **a line number proves where a
> body is, never that the body runs.** Only the registry dump answers that.

F5-1's line numbers for the four defective natives were verified before editing
and were all correct: `native_bb_array` L9703, `native_bb_has_array` L9717,
`native_bb_array_offset` L9730, `native_tb_array` L16018.

### 1.1 The four element types nobody had measured

F5-1 says its probe "already carries measured oracle rows for all seven element
types". It does not — `BufferAccessibleArrayProbe` covers **byte, char and
int**. `native_tb_array` and `native_bb_has_array` are registered on **six**
typed families, so `scratchpad/f14/F14BufferProbe.java` measured the other four
rather than inferring them from the "one template" argument:

```text
long.ro.array      java.nio.ReadOnlyBufferException          getMessage=null
long.view.array    java.lang.UnsupportedOperationException   getMessage=null
float.ro.array     java.nio.ReadOnlyBufferException          getMessage=null
float.view.array   java.lang.UnsupportedOperationException   getMessage=null
double.ro.array    java.nio.ReadOnlyBufferException          getMessage=null
double.view.array  java.lang.UnsupportedOperationException   getMessage=null
short.ro.array     java.nio.ReadOnlyBufferException          getMessage=null
short.view.array   java.lang.UnsupportedOperationException   getMessage=null
```

Identical, as the template argument predicts. Recording it so the next lane does
not re-derive it, and because "generated from one template" is a claim about
the *source*, not about what shipped.

---

## 2. CORRECTION to F5-1: `ReadOnlyBufferException` **is** an `UnsupportedOperationException`

F5-1 §1 states the two classes *"share no supertype below `RuntimeException`, so
no `catch` can absorb the difference."* MEASURED, same JDK:

```text
ROBE.super   OK java.lang.UnsupportedOperationException
UOE.super    OK java.lang.RuntimeException
```

and `java.base/java/nio/ReadOnlyBufferException.java:40`:

```java
public class ReadOnlyBufferException
    extends UnsupportedOperationException
```

So `catch (UnsupportedOperationException)` **does** absorb
`ReadOnlyBufferException`, and it absorbs it in exactly one direction. This
matters twice over and both times against the reader:

* An **`instanceof`-shaped or subtype-shaped assertion** that a receiver "throws
  `UnsupportedOperationException`" passes when the implementation wrongly
  throws `ReadOnlyBufferException`. Half the discriminating power a
  hierarchy-shaped test appears to have is not there. Every row this lane added
  compares the **exact class name** through the fixture's existing `nameOf()`,
  and `scratchpad/f14/F14Mutation.java` prints the control:

  ```text
  (control) ReadOnlyBufferException instanceof UOE = true
            -> an instanceof-shaped row would let the roArr.array() mutant LIVE
  ```

* It is a wrong reason attached to a right conclusion. The conclusion — that
  the two arms must be implemented separately — stands on the ORDER (§3), not
  on the class hierarchy. A future lane that checks the hierarchy claim, finds
  it false, and concludes the distinction does not matter would be wrong about
  something F5-1 got right.

`[hypothes]`: a known-issue doc's reasoning can be wrong, not only stale.

---

## 3. Why the split is not "read-only versus not", measured

```text
vib.getClass    OK java.nio.ByteBufferAsIntBufferB
vib.isReadOnly  OK false                                    <- WRITABLE
vib.hasArray    OK false
vib.array       java.lang.UnsupportedOperationException      <- and still UOE
rdb.getClass    OK java.nio.DirectByteBufferR
rdb.array       java.lang.UnsupportedOperationException      <- read-only AND array-less
rbb.getClass    OK java.nio.HeapByteBufferR
rbb.array       java.nio.ReadOnlyBufferException
```

Two independent witnesses. `vib` is writable and answers `UnsupportedOperation`,
so the test is not on `isReadOnly`. `rdb` is read-only *and* array-less and
answers `UnsupportedOperation`, so `hb == null` is asked **first**. An
implementation that asks read-only first gets that one cell wrong and every
other cell right.

---

## 4. What was there, and what landed — `native-io/src/lib.rs`

| site | before | HotSpot |
|---|---|---|
| `native_bb_array` | `Heap -> the array`, `Direct -> UOE("ByteBuffer has no backing array")`. `isReadOnly` never read | UOE(null msg) / ROBE / the array |
| `native_bb_has_array` | `Heap -> 1`, `Direct -> 0` | `(hb != null) && !isReadOnly` |
| `native_bb_array_offset` | `Heap -> offset`, `Direct -> UOE(msg)` | UOE(null msg) / ROBE / `offset` |
| `native_tb_array` | heap -> array; direct -> UOE(msg); **storage-less -> `null`** | UOE / ROBE / the array |
| `native_bb_is_read_only` | `Ok(Int(0))` — a constant (§5) | the `isReadOnly` field |

**THE SEVERITY, and why it outranks a wrong answer.** `hasArray()` answering
`true` for a `HeapByteBufferR` is a wrong value. `array()` then handing that
receiver its backing `byte[]` is a wrong **capability**: a caller who followed
the documented `hasArray()` → `array()` protocol — the protocol the javadoc
tells it to follow — received a **mutable alias to storage the JDK guarantees
is not writable through that handle**, and every write through it corrupted a
read-only buffer with no exception at any point. The two defects compose: the
first is what steers the caller into the second.

`native_tb_array`'s null is the same shape one level quieter. Its own doc
comment excused it — *"turning that into a throw is a behaviour change this lane
has no vector to measure"* — and the receiver it named as the reason
(`native-builtins`' typed views, "neither array nor address") is precisely the
receiver HotSpot answers `UnsupportedOperationException` for. There is a vector
now (§6). A `null` returned through a `()[I` descriptor surfaces as a
`NullPointerException` at the caller's `arraylength`, at a site with no
connection to the buffer.

**The fix: one decision function, five call sites.**

```rust
pub enum BufferArrayAccess { Accessible, ReadOnly, Absent }

pub fn buffer_array_access(has_hb: bool, read_only: bool) -> BufferArrayAccess {
    if !has_hb        { BufferArrayAccess::Absent }      // FIRST, per the JDK body
    else if read_only { BufferArrayAccess::ReadOnly }
    else              { BufferArrayAccess::Accessible }
}
```

**It could not be `use`d from the sibling lane's copy, and the reason is worth
recording.** F5-1's `cb_array_access` lives in
`native-builtins/src/phases_late/charset_buffers.rs`, and
`cratonvm-native-builtins` **depends on** `cratonvm-native-io`, not the
reverse (`native-builtins/Cargo.toml:57`). A nomination that says "reuse the
existing function" is only actionable if the dependency edge points the right
way — the same shape F2-1 §2.1 hit with a private callee, one edge up. This
copy is therefore `pub`, in the crate both other registrars can already see, and
NOMINATION N1 below is the convergence.

Also landed, because leaving them made the four accessors mutually
inconsistent:

* `bb_is_read_only` reads the `isReadOnly` **FIELD**, transcribed from
  `servlet.rs::s2_bb_is_read_only`. The field and not the method, because that
  is what the JDK bodies read: `ByteBufferAsIntBufferRB` &c. override the METHOD
  and never write the FIELD, and are still served correctly because they have no
  `hb` and the `Absent` arm answers first. MEASURED:
  `allocate(8).asReadOnlyBuffer().asIntBuffer()` is `isReadOnly() == true` and
  `array()` → **UnsupportedOperation**.
* `bb_no_backing_array()` uses the EMPTY message, which `types/src/error.rs`
  maps to the `()V` constructor, so `getMessage()` is null (§5.1).
* All four accessors resolve `hb` through `bb_resolve_heap_array` instead of
  `bb_storage_view`. `bb_storage_view` is three-valued
  (heap / direct / `InternalError`) where the JDK's question is two-valued, and
  its error arm gave `hasArray()` — a method that on HotSpot is one `&&` over
  two fields and **cannot throw** — a throwing path. The heap arm is unchanged:
  `bb_resolve_heap_array` is `bb_storage_view`'s own first step, slot-5 kind
  screen included. The `"ByteBuffer missing backing storage"` diagnostic is not
  lost; every get/put on the same receiver still goes through `bb_state` /
  `bb_storage_view`, where a missing backing store really is a VM inconsistency
  rather than a legal Java state.

### 4.1 Tests

Three `#[test]`s on `buffer_array_access` and one on `bb_no_backing_array` —
deliberately **not** receiver-shaped. `MockNativeContext` resolves field NAMES
through a slot fallback, so a receiver-shaped assertion about `isReadOnly` would
be measuring the mock's name-to-slot table rather than the ordering rule
(`[mock=slot table]`). The two existing receiver tests
(`typed_array_accessor_refuses_a_direct_receiver_and_never_returns_mark`) still
hold and were re-checked against the new control flow: a direct receiver is
`Absent` and still throws; a heap receiver is `Accessible` and still returns its
array.

---

## 5. Found while landing: `native_bb_is_read_only` was a constant

```rust
fn native_bb_is_read_only(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}
```

Registered on `java/nio/ByteBuffer`, `java/nio/HeapByteBuffer` **and the
catch-all `java/nio/Buffer`**, so it answers for every abstract-stamped typed
buffer this VM mints — including the ones this crate's own `$ro_fn` macro arm
has just marked `isReadOnly = 1` by name (`lib.rs` typed-buffer macro).

This is not merely a wrong answer; after the fix above it is an **impossible
state**. A read-only heap buffer would report `hasArray() == false`, throw
`ReadOnlyBufferException` from `array()`, and report `isReadOnly() == false` —
no real receiver can be in that state, and a caller that branches on
`isReadOnly()` rather than on `hasArray()` is steered straight into the throw
it was trying to avoid. It now reads the same field the other three do, so the
four agree by construction. MEASURED: `allocate(8).isReadOnly()` is `false`,
`.asReadOnlyBuffer().isReadOnly()` is `true`, in all seven families.

A layout with no such field still answers `false` — the previous answer for
every receiver that had one — so nothing that was right before changes.

### 5.1 N3 — `servlet.rs`'s UOE carried a detail message

`register_s2_bytebuffer`'s `array()` and `arrayOffset()` raised
`UnsupportedOperationException { message: "direct buffer has no backing array" }`
where HotSpot's is `new UnsupportedOperationException()`. MEASURED,
`getMessage()` on all four UOE arms of this family:

```text
directByteBuffer.array        java.lang.UnsupportedOperationException  getMessage=null
directByteBuffer.arrayOffset  java.lang.UnsupportedOperationException  getMessage=null
charView.array                java.lang.UnsupportedOperationException  getMessage=null
stringCharBuffer.array        java.lang.UnsupportedOperationException  getMessage=null
new UOE().getMessage          OK null
```

Both sites now call one `s2_bb_no_backing_array()` with the empty-message
spelling. **Why it survived W7-83 §7.1, which repaired everything else in this
family**: the CLASS was right, so no `catch`, no class-name assertion, and — the
part worth recording — **no variant-shaped `matches!` either**. The existing
test at `servlet.rs` matches `RuntimeError::UnsupportedOperationException { .. }`,
which is blind to the message by construction. A new test reads it.

---

## 6. N4 — `RJdkIntrinsics2 --only=bounds`: the cell the fixture never asked about

`bounds` asserted the `StringCharBuffer` cell (read-only AND array-less → UOE)
and the writable array-backed cell, and never the third. The read-only
**array-backed** cell is exactly where the old code handed out the mutable
alias, and the fixture passed regardless — `[gap masks bug]`.

Nine rows added as **GAP 3b**, all measured:

| row | measured |
|---|---|
| `allocate(4).asReadOnlyBuffer().isReadOnly()` | `true` |
| `.hasArray()` | `false` |
| `.array()` | `java.nio.ReadOnlyBufferException` |
| `.arrayOffset()` | `java.nio.ReadOnlyBufferException` |
| `ByteBuffer.allocate(16).asIntBuffer().isReadOnly()` | `false` |
| `.hasArray()` | `false` |
| `.array()` | `java.lang.UnsupportedOperationException` |
| that UOE's `getMessage()` | `null` |
| `.arrayOffset()` | `java.lang.UnsupportedOperationException` |

The last four are the fourth cell F5-1 N4 asks for — writable, array-less — and
they are what prove the split is not "read-only versus not". The
`getMessage() == null` row is the **only assertion in the file that can see a
wrong detail message**; every other row compares a class name.

**`bounds` denominator: 93 → 102.** Re-derived by running the family on
jdk-25.0.3+9 (`CK RJdkIntrinsics2 bounds=102`), not by adding nine on paper.
The whole fixture is `PASS RJdkIntrinsics2 (1003 checks)` on HotSpot,
up from 990.

### 6.1 Mutation check

`scratchpad/f14/F14Mutation.java` replaces each new row's expected value with
what a **specific wrong implementation** would answer — never with a negated
condition — and runs it on HotSpot. All eighteen mutants die, including:

```text
MUTANT-DIES  !roArr.hasArray()   [PRE-FIX native_bb_has_array: Heap -> 1, never reads isReadOnly]
MUTANT-DIES  roArr.array()->ROBE [PRE-FIX native_bb_array: Heap -> hand the array over]
MUTANT-DIES  vw.array()->UOE     [PRE-FIX native_tb_array: storage-less -> null, .length is an NPE]
MUTANT-DIES  vw.array() msg null [UOE { message: "direct buffer has no backing array" }]
MUTANT-DIES  !vw.hasArray()      [the split written as read-only-versus-not: vw is writable -> true]
```

Each of the five names a defect this lane actually fixed, so the rows are
demonstrably load-bearing rather than merely true.

### 6.2 Predicted effect on CratonVM

PREDICTED, not measured. `bounds` stops at its first failure, so the fixture's
own §5-style ordering applies: F5-1 predicts the family currently fails at line
2986 (`CharBuffer.wrap(String).array()`), which is **before** GAP 3b. So until
F5-1's own fix lands, **none of these nine rows executes**, and the first
observation that matters is F5-1 §6's discriminator, not this block.

Once execution reaches GAP 3b, in **real-JDK mode** all nine rows are served by
real JDK bytecode — `hasArray`/`array`/`arrayOffset` are `final` on
`CharBuffer`/`ByteBuffer`, which carry `Code`, and neither class is in
`force_native_over_real_jdk_bytecode` nor the `check_override` name chain — so
they measure the VM's *field resolution* for `hb` / `isReadOnly`, which is F5-1's
N2. In **`--features synthetic-jdk` mode** they measure this lane's four natives
directly, and are predicted GREEN.

`sectionEnd` asserts an exact count, so a VM that reaches the block and answers
every row correctly prints `CK RJdkIntrinsics2 bounds=102`; any other number is
a failure, not a warning.

---

## 7. F2-1 NOMINATION 3, all three parts

Folded in at the orchestrator's direction. `hex` denominator: **73 → 77.**

**(a) The message that contradicted its own expectation.** The row asserting
`parseHex(char[], 1, 5) == {0, 0xff}` said *"must honour its own
offset/length"*. The parameters are `(fromIndex, toIndex)`
(`HexFormat.java:577`); "offset, length" is the class javadoc at `:79` and it is
wrong. Message corrected, and it now states both readings with the measured
consequence rather than just naming the right one:

```text
parseHex(char[9], 1, 5)                    OK [0, -1]
(1,5) as offset/length would be            [0, 0, f, f, 0]      <- five chars, ODD, throws
```

**(b) and (c), APPENDED at the end of the family on purpose.** F2's prediction
table is keyed by check NUMBER (it stops at 32 of 73 and names 32–49, 54–57, 69,
73). Inserting these beside the other `parseHex` rows would have renumbered every
entry in that table. They are checks **74–77**; every number F2 predicted still
means what it meant.

| row | measured |
|---|---|
| `parseHex(new StringBuilder("00ff0a80"))` | `[0, -1, 10, -128]` |
| `parseHex(CharBuffer.wrap("x00ff0a80y", 1, 9))` | `[0, -1, 10, -128]` (its `length()` is the REMAINING count, 8) |
| `parseHex(char[9], 3, 1)` | `java.lang.IndexOutOfBoundsException` |
| its message | `Range [3, 1) out of bounds for length 9` |

The `CharBuffer` row is the one that matters: it is the operand the JDK's own
`parseHex(char[], int, int)` manufactures internally, so it is what stands
between the ranged overloads and F2-1 §1.1's silent empty answer. The message
row pins F2-1's "`Range [f, t)` counts the WHOLE operand, `string length not
even` counts the SLICE" fact — the two bounds messages in this one family have
different denominators.

Mutation-checked with the same harness; the pre-fix `String`-only reader
(`-> new byte[0]`), a silent empty answer for the reversed range, a
`NegativeArraySizeException` underflow and a slice-counted message all die.

**`--only=hex` is `CK RJdkIntrinsics2 hex=77` on HotSpot.** F2's predicted flips
are unchanged; only the denominator moved, and it moved because four rows were
appended after check 73.

---

## 8. Swept and CLEARED — checked, measured, NOT changed

Against the same transcripts. Recorded so the next lane does not re-derive them.

| cell | CratonVM | oracle | verdict |
|---|---|---|---|
| `native_bb_is_direct` | `Heap -> 0`, `Direct -> 1` | `hbb/rbb.isDirect false`, `dbb/rdb true` | correct; read-only is orthogonal to directness and this native correctly ignores it |
| writable heap `array()` / `arrayOffset()` | array, `offset` | `hbb.array 8`, `hbb.arrayOffset 0` | unchanged by the fix — the `Accessible` arm is byte-for-byte the old happy path |
| `hbb.slice().arrayOffset()` | `bb_resolve_heap_offset` | `0` | correct |
| `hbb.position(2).slice().arrayOffset()` | `bb_resolve_heap_offset` | `2` | correct — the window base still comes through |
| `servlet.rs` `array()` read-only arm | `ReadOnlyBufferException` | `rbb.array ROBE` | already right since W7-83 §7.1; only the message changed |
| `servlet.rs` `hasArray()` | `arr.is_some() && !is_read_only` | `rbb.hasArray false` | already the full three-way rule |
| `servlet.rs` `isReadOnly()` | reads the field | `rbb.isReadOnly true` | already right — the twin `native-io` had drifted from |
| `charset_buffers.rs` `cb_array_access` and its three call sites | three-way | §1.1 | F5-1's, verified as the transcription source; unchanged |
| the six typed families' `asReadOnlyBuffer` (`$ro_fn`) | copies, then sets `isReadOnly = 1` by name | `.ro.isReadOnly true` for all six | correct, and it is what makes the field-read in §4 the right input in synthetic mode |
| `ReadOnlyBufferException` detail message | `RuntimeError::ReadOnlyBufferException` -> `("java/nio/ReadOnlyBufferException", None)` | `getMessage=null` on all eight ROBE rows measured | correct, no change needed |
| all seven `hasArray()` happy-path answers | `Heap && !ro -> 1` | `hbb/long/float/double/short.heap.hasArray true` | correct after the fix |

---

## 9. NOMINATIONS

**N1 — converge the three copies of `buffer_array_access` onto the one that can
be imported.** The rule now exists three times: `charset_buffers.rs`'s
`cb_array_access`, `servlet.rs`'s inline `s2_bb_arr(..).is_some() && !s2_bb_is_read_only(..)`
pair, and this lane's `native_io::buffer_array_access`. Only the third is
importable by the other two — `cratonvm-native-builtins` depends on
`cratonvm-native-io`, so the edge runs that way and no other. Concretely:
`charset_buffers.rs` deletes `CbArrayAccess`/`cb_array_access` and calls
`cratonvm_native_io::buffer_array_access`, and `servlet.rs`'s `array()` /
`arrayOffset()` / `hasArray()` arms do the same. Not this lane's call to make
for `charset_buffers.rs`; `servlet.rs` is this lane's file and was left on its
own spelling deliberately, so the convergence is one reviewable edit rather than
half of one. `cb_is_read_only` / `s2_bb_is_read_only` / `bb_is_read_only` are a
second, identical, three-copy set in the same edit.

**N2 — `docs/known-issues/jdk-only/F5-1-…md` §1 states a false fact about the
class hierarchy.** *"share no supertype below `RuntimeException`, so no `catch`
can absorb the difference"* — `ReadOnlyBufferException extends
UnsupportedOperationException` (§2, measured and quoted from
`jdk25src`). The conclusion the sentence supports is right for a different
reason (the ORDER), so the fix is to replace the reason, not to weaken the
conclusion. Not this lane's file. The operational consequence is in §2 and
belongs in any doc that tells a reader to assert on these two classes:
**subtype-shaped assertions on this pair are one-directional and therefore
ornamental in one direction.**

**N3 — `native_bb_is_direct` is registered on the `java/nio/Buffer` catch-all and
answers from storage; `native_tb_array`'s siblings `hasArray`/`arrayOffset` are
not registered for the six typed families at all.** `arrayOffset` in particular
is registered only on `java/nio/{ByteBuffer,HeapByteBuffer}` — the six typed
families have `array` and `hasArray` and no `arrayOffset`, so in
`--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolves to
`java/nio/Buffer` with no `Code` and no registration. MEASURED oracle for all
six is in `scratchpad/f14/F14BufferProbe.java` (`*.heap.arrayOffset OK 0`,
`*.ro.arrayOffset ROBE`, `*.view.arrayOffset UOE`); the fix is one registration
per family over the same `buffer_array_access`. This lane did not add it because
a new registration in synthetic-jdk mode is a behaviour change with no fixture
row behind it — GAP 3b's `arrayOffset` rows go through `CharBuffer`/`IntBuffer`,
whose real-JDK path is bytecode, so they would not have exercised it.

**N4 — `native_bb_duplicate` and `native_bb_slice` do not propagate
`isReadOnly`.** MEASURED: `rcb.duplicate().getClass()` is
`java.nio.HeapCharBufferR` and `rcb.duplicate().hasArray()` is `false`;
`scb.slice().isReadOnly()` and `scb.duplicate().isReadOnly()` are both `true`.
`native_bb_duplicate` allocates through `alloc_byte_buffer` and copies
pos/lim/cap/mark, and writes nothing to `isReadOnly` — so in synthetic-jdk mode
`readOnlyBuffer.duplicate()` is **writable**, which re-opens the same wrong
CAPABILITY this record closes, one call further along. F5-1 fixed the CharBuffer
twin of exactly this (`subSequence` was writing a flat `isReadOnly = 0`); the
ByteBuffer twin was not in its scope. Same file as this lane's fix, deliberately
left: it needs its own measured rows for `slice`, `duplicate`,
`asReadOnlyBuffer` and `compact` across the seven families, which is a probe
this lane did not run.
