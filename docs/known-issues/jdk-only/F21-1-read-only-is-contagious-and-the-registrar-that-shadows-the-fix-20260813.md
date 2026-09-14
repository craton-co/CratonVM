# F21-1 — read-only is contagious, the three copies converge, and the registrar that shadows a repair

**2026-08-13, lane F21.** Lands F14-1's three disclosed residuals: **N4** (the
wrong CAPABILITY re-opening one call later, via `duplicate()`/`slice()`), **N1**
(three copies of one rule), **N3** (the six typed families' missing
`arrayOffset`). Plus two defects found while landing them (§6) and one finding
that changes how F14-1's own §8 clearance should be read (§7).

**Provenance: MEASURED oracle, PREDICTED VM.** Every expected value below is a
pasted transcript from `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
(Microsoft build) on this host. **This lane may not build or run CratonVM**, so
every "after" is a prediction and is labelled as one. Probes:
`scratchpad/f21/F21ViewContagionProbe.java` (916 rows, the seven-family sweep)
and `scratchpad/f21/F21Rows.java` (the proposed fixture rows plus their
mutants).

Files changed: `native-io/src/lib.rs`, `native-builtins/src/servlet.rs`,
`native-builtins/src/phases_late/charset_buffers.rs`. All three are this lane's.
Everything else is a NOMINATION in §9 with exact old/new text.

---

## 1. The seven-family sweep (F14-1 N4), MEASURED

`F21ViewContagionProbe` allocates a writable buffer of 8 elements in each of
the seven families, takes `asReadOnlyBuffer()` of it, and for **both** sources
applies `duplicate()`, `slice()`, `slice(1,2)` and `asReadOnlyBuffer()`,
dumping `class`, `isReadOnly`, `position`, `limit`, `capacity`, `isDirect`,
`hasArray`, `array`, `arrayOffset`, `order` for every result.

**The six typed families' 100-row transcripts are byte-identical to one
another** — verified by mechanical diff after normalising the family name, not
inferred from the "one template" argument. `byte` differs from them in
**exactly one column**, `order` (10 rows), and in nothing else.

### 1.1 `isReadOnly` — the answer to N4

Read-only is **CONTAGIOUS** and `asReadOnlyBuffer()` is **one-way**. Identical
in all seven families:

| source | `duplicate()` | `slice()` | `slice(1,2)` | `asReadOnlyBuffer()` |
|---|---|---|---|---|
| writable | `false` | `false` | `false` | **`true`** |
| read-only | **`true`** | **`true`** | **`true`** | **`true`** |

and the derived class name carries the `R` suffix with it —
`rcb.duplicate().getClass()` is `java.nio.HeapCharBufferR`,
`rbb.slice().getClass()` is `java.nio.HeapByteBufferR`. There is **no route
back**: `ro.duplicate().asReadOnlyBuffer().isReadOnly()` is `true`, and the JDK
has no `asWritableBuffer` — nothing anywhere clears the flag. Confirmed, not
assumed.

### 1.2 The accessors on each derived buffer

Every derived buffer answers the F14-1 three-way table for its own state, so
contagion is not a cosmetic flag — it is what makes the four accessors agree:

| receiver | `isReadOnly` | `hasArray` | `array()` | `arrayOffset()` |
|---|---|---|---|---|
| `<fam>.w` | false | true | `array[8]` | `0` |
| `<fam>.w.dup` | false | true | `array[8]` | `0` |
| `<fam>.w.slice` (pos 1) | false | true | `array[8]` | **`1`** |
| `<fam>.w.slice(1,2)` | false | true | `array[8]` | **`1`** |
| `<fam>.w.aro` | **true** | false | **ROBE** | **ROBE** |
| `<fam>.ro` | true | false | ROBE | ROBE |
| `<fam>.ro.dup` | **true** | **false** | **ROBE** | **ROBE** |
| `<fam>.ro.slice` | **true** | **false** | **ROBE** | **ROBE** |
| `<fam>.ro.slice(1,2)` | **true** | **false** | **ROBE** | **ROBE** |
| `<fam>.ro.aro` | true | false | ROBE | ROBE |
| `<fam>.ro.compact()` | — | — | — | **ROBE** |

`ROBE` = `java.nio.ReadOnlyBufferException`, `getMessage()` null on every row.

### 1.3 Direct and typed views, for the `Absent` arm

| receiver | `isReadOnly` | `hasArray` | `array()` / `arrayOffset()` |
|---|---|---|---|
| `DirectByteBuffer` | false | false | UOE / UOE |
| `dbb.dup` | false | false | UOE |
| `dbb.aro` → `DirectByteBufferR` | **true** | false | **UOE** (not ROBE — array-less is asked FIRST) |
| `dbb.ro.dup`, `dbb.ro.slice` | **true** | false | UOE |
| `hb.asXBuffer()` → `ByteBufferAsXBufferB` | false | false | UOE, `getMessage()` null |
| `hb.asReadOnlyBuffer().asXBuffer()` → `…RB` | **true** | false | **UOE** |

The `…RB` row is the one that keeps F14-1's `buffer_is_read_only` reading the
FIELD rather than the method correct: those classes override the METHOD to
`true` and never write the field, and they answer `UnsupportedOperation`
because the `Absent` arm fires first.

### 1.4 Attributes that are NOT propagated (checked, so nobody re-derives them)

* **`order` is RESET on a derived ByteBuffer.**
  `allocate(8).order(LITTLE_ENDIAN).duplicate().order()` is **`BIG_ENDIAN`**,
  and the same for `.slice()` and `.asReadOnlyBuffer()`. `asIntBuffer()` is the
  exception and DOES carry it (`le.asIntBuffer().order()` is `LITTLE_ENDIAN`) —
  the same exclusion `servlet.rs`'s existing block comment records.
  The typed heap families' own `order()` is `ByteOrder.nativeOrder()`
  (`LITTLE_ENDIAN` here) and IS preserved through `duplicate()`.
* **`mark` survives `duplicate()` and is DISCARDED by `slice()`.**
  `bb.dup.reset()` restores position 3; `bb.slice().reset()` throws
  `java.nio.InvalidMarkException`.
* **`position`/`limit`/`capacity`**: `duplicate()` copies all three;
  `slice()` gives `0 / remaining / remaining`; `slice(1,2)` gives `0 / 2 / 2`.
* **Content is SHARED, not copied**, by `duplicate()`, `slice()` and
  `asXBuffer()`, in every family (`bb.dup sees write OK 42`,
  `ib.slice sees write OK 7`, `view.dup sees write OK z`). See §7.

---

## 2. What was there, and what landed — `native-io/src/lib.rs`

| site | before | HotSpot | after (PREDICTED) |
|---|---|---|---|
| `native_bb_duplicate` | copies pos/lim/cap/mark, shares `hb`, **writes nothing to `isReadOnly`** | `ro.duplicate()` is `HeapByteBufferR` | inherits the flag |
| `native_bb_slice` | copies the remaining bytes, **writes nothing to `isReadOnly`** | `ro.slice()` is `HeapByteBufferR` | inherits the flag |
| `tb_abstract_view_fns!` `$slice_fn` ×6 | same omission | contagious | inherits |
| `tb_abstract_view_fns!` `$slice2_fn` ×6 | same omission | contagious | inherits |
| `tb_abstract_view_fns!` `$dup_fn` ×6 | same omission | contagious | inherits |
| `tb_abstract_view_fns!` `$ro_fn` ×6 | flat `set_field_by_name(o,"isReadOnly",1)` | unconditional | routed through the same function |
| six typed families' `arrayOffset` | **not registered at all** | `0` / ROBE / UOE | registered, same native as ByteBuffer's |

**The severity, and why it is the same one F14-1 closed.** F14-1's fix stops
`hasArray()` → `array()` handing a caller a mutable alias to read-only storage.
Without propagation the identical capability re-opens **one call later**:
`readOnlyBuffer.duplicate()` came back with `isReadOnly` unset, so `hasArray()`
answered `true` again, `array()` handed the array over again — and because
`native_bb_duplicate` **shares** the source's backing array, it is the *same*
array. Every write through it corrupts the read-only buffer, with no exception
at any point, reached by following the documented protocol on a receiver
obtained from a read-only one.

**The fix: one decision function, eleven call sites.**

```rust
pub enum BufferDerivation { Inherit, ForceReadOnly }

pub fn buffer_view_read_only(derivation: BufferDerivation, source_read_only: bool) -> bool {
    match derivation {
        BufferDerivation::Inherit      => source_read_only,   // duplicate/slice/slice(i,l)
        BufferDerivation::ForceReadOnly => true,              // asReadOnlyBuffer, one-way
    }
}
```

with a private `buf_stamp_read_only(ctx, src, derived, derivation)` that writes
the flag on **both** outcomes. Writing the `false` case explicitly is
deliberate: neither `alloc_byte_buffer` nor `alloc_typed_buffer` writes
`isReadOnly` at all, so an unwritten flag is whatever the allocation left
behind — the explicit `0` makes "writable source ⇒ writable view" a decision
this file makes rather than a default it inherits.

`$ro_fn` was already correct and is routed through `ForceReadOnly` anyway, so
one-way-ness is a property a test can pin rather than a literal at six
expansion sites.

### 2.1 Tests

Five `#[test]`s, all on the PURE functions and none receiver-shaped — the same
reason F14-1 gives: `MockNativeContext` resolves field NAMES through a slot
fallback, so a receiver-shaped assertion about `isReadOnly` measures the mock's
name-to-slot table. Each negative row is what a **specific wrong
implementation** answers, never a negated condition:

* `derived_views_inherit_read_only_in_both_directions` — the pre-fix
  `native_bb_duplicate` (no propagation) fails the first assertion; an
  implementation that stamped *every* derived view read-only fails the second.
* `as_read_only_buffer_is_unconditional_and_therefore_one_way` — carries the
  mutant `$ro_fn`-as-a-plain-`duplicate`-alias, which is *exactly what
  `servlet.rs`'s typed-view `$ro` still is* (§9 N3), and which agrees with the
  read-only-source row and disagrees only with the writable-source one. A test
  that exercised only a read-only source would let it live.
* `exactly_one_of_the_four_derivation_cells_is_writable` — catches both the
  drop-the-flag mutant (two writable cells) and the force-everywhere mutant
  (zero) without naming either.
* `the_two_refusals_are_distinct_states_even_though_one_subclasses_the_other` —
  the CONTROL for §5.

---

## 3. F14-1 N1 — the three copies converge

`cratonvm-native-builtins` depends on `cratonvm-native-io`
(`native-builtins/Cargo.toml:57`) and not the reverse, and the edge is already
used from eleven files, so `native-io` is the only crate the other two
registrars can import. Three sets converged onto it:

| rule | copies before | after |
|---|---|---|
| the three-way `hasArray`/`array`/`arrayOffset` split | `native_io::buffer_array_access`, `charset_buffers::cb_array_access` + `CbArrayAccess`, `servlet.rs`'s open-coded `arr.is_some() && !ro` conjunction plus two `match`es | one `pub` `buffer_array_access` / `BufferArrayAccess`; `CbArrayAccess` **deleted**; `servlet.rs` routes all three accessors through it |
| the `isReadOnly` FIELD read | `bb_is_read_only`, `s2_bb_is_read_only`, `cb_is_read_only`, **and a fourth** open-coded in `charset_buffers`' `isReadOnly()Z` registration | one `pub buffer_is_read_only`; the other three are one-line delegations; the fourth calls `cb_is_read_only` |
| `new UnsupportedOperationException()` with the EMPTY message | `bb_no_backing_array`, `s2_bb_no_backing_array`, `cb_no_backing_array` | one `pub buffer_no_backing_array`; two delegations |

`bb_is_read_only` and `bb_no_backing_array` were **private**, which is why
F14-1 could not do this: the blocker was visibility inside `native-io`, not the
dependency edge. They are renamed to `buffer_is_read_only` /
`buffer_no_backing_array` and made `pub`; the `bb_` prefix was wrong anyway
since both serve the typed families too.

**The local names `s2_bb_is_read_only` / `cb_is_read_only` /
`cb_no_backing_array` are kept as delegations rather than deleted.** What F14-1
objected to is three BODIES that can drift; a delegation is one body whichever
name it wears, and keeping the names leaves ~20 call sites untouched — which
matters because this lane cannot compile. `CbArrayAccess` is the exception and
is genuinely deleted: an enum alias would leave two type names for one set of
states.

**Behaviour is neutral by construction on every one of these**, with one
exception worth naming: `charset_buffers`' `isReadOnly()Z` registration read
the field as `match … { Value::Int(v) => v, _ => 0 }` — the **raw** value —
where `cb_is_read_only`, which `hasArray`/`array`/`arrayOffset` on the same
receiver consult, normalises to 0/1. Any `isReadOnly` outside {0, 1} made
`isReadOnly()` and `hasArray()` disagree about one receiver. Nothing writes
such a value today, so this is a latent divergence closed, not a flip.

---

## 4. F14-1 N3 — the six typed families' `arrayOffset`

`arrayOffset` was registered on `java/nio/{ByteBuffer,HeapByteBuffer}` only.
The six typed families had `array` and `hasArray` and **no `arrayOffset`**, so
in `--features synthetic-jdk` mode `IntBuffer.arrayOffset()` resolved to the
Code-less `java/nio/Buffer` declaration and threw `AbstractMethodError`.

One registration per family, over the same `native_bb_array_offset`. MEASURED
oracle, identical for all six (`F21ViewContagionProbe`, and F14-1's own
`F14BufferProbe`):

```text
<fam>.w.arrayOffset        OK 0
<fam>.ro.arrayOffset       java.nio.ReadOnlyBufferException          msg=null
view.<fam>.arrayOffset     java.lang.UnsupportedOperationException   msg=null
```

F14-1 held this back because there was no fixture row behind it. §9 N1 is that
fixture row, measured and mutation-checked; three of its nineteen rows exist
only to make this registration load-bearing.

---

## 5. The correction F14-1 made, restated because it changes how to TEST

`java.nio.ReadOnlyBufferException` **extends**
`java.lang.UnsupportedOperationException` (`jdk25src/java.base/java/nio/
ReadOnlyBufferException.java:40`; MEASURED `ROBE.super OK
java.lang.UnsupportedOperationException`, `ROBE instanceof UOE OK true`,
`UOE instanceof ROBE OK false`).

F5-1's claim that the two "share no supertype below `RuntimeException`" is
false; its conclusion survives on the ORDER of the two checks, not the
hierarchy. Operationally, and this is the part that keeps costing:

* an `instanceof`-shaped or subtype-shaped assertion on this pair is
  **one-directional** and lets a wrong-class mutant live;
* a `matches!(…, RuntimeError::UnsupportedOperationException { .. })` is blind
  to the message by construction, which is how a wrong detail message survived
  a prior repair of this same family.

Every row this lane proposes compares an **exact class name**, and one row
compares `getMessage()`. The Rust-side control is
`the_two_refusals_are_distinct_states_even_though_one_subclasses_the_other`.

---

## 6. Found while landing

### 6.1 `asReadOnlyBuffer()` could return a WRITABLE buffer

`servlet.rs`'s `ByteBuffer.asReadOnlyBuffer()` storage-less arm wrote
`isReadOnly = 1` **inside** `if let Some(src_arr) = s2_bb_arr(ctx, this)`. A
receiver with no resolvable backing array therefore got a result with the flag
never written — a writable buffer from the one method whose entire contract is
that its result is not writable. The write is moved out of the `if let` (and
must stay after `bb_write_hb`, which writes `isReadOnly = 0`). MEASURED:
`asReadOnlyBuffer().isReadOnly()` is `true` for every receiver in all seven
families and there is no receiver for which it is conditional.

### 6.2 Three `ro`-dropping fallback arms in the same registrar

`servlet.rs`'s `slice()`, `slice(int,int)` and `duplicate()` each compute
`let ro = s2_bb_is_read_only(ctx, this)` and pass it to their heap and direct
arms — and their **copying fallback arm ignores it**, while running
`bb_write_hb`, which writes `isReadOnly = 0`. So the flag was not merely left
unset, it was actively cleared. All three now call a new
`s2_bb_set_read_only(ctx, buf, ro)` after `bb_write_hb`.

**Honest reachability:** on those arms the receiver is a bare 6-slot synthetic
or storage-less carrier, for which `s2_bb_is_read_only` reads no field and
answers `false`, so this is **PREDICTED a no-op today** on all three. It is
landed because "correct by accident, via a helper that answers `false` for a
layout with no flag" is exactly the shape that breaks the moment the layout
widens — which the `isReadOnly`-follow-up comment in the same file already
plans (§9 N3). §6.1 is the one that genuinely flips.

### 6.3 A sibling constant of the `native_bb_is_read_only` shape — still there

Hunting the shape F14-1 §5 names (a native returning a constant where the JDK
reads state) found one live instance, **not** in this lane's reach to fix
safely: `servlet.rs:6481`,
`r.register(cls, "isReadOnly", "()Z", |_, _| Ok(Some(Value::Int(0))))` for the
five typed view classes. Its own comment argues the constant is exact *because*
`asReadOnlyBuffer` there aliases `duplicate` and therefore cannot produce a
read-only view. That argument is **self-consistent and currently true** — and
it is the same "correct because the other half is broken" pair the JDK
transcript refutes: HotSpot's `ByteBuffer.allocate(32).asReadOnlyBuffer()
.asIntBuffer()` is a `ByteBufferAsIntBufferRB` with `isReadOnly() == true`.
Fixing it means making `$ro` real, which the comment says needs a wider
allocation than the 6-slot synthetic. Left alone deliberately; §9 N3.

---

## 7. The finding that re-reads F14-1 §8 — `register()` order, and who actually runs

`[dup nati]`, and it cuts against a cell F14-1 CLEARED.

`vm/src/vm/vm_init.rs`:

* **synthetic-jdk arm** (`if config.use_synthetic_jdk`, ~L1928–2001):
  `register_builtins` then `register_io_natives`. It does **not** call
  `register_essential_natives_with_shims`, so `servlet.rs`'s
  `register_s2_bytebuffer_essentials` never runs; `native-io` is the only
  registrar for `java/nio/ByteBuffer`.
* **real-JDK arms** (L2001-… and L2560-…):
  `register_essential_natives_with_shims` at L2055 / L2593 (which is the *only*
  call site of `register_s2_bytebuffer_essentials`), and `register_io_natives`
  **later**, at L2252 / L2788. Registration is last-write-wins, and
  `register_nio_natives` is `NativeKind::Bridge`, so `--jdk-only` does not
  refuse it.

`("java/nio/ByteBuffer", "slice", "()Ljava/nio/ByteBuffer;")` and
`("duplicate", "()Ljava/nio/ByteBuffer;")` are both in
`native_override.rs::force_native_over_real_jdk_bytecode`'s ByteBuffer list, so
they fire over real bytecode too.

**Therefore `native-io`'s `native_bb_slice` / `native_bb_duplicate` are the
live bodies in BOTH modes**, and `servlet.rs`'s aliasing implementations of the
same two descriptors — the ones whose block comment records the residual-doc
items 2/3 repair — are **shadowed**. Two consequences:

1. This lane's N4 fix had to go in `native-io` to be reachable at all. It did.
2. F14-1 §8 clears `hbb.slice().arrayOffset()` → `0` and
   `hbb.position(2).slice().arrayOffset()` → `2` as "correct". Against the
   *live* body that is **wrong**: `native_bb_slice` COPIES the remaining bytes
   into a fresh `alloc_byte_buffer`, whose `offset` is 0, so it can only answer
   `0`. MEASURED on HotSpot: `hbb.position(2).slice().arrayOffset()` is `2`,
   `hbb.slice().array()` is the **full 8-element source array** (not a
   7-element copy), and `bb.slice sees write OK 7` — a write through the source
   is visible through the slice.

Not fixed here: it is a storage-model change (share the array plus an offset,
as `servlet.rs` already does) on a body this lane cannot build or measure. §9
N4. The `isReadOnly` propagation this lane adds is orthogonal and correct
either way.

---

## 8. Swept and CLEARED — checked, measured, NOT changed

| cell | CratonVM | oracle | verdict |
|---|---|---|---|
| `charset_buffers`' `subSequence` read-only propagation | writes `isReadOnly` from the source | `scb.subSequence(...).isReadOnly()` true | F5-1's, correct, untouched |
| `servlet.rs` `slice`/`slice(II)`/`duplicate`/`asReadOnlyBuffer` **heap** and **direct** arms | carry `ro` already | contagious | correct before this lane |
| `servlet.rs` byte-order reset on the four view producers (`ord = 0`) | BIG_ENDIAN | `le.dup.order OK BIG_ENDIAN` | correct, and my §1.4 measurement independently confirms it |
| `asXBuffer()` DOES carry the order | via the `…{B,L}` class-name arm | `le.asIntBuffer().order() OK LITTLE_ENDIAN` | correct; the documented exclusion holds |
| `native_bb_is_direct` | Heap→0, Direct→1 | read-only is orthogonal to directness | correct |
| `<fam>.ro.compact()` | `s2_bb_is_read_only` guard raises ROBE | `ROBE`, `getMessage()` null | correct |
| `buffer_is_read_only` reading the FIELD not the METHOD | field | `…RB` views override the method, carry no `hb`, and answer UOE via `Absent` | correct, and §1.3 is the witness |
| ROBE detail message | `RuntimeError::ReadOnlyBufferException` → `(class, None)` | `msg=null` on all 40+ ROBE rows measured | correct |

---

## 9. NOMINATIONS

### N1 — `regression-suite/src/RJdkIntrinsics2.java`: GAP 3c, nineteen rows

Not this lane's file. **All nineteen measured green on jdk-25.0.3+9 and all
nineteen mutants die** (`scratchpad/f21/F21Rows.java`; `BASE checks=19 fail=0`,
then one run per mutant, each `fail=1`). Each mutant is what a *specific wrong
implementation* answers — the pre-fix `native_bb_duplicate` (no propagation ⇒
`array()` does not throw at all), the pre-fix `native_bb_slice`, the
unregistered typed `arrayOffset` (`AbstractMethodError`), a read-only-first
ordering, and the `"direct buffer has no backing array"` detail message.

Insert **immediately before** the existing line

```java
        // 93 -> 102: F14 added GAP 3b, the nine rows for the read-only
```

the block below; the rows use the file's existing `check`/`step`/`nameOf`/`t`/
`sink`/`OPAQUE_I` idiom, taken from GAP 3b twenty lines above.

```java
        // GAP 3c: READ-ONLY IS CONTAGIOUS. GAP 3b asks what a read-only buffer
        // answers; nothing has ever asked what a buffer DERIVED from one
        // answers. MEASURED on jdk-25.0.3+9, all seven families, identical:
        // duplicate()/slice()/slice(int,int) INHERIT isReadOnly and
        // asReadOnlyBuffer() sets it unconditionally, so there is no
        // composition of buffer operations that returns to writable.
        //
        // This is where the wrong CAPABILITY GAP 3b closes re-opens ONE CALL
        // LATER: a duplicate that lost the flag answers hasArray() == true and
        // hands out the SAME backing array the read-only buffer wraps, so every
        // write through it corrupts a read-only buffer with no exception.
        //
        // Every row compares an EXACT class name. ReadOnlyBufferException
        // EXTENDS UnsupportedOperationException, so instanceof discriminates in
        // one direction only.
        CharBuffer roDup = roArr.duplicate();
        check(roDup.isReadOnly(),
                "duplicate() of a read-only buffer is read-only — rcb.duplicate()"
                        + " is a java.nio.HeapCharBufferR. An implementation that copies"
                        + " pos/lim/cap and writes nothing to isReadOnly answers false here");
        check(!roDup.hasArray(),
                "and therefore reports hasArray() == false — this is the row that"
                        + " stops the caller being steered into array() a second time");
        step("bounds", "CharBuffer.allocate(4).asReadOnlyBuffer().duplicate().array()");
        t = null;
        try {
            sink = roDup.array().length;
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "array() on the duplicate of a read-only buffer must throw"
                        + " ReadOnlyBufferException; a duplicate that lost the flag returns"
                        + " the array and this row sees no throwable at all; got " + nameOf(t));
        check(roArr.slice().isReadOnly(),
                "slice() is contagious too — rcb.slice() is a HeapCharBufferR");
        check(roArr.slice(0, 2).isReadOnly(),
                "and so is the absolute-indexed slice(int,int) overload, which is a"
                        + " SEPARATE registration and can be fixed independently");
        check(roDup.asReadOnlyBuffer().isReadOnly(),
                "asReadOnlyBuffer() is ONE-WAY: nothing in java.nio clears the flag,"
                        + " there is no asWritableBuffer, and a read-only buffer's own"
                        + " asReadOnlyBuffer() stays read-only");
        CharBuffer wArr = CharBuffer.allocate(OPAQUE_I[6] + 1);
        check(!wArr.duplicate().isReadOnly(),
                "and it is NOT contagious upward — hcb.duplicate().isReadOnly() is"
                        + " false. An implementation that stamped every derived view"
                        + " read-only passes every row above and fails this one");
        check(wArr.duplicate().hasArray(),
                "a writable duplicate keeps its accessible array");

        ByteBuffer roBb = ByteBuffer.allocate(OPAQUE_I[11]).asReadOnlyBuffer();
        check(roBb.duplicate().isReadOnly(),
                "the ByteBuffer half of the same contract — rbb.duplicate() is a"
                        + " java.nio.HeapByteBufferR. This is the family whose natives"
                        + " share the source's backing array, so the mutable alias here"
                        + " aliases the READ-ONLY buffer's own storage");
        check(!roBb.duplicate().hasArray(), "rbb.duplicate().hasArray() is false");
        step("bounds", "ByteBuffer.allocate(8).asReadOnlyBuffer().duplicate().array()");
        t = null;
        try {
            sink = roBb.duplicate().array().length;
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "rbb.duplicate().array() must throw ReadOnlyBufferException; got "
                        + nameOf(t));
        check(roBb.slice().isReadOnly(), "rbb.slice().isReadOnly()");
        step("bounds", "ByteBuffer.allocate(8).asReadOnlyBuffer().slice().arrayOffset()");
        t = null;
        try {
            sink = roBb.slice().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "arrayOffset() on a read-only slice repeats array()'s split — a plain"
                        + " offset here is indistinguishable from a legitimate one; got "
                        + nameOf(t));
        check(!ByteBuffer.allocate(OPAQUE_I[11]).duplicate().isReadOnly(),
                "hbb.duplicate().isReadOnly() is false");

        // The three rows that make the typed families' arrayOffset registration
        // load-bearing: arrayOffset was registered for ByteBuffer only, so on a
        // VM-minted IntBuffer it resolved to the Code-less java/nio/Buffer
        // declaration and threw AbstractMethodError.
        IntBuffer wIb = IntBuffer.allocate(OPAQUE_I[6] + 1);
        check(wIb.arrayOffset() == 0,
                "IntBuffer.allocate(4).arrayOffset() is 0 — an ANSWER, not an"
                        + " AbstractMethodError from an unregistered accessor");
        step("bounds", "IntBuffer.allocate(4).asReadOnlyBuffer().arrayOffset()");
        t = null;
        try {
            sink = wIb.asReadOnlyBuffer().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "the read-only cell of the typed families' arrayOffset; got " + nameOf(t));
        step("bounds", "IntBuffer.allocate(4).asReadOnlyBuffer().duplicate().arrayOffset()");
        t = null;
        try {
            sink = wIb.asReadOnlyBuffer().duplicate().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "and the same cell reached through a DUPLICATE, which is the"
                        + " registration and the contagion in one row; got " + nameOf(t));
        step("bounds", "ByteBuffer.allocate(16).asIntBuffer().arrayOffset()");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[11]).asIntBuffer().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "a WRITABLE typed view has no array at all, so arrayOffset() answers"
                        + " UnsupportedOperation — the cell a read-only-first"
                        + " implementation gets wrong; got " + nameOf(t));
        check(t != null && t.getMessage() == null,
                "and that UnsupportedOperationException carries a NULL detail message."
                        + " This is one of only two assertions in this file that can see a"
                        + " wrong message; the CLASS being right is what let"
                        + " \"direct buffer has no backing array\" survive a prior repair");
```

and change the denominator line and its comment from

```java
        // 93 -> 102: F14 added GAP 3b, the nine rows for the read-only
        // ARRAY-BACKED cell and the writable ARRAY-LESS view cell. Re-derived
        // by running this family on jdk-25.0.3+9, not by adding 9 on paper.
        sectionEnd("bounds", 102);
```

to

```java
        // 93 -> 102: F14 added GAP 3b, the nine rows for the read-only
        // ARRAY-BACKED cell and the writable ARRAY-LESS view cell. Re-derived
        // by running this family on jdk-25.0.3+9, not by adding 9 on paper.
        // 102 -> 121: F21 added GAP 3c, the nineteen read-only-contagion and
        // typed-arrayOffset rows. Same derivation: measured, not counted.
        sectionEnd("bounds", 121);
```

**121 is MEASURED, not counted on paper.** The block above was spliced into a
scratch copy of the fixture at exactly the anchor named, compiled with
`javac`, and run on jdk-25.0.3+9:

```text
(before)  CK RJdkIntrinsics2 bounds=102   PASS RJdkIntrinsics2 (102 checks)
(after)   CK RJdkIntrinsics2 bounds=121   PASS RJdkIntrinsics2 (121 checks)
```

so the block compiles in situ, introduces no local-name collision (`roDup`,
`roBb`, `wArr`, `wIb` are all fresh; `t` and `sink` are the enclosing method's
and the file's), needs no new `import` (`ByteBuffer`, `CharBuffer` and
`IntBuffer` are already imported at lines 1/3/4), and the denominator is right.

Note F14-1 §6.2's warning: `bounds` stops at its first failure, so on CratonVM
none of these rows executes until the earlier ones pass.

### N2 — `probes/BufferAccessibleArrayProbe.java` / `.expected.txt`: no contagion rows

Not this lane's files. The expected transcript has four contagion rows
(`scb.duplicate.isReadOnly`, `scb.slice.isReadOnly`,
`scb.asReadOnlyBuffer.isReadOnly`, `rcb.duplicate.hasArray`) and they are
CharBuffer-only. The seven-family sweep is
`scratchpad/f21/F21ViewContagionProbe.java`; promoting it to `probes/` with its
916-row transcript would give the next lane §1's tables without re-measuring.

### N3 — `servlet.rs:6481`, the typed views' `isReadOnly` constant, and its cause

This one IS in a file this lane owns and is deliberately **not** taken; see
§6.3. The registration is a flat `Ok(Some(Value::Int(0)))` for
`java/nio/{IntBuffer,LongBuffer,ShortBuffer,FloatBuffer,DoubleBuffer}`, and it
is exact today only because `s2_typed_buffer_view_fns!`'s `$ro` aliases `$dup`
and so no read-only view of an s2 buffer can exist. HotSpot disagrees about the
premise: `ByteBuffer.allocate(32).asReadOnlyBuffer().asIntBuffer()` is a
`java.nio.ByteBufferAsIntBufferRB` with `isReadOnly() == true`, and
`hb.asIntBuffer().asReadOnlyBuffer()` is read-only too. Making `$ro` real needs
a read-only flag the 6-slot synthetic has no room for (slots 0..5 are
array/pos/limit/cap/byte-start-marker/segment), then `put`/`compact` guards,
then this registration reading the flag. That is a layout change with a
build-and-measure loop this lane does not have.

The same file's `s2_bb_as_int_buffer` &c. also do not propagate the source's
read-only-ness onto the view (`view.ro.asInt.isReadOnly` is `true` on HotSpot),
which is the other half of the same item.

### N4 — `native_bb_slice` copies where HotSpot aliases; F14-1 §8 cleared two cells against a shadowed body

§7. Two measured divergences that survive this lane's fix:
`hbb.position(2).slice().arrayOffset()` is `2` on HotSpot and can only be `0`
from a copy, and `hbb.slice().array()` is the full source array. The repair
already exists in `servlet.rs` (`s2_bb_new_heap_view` with a base offset) and
is shadowed by registration order; the options are to port it into
`native_bb_slice` or to reorder `register_io_natives` before
`register_essential_natives_with_shims`, and the second is a global change with
a much larger blast radius than one native.

### N5 — F5-1's §1 hierarchy claim is still uncorrected in its own file

F14-1 raised this as its N2 and it is still open: `docs/known-issues/jdk-only/
F5-1-charbuffer-accessible-array-three-way-split.md` §1 states the two
exception classes "share no supertype below `RuntimeException`". Not this
lane's file. §5 has the measurement and the replacement reason.
