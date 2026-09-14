# W7-83 — a `MemorySegment` returned as a `byte[]`, and the mock that made the screen untestable

Status: **source landed, NOT built and NOT run on CratonVM.** Every claim about
CratonVM is read from source and says so. Every claim about the real JDK is a
transcript of a Java program on Eclipse Adoptium jdk-25.0.3.9 on this Windows
host, and the probe transcript (`probes/DirectByteBufferStateProbe.expected.txt`,
282 checks, all green) is the only measured column in the record.

Branch `fix/heap-kind-mock-and-segment-as-array-20260812`, worktree
`C:/craton/CratonVM-segarr-20260812`.

> **Re-verified 2026-08-12 (lane A12), source-level, nothing run.** Three
> claims were re-checked rather than inherited: the kind screen is present in
> **both** crates (`native-builtins/src/servlet.rs` `s2_bb_arr`,
> `native-io/src/lib.rs` `bb_resolve_heap_array`, each
> `ctx.heap_kind_of(a) == ObjectKind::Array`); `asByteBuffer` **still** has no
> native registration anywhere in the workspace, so §8's finding that §4's
> Compatible-mode receiver cannot be constructed **still holds** and §4's
> `before` column is still not a measurement; and §7.2 has since been **closed
> in source** (see §7). The CratonVM probe column remains unproduced.

Input: W7-76-bytebuffer-alias-residuals.md §8.1, which found the defect, refused
to repair it, and named the reason — the repair needs a kind screen, and
`native-io`'s test mock cannot tell an array from an instance. It also
prescribed the order: **fix the mock from its own `HeapEntry::Array`
discriminant first, then add the screen.** That order is followed here and it is
the reason the record has anything to say: the screen is four lines, and the
mock is the finding.

---

## 1. The defect, measured

`javap`-free this time — the field values are read by reflection with
`--add-opens java.base/java.nio=ALL-UNNAMED`, and the accessor answers by
calling the accessors:

| receiver | runtime class | `hb` | `segment` | `address` | `hasArray()` | `array()` |
|---|---|---|---|---|---|---|
| `ByteBuffer.allocate(16)` | `HeapByteBuffer` | the array | `null` | `16` | `true` | the array |
| `ByteBuffer.allocateDirect(16)` | `DirectByteBuffer` | `null` | `null` | a pointer | `false` | throws `UnsupportedOperationException` |
| `Arena.ofAuto().allocate(16).asByteBuffer()` | `DirectByteBuffer` | `null` | **`jdk.internal.foreign.NativeMemorySegmentImpl`** | a pointer | `false` | throws `UnsupportedOperationException` |
| `Arena.ofConfined()`/`Arena.global()`, same call | `DirectByteBuffer` | `null` | `NativeMemorySegmentImpl` | a pointer | `false` | throws |
| `MemorySegment.ofArray(new byte[16]).asByteBuffer()` | `HeapByteBuffer` | the array | `null` | `16` | `true` | **the very array**, by identity |
| `MemorySegment.ofArray(a).asSlice(4, 8).asByteBuffer()` | `HeapByteBuffer` | the array | `null` | `20` | `true` | the whole 16-element parent, `arrayOffset() == 4` |
| `MemorySegment.ofArray(new int[4]).asByteBuffer()` | — | — | — | — | — | the CALL throws `UnsupportedOperationException` |

The third row is the one nobody had looked at. On it, both of CratonVM's
ByteBuffer families resolve the backing array as *`hb` by name → slot 5 →
slot 0*, and slot 5 on JDK 25 is `java.nio.Buffer.segment`. So `hb` answers
null, the slot-5 probe matches the `MemorySegment`, and it is returned as the
backing array — to `ctx.heap_element_type_of` and `ctx.get_array_element` in
`native-io`, and to a Java-visible `array()` whose declared return type is `[B`
in `native-builtins`.

This is a **wrong-KIND** read, not a wrong-slot one. The slot index is right,
the field name the site declares is right, and the read is in bounds — so it is
outside the read-side slot census's vocabulary exactly as an in-bounds
wrong-field read is outside the allocation census's (W7-69 §1), and one layer
further out again.

**Why the slot-5 probe exists and must stay.** `segment` is the only
Object-typed field `java.nio.Buffer` declares, so `native-builtins`' typed
buffer views (`s2_view_buf_fn!`) deliberately park their backing array there —
they are stamped with an abstract class that declares no `hb` to write by name.
Slot 5 therefore carries two populations, and only a kind question separates
them. A class-name question cannot: a heap reference array reports its
**component** class, which is what `object_is_array`'s own trait doc says.

---

## 2. The mock lied, and it lied about more than one thing

`native-io/src/test_support.rs`'s `MockNativeContext` allocates arrays as their
own `HeapEntry::Array` variant and has done since it was written. Nothing that
answered a *kind* question consulted it. The full population, checked method by
method against the `NativeHeapAccess` contract:

| method | was | should be | consequence |
|---|---|---|---|
| `heap_kind_of` | `ObjectKind::Object`, **unconditionally** | the entry's discriminant | any array/instance fork is untestable in either direction |
| `heap_element_type_of` | `ArrayElementType::Reference`, **unconditionally** | the array's own element type; `Reference` for a non-array (that part IS the contract) | `new_array`'s element-type argument was unobservable |
| `object_is_array` | not overridden — the trait default `false` | the entry's discriminant | the default is documented as "mock contexts **without a heap**"; this one has a heap |
| `new_array(et, n)` | `_et` — the argument was **discarded**, and every array was filled with `Value::Int(0)` | record `et`; fill the typed zero | a fresh `long[]` read back `Int(0)`, a fresh reference array read back `Int(0)` instead of `Object(None)` |
| `object_num_fields` | `0` for an `Array` entry | `0` — **correct, and left alone** | an array has no instance fields; the honest answer happens to be the stub one |
| `array_length` / `get_array_element` / `set_field` on the wrong kind | silent `0` / `Int(0)` / no-op | left alone, documented | changing these to panic is a blast radius this lane cannot measure without a build; they are now the fail-safe behind a screen a caller can actually perform |

The first four are fixed. The last two rows are recorded rather than changed,
and the reason is stated at the code.

**Why this matters more than the defect it was blocking.** Every unit test in
`native-io` runs against this mock. Three constants meant that a native asking
"is this actually an array?" received the same answer for a `byte[]` and for an
instance — so a test of any such screen passed whether or not the screen was
correct. That is the vacuous-green shape one layer below the code under test,
and it is the same species W7-69 §2.1 recorded for
`MockNativeContext::superclass_of` returning `None` unconditionally (still
true, in `native-api/src/test_mock.rs`, and untouched here).

### 2.1 What the constants were hiding, beyond this lane

`heap_kind_of` has three production consumers in `native-io` besides the new
screen, all in `process.rs`, and the old constant steered every one of them:

* `read_process_redirects` (`native-io/src/process.rs:1294`) —
  `if heap_kind_of(arr) != Array { return }`. Under the mock this branch was
  **always taken**, so everything after it was unreachable in unit tests.
* `native_process_builder_start` (`:5508`) — the `String[]` arm of
  `command`. `== Array` was **always false**, so the array arm was dead and only
  the `List` arm ran.
* the same function's `elementData` read (`:5547`) — likewise dead.

No test drives either function through the mock, so **nothing went red**; the
finding is that those three screens had no unit-test coverage available to them
at all, and now do. A lane touching `ProcessBuilder.start` should write the
`String[]` arm's test first — it is reachable for the first time.

### 2.2 Two adjacent mocks, checked

* `native-builtins/src/test_utils.rs` — **already honest**: `HeapEntry::Array`
  carries `element_type`, and `heap_kind_of`, `heap_element_type_of` and
  `object_is_array` all consult it. This is why the Compatible-mode half of the
  repair (§4) needed no mock work. It does still fill every primitive array with
  `Value::Int(0)` regardless of element type, which is the same smaller lie
  `new_array` had here.
* `native-api/src/test_mock.rs` — honest on `heap_kind_of` /
  `heap_element_type_of` (it keeps a separate `MockArray` map with the element
  type and a `default_array_value` table, copied here) but **does not override
  `object_is_array`**, so it still answers `false` for its own arrays. Left
  alone: it is a different crate's mock and no change in this lane depends on
  it. Recorded so the next lane does not re-derive it.

---

## 3. The screen — `native-io`, and which arm it reaches

`bb_resolve_heap_array` (`native-io/src/lib.rs`) now screens the slot-5 value:

```rust
if let Value::Object(Some(a)) = ctx.get_field(this, BB_SEGMENT_SLOT) {
    if ctx.heap_kind_of(a) == ObjectKind::Array {
        return Some(a);
    }
}
```

Four decisions in that shape, each of which could have been made wrong:

1. **The screen is on the VALUE, after the read.** The `read_alias::observe_read`
   above it is unmoved and unchanged. It is keyed on what slot 5 *means* on the
   loaded class, and it means `segment`, which is what the site declares — so
   W7-69 §3's deliberate non-firing control **stays non-firing** and the census
   row stays clean. Turning a clean census row into a false positive costs the
   next reader a lane, and the census's whole claim is that it can stay quiet.
   `native-api/tests/read_alias_coverage.rs::the_calibration_site_is_still_observed_before_its_own_read`
   still holds: the observation is still before its `get_field`, and the added
   lines contain no `get_field(this, BB_FIELD_ARRAY)`.
2. **It falls through rather than returning `None`.** A receiver could carry a
   segment at 5 and a synthetic array at 0; the slot-0 arm must still be reached.
3. **It is on slot 5 alone.** `hb`-by-name resolves a field the class declares as
   `byte[]`, so the VM's own typing covers it. Slot 0 is `Buffer.mark`, an `int`,
   already refused by the `Value::Object` match — and is the backing array on the
   synthetic layout, where screening it would be wrong. Slot 5 is the only index
   in this family that is a **reference field of non-array type on a real
   receiver**, so it is the only place an in-bounds, correctly-named read can
   still yield the wrong kind.
4. **`is_plausible_native_addr` is untouched.** Rejecting the segment lets
   control reach `bb_resolve_direct_address`, and on an `Arena` receiver
   `address` is a genuine process pointer — so the buffer resolves as DIRECT,
   which is what it is. The `>= 0x1_0000` screen is still what stops a heap
   buffer's `address = 16` from being dereferenced (`addr=0x10`, 51 of the 53
   crashes in the 2026-08-10 three-GC-variant H2 sweep). The repair *depends* on
   that screen staying where it is; it does not compete with it.

**Which arm.** Every caller of `bb_state` / `bb_storage_view` in `native-io` is
registered inside `register_nio_natives` (`native-io/src/lib.rs:8421–8931`,
re-walked for this record), which is `#[cfg(feature = "synthetic-jdk")]` **and**
gated on `!registry.drops_real_layout_synthetic()`. W7-76 §2 established that
`set_drop_real_layout_synthetic(true)` runs before `register_io_natives` in both
Compatible arms, so this registrar is skipped there. **This change reaches
synthetic mode only** (`--features synthetic-jdk` + `--synthetic-jdk`).

---

## 4. The same defect on the Compatible-mode winner — and that is where it bites

Found while checking the registration question rather than assuming it.
`native-builtins/src/servlet.rs`'s `s2_bb_arr` has the identical three-arm
resolution and the identical slot-5 fallback, and per W7-76 §2 **s2 is the
registrar that wins in Compatible mode** — nothing overwrites
`register_s2_bytebuffer` there. `array()[B`, `hasArray()Z` and `arrayOffset()I`
are all on `vm/src/runtime/interpreter/native_override.rs`'s forced-native list
for `java/nio/ByteBuffer`, so the native answers even though the real bytecode
is present and loaded.

So in Compatible mode, on an `Arena…allocate(n).asByteBuffer()` receiver:

| accessor | HotSpot 25.0.3.9 | CratonVM before | after |
|---|---|---|---|
| `hasArray()` | `false` | `true` | `false` |
| `array()` | `UnsupportedOperationException` | **a `MemorySegment`, where `[B` is declared** | `UnsupportedOperationException` |
| `isDirect()` | `true` | `true` (`s2_bb_direct_addr` was reached anyway) | `true` |

The `after` column is not a new branch: `array()`'s existing
`None if s2_bb_direct_addr(..).is_some()` arm already raises
`UnsupportedOperationException`, and `hasArray()` is already
`s2_bb_arr(..).is_some() && !read_only`. Rejecting the segment is what lets both
reach the answer they were already written to give. `s2_bb_heap_window` and
`s2_bb_view_backing` both route through `s2_bb_arr`, so one screen covers the
family.

**Mode per change**, stated as the frozen-Compatible rule requires:

| change | file | mode reached | why it is allowed |
|---|---|---|---|
| mock `heap_kind_of` / `heap_element_type_of` / `object_is_array` / `new_array` | `native-io/src/test_support.rs` | **test-only**, no production mode | `#![cfg(test)]` module |
| slot-5 kind screen | `native-io/src/lib.rs` (`bb_resolve_heap_array`) | **synthetic only** — `register_nio_natives` is skipped in both Compatible arms | correctness inside the mode that reaches it |
| slot-5 kind screen | `native-builtins/src/servlet.rs` (`s2_bb_arr`) | **Compatible AND synthetic** | genuine HotSpot-parity fix: `array()` returned a `MemorySegment` where `[B` is declared, and `hasArray()` answered `true` where HotSpot answers `false`. Squarely the carve-out. |
| `ObjectKind` import in each of the two crates | both | both, **no behaviour** | — |
| probe section, unit tests, this record | — | none | — |

No `CRATONVM_*` variable was added, so the four-file flag surface
(`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md`, `docs/config/flag-inventory.md`) is untouched;
`CRATONVM_DBG_LAYOUT_ALIAS` is reused as W7-69 established. No registration was
added, moved or removed, so no `NativeKind` block boundary moved and
last-write-wins is unaffected. `HEADER_SIZE` is not a factor: every access here
is by slot index, which the object model resolves relative to the header for the
caller.

---

## 5. Did any test go red?

**No, and both halves of that are worth stating.**

* `bb_get_bulk_reads_real_heap_layout_slot_hb` — the test W7-76 §8.1 named as the
  one that would break — **passes honestly now**. It stashes a real
  `ctx.new_array(ArrayElementType::Byte, 8)` at slot 5; with the mock consulting
  its own discriminant that array answers `ObjectKind::Array` and the screen
  admits it. Before the mock repair the same screen would have rejected it, for a
  reason that has nothing to do with the guard being wrong — which is precisely
  why the order was prescribed and why adding the screen first and then
  "fixing" the test would have papered over a real defect.
* The honest `heap_element_type_of` changes what `bb_state` puts in `TbView.elem`
  for a heap receiver (`Byte` now, `Reference` before). Checked: `view.elem` is
  read at exactly three places and **all three are on the `BbStorage::Direct`
  arm**, so no heap-path behaviour moves. The one test that asserts on `elem`
  (`direct_typed_view_scales_by_element_width_and_honours_order`) is a direct
  receiver and takes its `elem` from `tb_direct_element_kind`, not from the heap.
* The typed-zero fill changes what a fresh `long[]`/`float[]`/`double[]`/
  reference array reads back as. The four production sites in `native-io` that
  allocate such arrays (`file_channel.rs:377`, `lib.rs:19608`, `:19868`,
  `:20117`, `watch.rs:686`) either fill every element before any read or bound
  their reads by a separate count, and no test asserts an unfilled element.

Two new tests were added rather than any relaxed:

* `native-io/src/lib.rs::mock_answers_object_kind_from_its_own_heap_discriminant`
  — the mock's honesty, asserted **before** anything depends on it, and asserted
  in pairs (array *and* non-array, each element type *and* its default value)
  because a one-sided assertion is exactly what a constant satisfies.
* `native-io/src/lib.rs::bb_state_refuses_a_memory_segment_at_the_segment_slot`
  and `native-builtins/src/servlet.rs::s2_bytebuffer_refuses_a_memory_segment_as_a_backing_array`
  — each carries its control arm **in the same test**, so a screen that refuses
  everything at slot 5 fails. The `native-builtins` one drives the registered
  `array()`/`hasArray()` natives rather than the helper, i.e. the pair a Java
  caller actually reaches.

---

## 6. The probe

`probes/DirectByteBufferStateProbe.java`, extended rather than competed with.
**233 checks before, 282 now, all green on HotSpot 25.0.3.9**, all exact values.
The new `segmentBuffer` section is appended after the five existing ones, so the
first 233 lines of `probes/DirectByteBufferStateProbe.expected.txt` are
byte-identical and the expected file diffs purely additively.

The section is written against the vacuous shape it would otherwise take.
Asserting that `hasArray()` returns *a boolean* passes against either answer, so
every row states the exact value on **both** arms, and `array()` is driven
through real `invokevirtual java/nio/ByteBuffer.array:()[B` — the
`throwName(() -> b.array())` harness, which reports `NO-THROW:<value>` when a VM
returns instead of throwing, so a fabricated array or a returned `MemorySegment`
shows *what it returned*.

| rows | what they separate |
|---|---|
| `seg.native.*` (13) | an `Arena` segment: `hasArray() == false`, both accessors throw `UnsupportedOperationException`, and the storage is still real (`put`/`get` round-trips and aliases the segment, `getInt` decodes) — so a VM cannot pass by answering "no storage" |
| `seg.native.slice/readOnly.*`, `seg.nativeRO.*`, `seg.confined.*` (13) | a derived view, a read-only view, a read-only segment and a confined arena all answer the same; the read-only rows keep W7-58's measured split (`UnsupportedOperationException`, **not** `ReadOnlyBufferException`) |
| `seg.heap.*` (10) | `MemorySegment.ofArray(byte[])`: `hasArray() == true` and `array()` is the very array **by identity** — the arm that fails if the screen refuses everything at slot 5 |
| `seg.heapSlice.*` (8) | capacity 8, `arrayOffset() == 4`, `array()` the whole 16-element parent by identity, and a write through it aliases the backing array. A native that answers `array()` with a fresh right-sized copy passes every other row here and fails these |
| `seg.heapRO.*` (4) | a read-only HEAP segment splits the other way — `ReadOnlyBufferException` — measured, not assumed |
| `seg.intArray.asByteBuffer.throws` (1) | the JDK refuses to mint a ByteBuffer over an `int[]` segment at all, rather than minting one whose `array()` would be an `int[]`. The same species of refusal as this lane's screen, one layer up |

**Where to run it**: both arms answer different questions and both are worth
running. Default Compatible mode exercises real JDK bytecode plus the s2 family
— the arm §4's fix is for, and the only arm where the `array()` rows can
currently be wrong. `--features synthetic-jdk` + `--synthetic-jdk` exercises the
`native-io` family and §3's fix.

---

## 7. What this lane could not resolve

1. **Runtime confirmation of anything on CratonVM.** Nothing was built or run.
   The probe's oracle is measured on HotSpot; its CratonVM column has still never
   been produced — for this lane, W7-76, W7-69 or W7-58. That column is still the
   single highest-value next step and it is one command in each mode.
   **Qualified 2026-08-12 by §8**: the `seg.*` section of that column cannot be
   produced at all until `MemorySegment.asByteBuffer()` is registered, and the
   part of it a scheduled fixture *can* answer is §7.1, which §8.1 now schedules.
2. ~~**`arrayOffset()` on a direct receiver.**~~ **CLOSED in source
   2026-08-12** — verified by this lane, not assumed. `s2`'s `arrayOffset`
   (`native-builtins/src/servlet.rs`, the `r.register(bb, "arrayOffset",
   "()I", …)` block) now carries both refusals, transcribed from the `array()`
   arm exactly as §8.1 prescribed: `ReadOnlyBufferException` on a read-only heap
   receiver, `UnsupportedOperationException` on a direct one, and the
   storage-less synthetic keeps its historic benign zero. The comment at the
   site cites W7-83 §7.1 and the measured oracle rows. **Still not run on
   CratonVM** — this is a source-level verification that the prescribed body
   landed, not a green probe column.
3. **`native-api/src/test_mock.rs::object_is_array`** still answers `false` for
   its own arrays (§2.2), and its `superclass_of` still answers `None`
   unconditionally (W7-69 §2.1).
4. **`native-builtins`' mock still fills every primitive array with `Int(0)`**
   regardless of element type (§2.2) — the smaller half of the lie fixed here.
5. **The three `ProcessBuilder` kind-screens are now testable and still
   untested** (§2.1). This is the largest thing the mock repair unblocks.
6. **The cross-kind fail-safes in `native-io`'s mock** — `array_length` and
   `get_array_element` on an instance, `set_field` on an array — still answer a
   silent default rather than failing. Documented at the code; changing them to
   panic needs a build to price.
7. Everything W7-76 §9 left open is still open, minus its item 4, which is this
   record.

---

## 8. §4's Compatible-mode receiver cannot be CONSTRUCTED today (2026-08-12)

§4's table is labelled a source-level prediction, and it is one. Verifying it —
rather than inheriting it — found that the row it predicts is currently
**unreachable**, and the reason is one grep away.

**`MemorySegment.asByteBuffer()` has no native registration in either crate.**
Searched tree-wide: the string `asByteBuffer` appears only in comments (in
`native-builtins/src/{servlet.rs,charset.rs}` and `native-io/src/lib.rs`) and in
this campaign's records. It is also on no forced-native list. So:

| receiver | route in Compatible mode | can `asByteBuffer()` answer? |
|---|---|---|
| `Arena.ofAuto().allocate(16)` | `Arena.ofAuto` → `p67_new_arena`, `allocate` → `panama::pe_arena_allocate` → a **synthetic** 6-slot object stamped with the `java/lang/foreign/MemorySegment` INTERFACE | **No.** `asByteBuffer` is abstract on the interface, nothing declares `Code`, and there is no native. This is the shape §6.3 already records for `copyFrom` (`AbstractMethodError: … has no Code attribute`), and it is almost certainly why the probe's section `G` "produced **no** output at all" |
| `MemorySegment.ofArray(byte[])` | `ofArray([B)` has **no** registration either, so the JDK's own static interface method runs → a genuine `HeapMemorySegmentImpl$OfByte` (whose `scope` holds one of OUR sessions, as W7-89 §4.2(2) measured) | Yes — real bytecode, giving a real `HeapByteBuffer` with `hb` set and `segment` null |

So the only receiver that puts a non-null `MemorySegment` in slot 5 of a real
`java.nio.Buffer` is the one CratonVM cannot mint. **Nothing in this VM currently
writes `Buffer.segment` on a real-layout ByteBuffer at all** — `bb_write_hb`
writes seven named fields and `address`, none of them `segment`, and the indexed
slot-5 write is gated behind `s2_bb_synthetic_layout`, which is false on the real
layout by construction (§6 of W7-76).

**What this does and does not change:**

* The screen in `s2_bb_arr` is **correct and not wasted**. It is the guard that
  makes the row safe *when* a receiver of that shape appears — and the shape
  arrives the moment anyone registers `asByteBuffer`, which §6.3's `copyFrom`
  finding says is a live gap. Landing the screen before the receiver exists is
  the right order, and it is the opposite mistake from the one W7-89 §5.3
  warns about.
* **§4's `before` column is not a measurement and cannot become one on today's
  tree.** Read it as "what would have happened", not "what happened". The
  `native-io` half (§3) is unaffected by this: its receivers are
  `native-builtins`' typed views, which genuinely do park an array at slot 5, and
  that population exists in synthetic mode.
* **No scheduled fixture can assert the `seg.*` battery.** Every `seg.native.*`
  and `seg.confined.*` row in the probe needs `Arena…asByteBuffer()`, which
  raises before the first assertion; the `seg.heap*` rows would work but they are
  the arm that was already green, so they discriminate nothing. §7.1 is
  therefore **the only part of this record a scheduled fixture can reach**, and
  it is what the fixture below asserts.

### 8.1 §7.1 — the prescription, and the assertions that pin it

`arrayOffset()` is three lines in the JDK and the two throws are the whole of
what a VM gets wrong, because the happy path is a plain field read that comes out
right by accident:

```java
public final int arrayOffset() {
    if (hb == null) throw new UnsupportedOperationException();
    if (isReadOnly) throw new ReadOnlyBufferException();
    return offset;
}
```

`array()` in the same file (`native-builtins/src/servlet.rs`, the
`r.register(bb, "array", "()[B", …)` block) already has exactly that shape.
`arrayOffset` is one line and has none of it. **Out-of-file for the lane that
found this** (`servlet.rs` is another lane's file); the replacement body is a
transcription of `array()`'s match with `Value::Int(s2_bb_heap_base(..))` in
place of the array, and it keeps the storage-less synthetic's historic benign
zero for the same reason `array()` keeps its benign null.

The scheduled assertions are `RDirectBufferElem.arrayOffsetContract()` — group 7
of `regression-suite/src/RDirectBufferElem.java`, which is in `CORE_CLASSES`
already, so there is no `run.sh` change. 26 checks, each value copied from
`probes/DirectByteBufferStateProbe.expected.txt`:

| assertion | oracle row | before |
|---|---|---|
| `direct arrayOffset()` → `UnsupportedOperationException` **exactly** | `direct.arrayOffset.throws` | returns `0` |
| `direct window arrayOffset()` → same | `direct.win.arrayOffset.throws` | returns `0` |
| `direct read-only arrayOffset()` → same | `direct.win.readOnly.arrayOffset.throws` | returns `0` |
| `heap read-only arrayOffset()` → `ReadOnlyBufferException` **exactly** | `heap.win.readOnly.arrayOffset.throws` | returns `4` |
| `a fresh heap buffer's arrayOffset is 0` | `heap.arrayOffset = 0` | green — the guard |
| `a heap window's arrayOffset is 4`, `…array is the parent's, by identity`, `…array is the PARENT's 16 elements` | `heap.win.arrayOffset`, `heap.win.array.identity`, `heap.win.array.length` | green — and these are the rows a fabricated right-sized copy fails |
| every `array()` row and every `get(0)` row beside them | `*.array.throws`, `*.win.get0` | green — the over-correction arm: a fix that refuses everything satisfies the four red rows and fails these |

`checkThrowsExactly` is a new helper and is not decoration:
`ReadOnlyBufferException extends UnsupportedOperationException`, so the
subclass-tolerant `checkThrows` the file already had would pass a heap read-only
`ReadOnlyBufferException` against an expectation of the wider type — making the
measured heap/direct split unobservable in exactly the direction §6's
`seg.heapRO` rows exist to pin.
