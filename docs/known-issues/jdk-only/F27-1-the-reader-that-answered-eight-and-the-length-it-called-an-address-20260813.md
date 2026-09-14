# F27-1 — the layout reader that answered eight, the buffer libffi wrote past, and the length that was handed out as an address

**2026-08-13, lane F27.** Lands F16-1 NOM-1 and NOM-2, and closes W7-89 §7.1 by
naming a different cause than the one that record names. Patches the two files
this lane owns:

* `native-builtins/src/panama_libffi.rs`
* `vm/src/runtime/interpreter/native_override.rs`

Both patches are in the working tree, uncommitted.

**This lane may not build or run CratonVM, and did not.** Every JDK fact below
is `javap` / `java` on this host (Microsoft build **25.0.3+9-LTS**) or the JDK
source at `C:\craton\jdk25src`, quoted at the point it is used. Every claim
about CratonVM's behaviour — before and after — is **PREDICTED** from source.
Both files were parse-checked (`rustfmt --edition 2021 --emit stdout` on
scratch copies, exit 0 for each), which rules out syntax errors and nothing
else; neither was type-checked.

**Line endings.** Both files are CRLF and stayed CRLF: `panama_libffi.rs`
1754/1754 CR/LF, `native_override.rs` 7930/7930, checked with
`tr -cd '\r' < f | wc -c` against `tr -cd '\n' < f | wc -c` as F16-1 §0
instructs. `grep -c $'\r'` was not used; it cannot go red in this shell. This
record is LF, matching every other file in this directory (measured: F16-1 is
0/538, F21-1 is 0/602).

---

## 0. Verdict

| claim | verdict |
|---|---|
| F16-1 NOM-1 part 1: `read_layout_kind` misses the real JDK's `ValueLayouts$Of*Impl` names | **CONFIRMED AND FIXED**, and it is worse than "an eight-byte read where four were meant": in `--jdk-only` those are the ONLY value layouts there are, so **every** one was `LAYOUT_LONG` — including `JAVA_FLOAT`, which then travelled in an integer register (§1) |
| F16-1 NOM-1 part 2: `layout_total_size`/`layout_align` answer 8/8 for every group layout | **CONFIRMED AND FIXED.** The `LAYOUT_STRUCT \| …` arm below the `kind < 10` early return was **unreachable**, and read the wrong slot anyway (§2) |
| **NOM-1 as written is incomplete** | **NEW.** Three more readers in the same file decode the carrier by stale slot index — the union's size, the sequence's element count and a padding member's width all read **slot 1, which is the ALIGNMENT**. Landing only the three functions NOM-1 names would have made the ffi type and the buffer disagree by MORE, not less (§3) |
| the FFI clamp in `panama.rs` | **NOT LIFTED, DELIBERATELY, and it was never the severe half.** It bounds an `unsafe copy_nonoverlapping` whose two sizes are computed in two different files; it stops truncating the moment this lands, and removing it would make the `unsafe` block's safety argument non-local. Comment-only NOMINATION (§4) |
| the actual severe defect on that path | **NEW, AND IT IS AN OUT-OF-BOUNDS WRITE THAT EXISTS TODAY.** `alloc_return_slot` sized every aggregate return buffer at **8 bytes** and hands its pointer to `ffi_call`, which writes `rtype->size` — the full C size of the aggregate (§4) |
| F16-1 NOM-2: four stale force-route entries, listed twice | **CONFIRMED AND DELETED**, both copies, each verified against `javap` and against every registration in the workspace. **And the "listed twice" is worse than a duplicate table: both copies are inside ONE function**, so the second copy is now deleted whole (§5) |
| W7-89 §7.1's fatal SIGSEGV at `0x10` | **DIAGNOSED, AND §7.1 NAMES THE WRONG CAUSE.** It is not a `Buffer.address` read as a pointer. `segment_address` answered a real heap segment's **byteSize**, and `0x10 == 16 == new byte[16].length` (§6) |

---

## 1. `read_layout_kind` — the drifted twin, and which mode it broke

### 1.1 What it was

```rust
Value::Long(_) => {
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(layout));
    match class_name.as_deref() {
        Some("java/lang/foreign/ValueLayout$OfByte") => LAYOUT_BYTE,
        …nine rows…
        _ => LAYOUT_LONG,
    }
}
```

Nine exact strings, all in the `java/lang/foreign/` spelling. Measured, the
real JDK's carriers are not:

```
$ java F27Probe                                   # 25.0.3+9-LTS
JAVA_INT   -> byteSize=4 byteAlignment=4 class=jdk.internal.foreign.layout.ValueLayouts$OfIntImpl   toString=i4
JAVA_FLOAT -> byteSize=4 byteAlignment=4 class=jdk.internal.foreign.layout.ValueLayouts$OfFloatImpl toString=f4
ADDRESS    -> byteSize=8 byteAlignment=8 class=jdk.internal.foreign.layout.ValueLayouts$OfAddressImpl toString=a8
JAVA_INT_UNALIGNED -> byteSize=4 byteAlignment=1 class=…$OfIntImpl toString=1%i4
```

None of the nine matched, so **every real value layout decoded as
`LAYOUT_LONG`.**

### 1.2 Why `--jdk-only` is the mode where that is total

`vm/src/vm/vm_util.rs::make_prepared_value_layout` — the preseed that fabricates
`java/lang/foreign/ValueLayout$Of*` objects — carries its own note:

```
// JDK-ONLY-LAYOUT: converted (step 3) — this whole function is now
// unreachable under `CompatibilityMode::JdkOnly`
…
// Wave-2 requirement, DONE 2026-08-10: under `CompatibilityMode::JdkOnly`
// the preseed is dropped entirely and the real `ValueLayout.<clinit>` runs
```

So under `--jdk-only` the `java/lang/foreign/…` spellings this list matched
**are not minted at all**, and the ones that are minted are exactly the nine it
missed. Independent corroboration in the same tree:
`phases_late/foreign_ffm.rs:3260-3347` registers `withName`/`withOrder`/
`carrier`/`order`/`name` on all nine `jdk/internal/foreign/layout/
ValueLayouts$Of*Impl` class names, and `native_override.rs:3853` force-routes
`class_name.starts_with("jdk/internal/foreign/layout/ValueLayouts$")`. Two
places in the VM know those objects arrive; the layout decoder did not.

### 1.3 What that cost, per consumer

| call site | with `LAYOUT_LONG` | correct |
|---|---|---|
| `pe_segment_get_impl` (`MemorySegment.get(JAVA_INT, off)`) | an **8-byte** read returned as `Value::Long` into an `I` slot | 4-byte `Value::Int` |
| `pe_segment_set_impl` (`set(JAVA_INT, off, v)`) | an **8-byte** write | 4-byte |
| `pe_segment_get_at_index` / `setAtIndex` | stride **8** | 4 |
| `marshal_arg(JAVA_FLOAT, …)` | `i64` slot, and `layout_to_ffi_type` says `i64` too — so the value goes in an **integer register** on every SysV/Win64 ABI | `f32` in an SSE register |
| `MemorySegment.get(JAVA_BYTE, 0)` on a 1-byte segment | `pe_segment_access_addr` bounds-checks width 8 against size 1 and **raises `IndexOutOfBoundsException`** | reads the byte |

The width-8 bounds check is the one mercy here: the wrong width is caught as an
out-of-bounds *refusal* on small segments rather than an over-read. On a large
segment it is a silent wrong answer.

### 1.4 The fix: delegate, do not extend

`p67_layout_carrier_name` (`foreign_ffm.rs:84`) already matches both spellings —
`class_name.contains("OfInt")`, and `ends_with("ValueLayouts$OfAddressImpl")` for
the address case — and `p67_layout_render` already uses it to produce the JDK's
`toString` letters. `read_layout_kind` now delegates to it through a new
`layout_kind_of_class`, so the two name-matchers in this codebase are ONE.
Adding nine more strings would have made the drift wider, not narrower.

Group, sequence and padding classes are matched first, one `contains` per kind,
covering both spellings at once — measured:

```
struct(LONG,INT)  class=jdk.internal.foreign.layout.StructLayoutImpl
union(INT,LONG)   class=jdk.internal.foreign.layout.UnionLayoutImpl
seq(10,INT)       class=jdk.internal.foreign.layout.SequenceLayoutImpl
pad(3)            class=jdk.internal.foreign.layout.PaddingLayoutImpl
```

against this VM's `java/lang/foreign/StructLayout` etc. from
`foreign_ffm.rs`'s four factories.

### 1.5 `_ => LAYOUT_LONG` is gone, and the replacement is loud

`LAYOUT_UNKNOWN` is a new constant in `panama_libffi.rs`, and every consumer in
that file turns it into an `IllegalStateException` naming the class and the
`Value` found in slot 0.

**It is `-2`, not `-1`, on purpose.** `panama.rs:3583` already uses `-1` as its
VOID sentinel in the upcall-stub builder (`None => -1`). Nothing can currently
reach the confusion — `layout_to_ffi_type` refuses an unknown carrier on the
very next line — but a sentinel colliding with another sentinel is a defect
waiting for a reorder.

---

## 2. `layout_total_size` / `layout_align` — the arm that could not run

The old bodies:

```rust
pub fn layout_total_size(…) -> Result<usize, MethodCallFailed> {
    let kind = read_layout_kind(ctx, layout);
    if kind < 10 { return Ok(layout_byte_size(kind)); }        // <- always taken
    match kind {
        LAYOUT_STRUCT | LAYOUT_UNION | LAYOUT_SEQUENCE | LAYOUT_PADDING =>
            match ctx.get_field(layout, 1) { … }               // <- unreachable
        _ => Ok(0),
    }
}
pub fn layout_align(…) -> usize {
    let kind = read_layout_kind(ctx, layout);
    if kind < 10 { return layout_alignment(kind); }
    match ctx.get_field(layout, 5) { … _ => 1 }
}
```

Two independent defects stacked, which is why the second one had never been
observed:

1. **`kind` was never ≥ 10** for a group layout, because §1's list had no group
   name. `layout_byte_size(LAYOUT_LONG)` is 8 and `layout_alignment(LAYOUT_LONG)`
   is 8, so the answer for `structLayout(JAVA_LONG, JAVA_INT)` — measured 12/8 —
   was **8/8**.
2. Had it been reachable, `get_field(layout, 1)` is the **byteAlignment** since
   F16's consolidation, and `get_field(layout, 5)` is not a field of any layout
   carrier this VM mints, nor of a real `AbstractLayout`. So the size would have
   been the alignment and the alignment would have been the `_ => 1` default.

Both now read the carrier head directly, which is also the real JDK's own field
order:

```
$ sed -n '52,54p' jdk25src/java.base/jdk/internal/foreign/layout/AbstractLayout.java
    private final long byteSize;
    private final long byteAlignment;
    private final Optional<String> name;
```

so `[0]` and `[1]` serve a CratonVM carrier and a real JDK layout with the same
two reads, and no class-name lookup is needed at all for the common case. The
kind-tagged fallback survives for `panama.rs`'s `#[cfg(test)] pe_make_layout`
fixture, which is the last minter of an `Int`-slot-0 carrier.

`layout_total_size` now REFUSES a negative `byteSize` and a group carrier with
no size in slot 1, instead of answering `Ok(0)`. `Ok(0)` reaches
`marshal_arg`'s `total == 0` check, which refuses — but it also reaches
`alloc_return_slot`, where it was silently widened to 8.

---

## 3. NOM-1 as written is incomplete — three more stale slot reads

**This is the finding that changes how the nomination should be landed.**
NOM-1 names three functions. Correcting only those three would leave
`layout_to_ffi_type` building an ffi type from a *different* — and now
differently wrong — set of numbers than the buffers around it, which is a worse
state than before, not a better one.

| site | read | slot 1 actually holds | measured consequence |
|---|---|---|---|
| `layout_to_ffi_type`, `LAYOUT_UNION` arm | `get_field(layout, 1)` as the union's **total size** | byteAlignment | `unionLayout(JAVA_BYTE, sequenceLayout(7, JAVA_BYTE))` is byteSize=7 **byteAlignment=1** → a ONE-byte ffi type for a seven-byte union. `unionLayout(sequenceLayout(7, JAVA_BYTE), JAVA_INT)` is byteSize=7 **byteAlignment=4** → a four-byte one |
| `layout_to_ffi_type`, `LAYOUT_SEQUENCE` arm | `get_field(layout, 1)` as the **element count** | byteAlignment | `sequenceLayout(10, JAVA_INT)` is byteSize=40 **byteAlignment=4** → a FOUR-element ffi struct for a ten-element sequence |
| `layout_struct_to_ffi_type`, padding member | `get_field(member, 1)` as the padding **width** | byteAlignment, always **1** for padding | `paddingLayout(3)` is byteSize=3 **byteAlignment=1** → one filler `i8` instead of three, so `structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT)` — the padded form the JDK *requires* — built `{i8, i8, i32}` and every member after the padding sat at the wrong frame offset |

All four "measured" rows above, verbatim from the oracle:

```
$ java F27Probe                                            # 25.0.3+9-LTS
pad(3)                     -> byteSize=3  byteAlignment=1  [x3]
seq(10,INT)                -> byteSize=40 byteAlignment=4  [10:i4]
seq(7,BYTE)                -> byteSize=7  byteAlignment=1  [7:b1]
union(INT,LONG)            -> byteSize=8  byteAlignment=8  [i4|j8]
union(BYTE,INT)            -> byteSize=4  byteAlignment=4  [b1|i4]
union(BYTE,seq(7,BYTE))    -> byteSize=7  byteAlignment=1  [b1|[7:b1]]
union(seq(7,BYTE),INT)     -> byteSize=7  byteAlignment=4  [[7:b1]|i4]
struct(LONG,INT)           -> byteSize=12 byteAlignment=8  [j8i4]
struct(BYTE,pad3,INT)      -> byteSize=8  byteAlignment=4  [b1x3i4]
JAVA_INT_UNALIGNED         -> byteSize=4  byteAlignment=1  1%i4
```

**Note the two union rows that were already in the tree's tests**:
`union(INT,LONG)` is 8/8 and `union(BYTE,INT)` is 4/4 — size and alignment
coincide in both, which is exactly why reading the alignment as the size passed
every test there was. A new test `a_unions_size_is_not_its_alignment` pins the
7/4 row for that reason.

The sequence's element count is now derived — `byteSize / element.byteSize()` —
which is the same reconstruction `p67_layout_render` uses to print `[10:i4]`,
so there is one rule and not two.

**One more, found while checking whether the group arms are safe on a real JDK
carrier and deliberately NOT repaired.** `layout_struct_to_ffi_type` reads the
member ARRAY from slot 2. A real `StructLayoutImpl` does not have that shape:

```
$ javap -p jdk.internal.foreign.layout.AbstractGroupLayout
  private final jdk.internal.foreign.layout.AbstractGroupLayout$Kind kind;
  private final java.util.List<java.lang.foreign.MemoryLayout> elements;
  final long minByteAlignment;
```

`AbstractLayout` declares `byteSize, byteAlignment, name`, so a real group
layout's slot 2 is the **name**, and its members are a `java.util.List`, not an
array — `ctx.array_length` on it is meaningless. It now refuses BY NAME rather
than silently building an empty or garbage member set. It should not arise:
`structLayout`/`unionLayout` are on the force-route list (§5), so every group
layout Java code builds is one of ours; JDK-internal code that builds one
directly can still produce one, and a named refusal is the honest answer.

---

## 4. The clamp: not lifted, and it was never the severe half

### 4.1 Where the clamp is, and why it stays

`native-builtins/src/panama.rs:2932` — **not** in either of this lane's files:

```rust
let copy_len = total.min(ret_slot.len());
```

`total` comes from `p67_layout_size_of` (`foreign_ffm.rs`); `ret_slot.len()`
comes from `alloc_return_slot` (`panama_libffi.rs`). **Two files, two size
functions, one `unsafe copy_nonoverlapping` between them.** The clamp is the
local proof that the copy is in bounds.

After this lane's fix `ret_slot.len() >= total` always — `alloc_return_slot`
sizes from the same carrier and then rounds UP (§4.2) — so the clamp becomes a
no-op and **stops truncating**. That is the entire behavioural change F16-1 §9.6
asked for.

It stays because removing it would make the safety of an `unsafe` block depend
on two functions in two crates' worth of file agreeing, with nothing at the site
to say so. **A bound that is currently a no-op is not dead code when it guards
an `unsafe` write.** What is stale is the comment above it, which says an
aggregate return wider than 8 bytes is truncated "until NOM-1 lands". NOM F27-1
below is comment-only.

### 4.2 The severe defect on that path is on the OTHER side, and it is live

`alloc_return_slot` sizes the buffer that `panama.rs:2841` hands to `ffi_call`
as its result address:

```rust
let result_ptr = … ret_slot.as_mut_ptr() as *mut std::ffi::c_void;
libffi::raw::ffi_call(raw_cif_ptr, …, result_ptr, …);
```

libffi writes `cif->rtype->size` bytes there. For an aggregate that size is
computed by `ffi_prep_cif` under C rules. **Before this lane
`layout_total_size` answered 8 for a group layout of any width**, so
`size = 8.max(8)` = 8 and the `Vec` was eight bytes — while libffi wrote the
whole struct. **That is an out-of-bounds WRITE past a heap allocation on every
by-value aggregate return wider than eight bytes**, and it is present in the
tree today. It is a strictly more severe defect than the truncation the clamp
was introduced to avoid, and it was invisible because both halves were wrong in
the same direction.

Correcting the size alone is not sufficient, because **the C size of an
aggregate is not the JDK's `byteSize`**: C rounds a struct's total up to the
struct's alignment and the JDK does not. Measured:
`structLayout(JAVA_LONG, JAVA_INT).byteSize()` is **12**; the C type
`struct { long; int; }` is **16**. Sizing the buffer at 12 would hand libffi
twelve bytes and let it write sixteen.

`alloc_return_slot` therefore allocates
`align_up(byteSize, byteAlignment).max(byteSize).max(size_of::<usize>())`. The
rounding can only ever ENLARGE the buffer, so it cannot introduce an
out-of-bounds write; and the `MAX_STRUCT_BYTES` ceiling still applies. Test:
`aggregate_return_slot_covers_the_c_size_not_just_the_jdk_byte_size` asserts
**16** for the 12/8 layout.

**Honest limit.** The reasoning "libffi's aggregate size is
`align_up(jdk_size, jdk_align)`" is derived from the C ABI and from the fact
that `structLayout` now REFUSES any layout whose members would need interior
padding (F16-1 §3.1), so libffi's per-element `ALIGN` steps are provable no-ops
and only its final round-up differs. It is not measured against libffi, and it
cannot be without a build. It is safe under-approximation-free in one direction
only: the buffer is never smaller than either candidate size.

---

## 5. F16-1 NOM-2 — the four stale entries, and the duplicate that was worse than a duplicate

### 5.1 Verified against `javap`, then against every registration

```
$ javap -p java.lang.foreign.MemoryLayout                  # 25.0.3+9-LTS
  public static java.lang.foreign.PaddingLayout  paddingLayout(long);
  public static java.lang.foreign.SequenceLayout sequenceLayout(long, java.lang.foreign.MemoryLayout);
  public static java.lang.foreign.StructLayout   structLayout(java.lang.foreign.MemoryLayout...);
  public static java.lang.foreign.UnionLayout    unionLayout(java.lang.foreign.MemoryLayout...);
```

A call site's descriptor is the resolved method's own descriptor, so no
classfile can name `…)Ljava/lang/foreign/MemoryLayout;` for any of the four.
And no registration answers it:

```
$ grep -rn '"(\[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"' \
       '"(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"' \
       'paddingLayout", "(J)Ljava/lang/foreign/MemoryLayout;"' --include=*.rs .
  (only vm/src/runtime/interpreter/native_override.rs, twice)
```

They were `panama.rs`'s rows and F16 deleted them. **Removing a force-route
entry IS a behaviour change** — it decides whether a registered native shadows
real JDK bytecode — but here the triple has neither a call site nor a
registration, so the predicate could only ever have cost a fruitless registry
probe for a call that cannot occur.

`withName(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;` **stays**:
`javap` shows `public abstract java.lang.foreign.MemoryLayout
withName(java.lang.String);`, so that is the real descriptor and not an erased
twin. The JDK-true spellings of all four factories stay, which is what keeps the
`SharedUtils.<clinit>` bootstrap-cycle escape the comment describes.

No test asserts any of the four (`vm/src/runtime/interpreter/tests.rs`
`ffm_memory_layout_force_native_covers_varhandle` and
`ffm_group_layout_force_native_covers_member_layouts` are the two that touch
this predicate; neither names an erased return).

### 5.2 "Listed twice in the file" is understating it — both copies are in ONE function

F16-1 NOM-2 says the same four "appear twice in the file (`:1807-1821` and
`:3822-3836`), which is itself worth a look: two copies of one table." Measured,
it is sharper than that:

* `is_ffm_memory_layout_native_override` (`:1794`) is called from
  `force_native_over_real_jdk_bytecode` at `:4370`.
* The inline `matches!` at `:3814` is **inside `force_native_over_real_jdk_bytecode`
  itself** (`:2466`–`:4793`).

So one function tested nine of the same triples twice, several hundred lines
apart, both arms returning `true`. The inline copy is now deleted whole and its
rationale moved to the helper. **Verified behaviour-neutral**: the helper is a
strict superset (it adds `name` and `withName`), and the only `return false`
inside `force_native_over_real_jdk_bytecode` is at `:2743`, *before* both sites,
so nothing between them could have short-circuited.

---

## 6. W7-89 §7.1 — the crash, and the cause that record does not name

§7.1 records, as "found in passing, not fixed":

> `MemorySegment.ofArray(new byte[16]).set(JAVA_INT_UNALIGNED, 0, 7)` takes
> `EXCEPTION_ACCESS_VIOLATION (0xC0000005) … read at address 0x10` and kills the
> process. `addr=0x10` is the signature the memory index already carries for a
> `Buffer.address` read as a pointer.

**It is not that signature. `0x10` is 16, and 16 is `new byte[16].length`.**

`segment_address` (`panama_libffi.rs`) resolves `min` by name first — a fix
whose own comment explains that on a real segment "field 0 there is the
segment's BYTE LENGTH, not its address", confirmed by a live gdb capture of a
`posix_madvise` downcall passed `276`. That fix pinned only the positive half.
Measured:

```
$ javap -p jdk.internal.foreign.AbstractMemorySegmentImpl
  final long length;  final boolean readOnly;  … scope
$ javap -p jdk.internal.foreign.HeapMemorySegmentImpl
  final long offset;  final java.lang.Object base;
$ javap -p jdk.internal.foreign.NativeMemorySegmentImpl
  final long min;
```

`HeapMemorySegmentImpl` does not extend `NativeMemorySegmentImpl`; they are
siblings. So a heap segment has **five** fields, **no `min`**, and fewer than the
six that select the synthetic `(base@0 + offset@5)` arm — it fell all the way
through to the final `get_field(seg, 0)` and answered its **length**.

Two more measurements complete it:

```
$ java F27Probe
segment class=jdk.internal.foreign.HeapMemorySegmentImpl$OfByte   # ofArray(new byte[16])
segment class=jdk.internal.foreign.HeapMemorySegmentImpl$OfInt    # ofArray(new int[4])
segment class=jdk.internal.foreign.NativeMemorySegmentImpl        # ofBuffer(allocateDirect(16))
```

and, in the tree, `panama.rs` registers `MemorySegment.ofArray` for **`[I`, `[J`,
`[F` and `[D` only** — there is no `([B)`, `([S)` or `([C)` row. `ofArray` IS on
`native_override.rs`'s force-route name list, but a forced name with no
registration for that descriptor falls back to real bytecode. So on `--jdk-only`
the three unregistered arms produce a real `HeapMemorySegmentImpl$OfByte/OfShort/
OfChar`, and the four registered ones copy into native memory and hand back a
six-slot synthetic. **That is exactly why only the byte/short/char arms crash**,
and §7.1's repro is the byte one.

### 6.1 The fix, and what it deliberately does not do

A heap segment's bytes live in a Java array on the managed heap. There is no
machine address to answer with, and inventing one is how this went wrong.
`segment_address` now recognises the carrier (`is_real_heap_segment`, by class
name and by a resolvable non-null `base` field) and answers **0**, which every
caller already treats as inaccessible: `pe_segment_access_addr` raises
`IllegalStateException: Null segment address`. A Java exception is strictly
better than a SIGSEGV.

`marshal_arg`'s `LAYOUT_ADDRESS` arm additionally refuses one **by name**,
because `0` is a legitimate C null that `checked_foreign_addr` deliberately
passes — without that arm a heap segment handed to a downcall would silently
become `NULL` instead of saying why.

**Not fixed**: making `MemorySegment.get/set` on a heap segment actually read
and write the backing Java array. That is `panama.rs`, not this lane's file, and
it is NOM F27-3.

---

## 7. Tests

Eleven new tests in `panama_libffi.rs`, all with the oracle's numbers quoted at
the site. Each is written so the pre-fix implementation fails it — the mutation
is named in the doc comment where it is not obvious.

| test | pins | pre-fix answer |
|---|---|---|
| `real_jdk_value_layout_carriers_are_classified` | all nine `ValueLayouts$Of*Impl` + three of this VM's spellings | nine of twelve → `LAYOUT_LONG` |
| `group_and_sequence_carriers_are_classified` | struct/union/sequence/padding, both spellings | all nine → `LAYOUT_LONG` |
| `an_unrecognised_carrier_is_unknown_not_an_eight_byte_integer` | `LAYOUT_UNKNOWN`, and that `(0..10)` excludes it | `LAYOUT_LONG` |
| `group_layout_size_and_alignment_come_from_the_carrier` | 12/8 for `struct(LONG,INT)` | 8/8 |
| `sequence_carrier_reports_its_total_not_its_alignment` | 40/4, and count 10 derived | size 8, align 8 |
| `a_unions_size_is_not_its_alignment` | 7/4 — the row where they differ | 8/8 |
| `aggregate_return_slot_covers_the_c_size_not_just_the_jdk_byte_size` | **16** bytes for the 12/8 layout | **8** |
| `primitive_return_slot_is_still_widened_to_ffi_arg` | negative control: a byte return is still `sizeof(usize)`; void is empty | same |
| `an_unclassifiable_return_layout_is_refused` | the refusal names the class | silently sized 8 |
| `a_real_heap_segment_has_no_machine_address` | `!= 16` and `== 0`; `segment_byte_size` still 16 | **16**, then SIGSEGV |
| `native_and_synthetic_segment_addresses_are_unchanged` | negative control for the arm above: `min` still wins, `(base@0+offset@5)` still 0x1020 | same |
| `a_heap_segment_pointer_argument_is_refused_by_name` | the refusal, plus a control that a plain long still marshals | passed **16** to the callee |
| `a_real_jdk_int_layout_marshals_four_bytes` | 4-byte slot for `ValueLayouts$OfIntImpl` | 8 |

Three of these are **negative controls** rather than assertions of the fix
(`primitive_return_slot_is_still_widened_to_ffi_arg`,
`native_and_synthetic_segment_addresses_are_unchanged`, and the second half of
`a_heap_segment_pointer_argument_is_refused_by_name`); they exist so a future
over-broad change to the same functions goes red.

**Mutation note on the mock.** `MockNativeContext::get_field_by_name` is a
name-keyed map independent of `set_field`, so a test that only *reads* a name
measures the mock, not the resolver `[mock=slot table]`. The two tests that
depend on a name (`min`, `base`) do so in opposite directions: the heap test
sets **no** names at all and still classifies (via the class name), and the
native test sets `min` and asserts it WINS. Neither can pass by mock accident.

---

## 8. NOMINATIONS

### NOM F27-1 — `native-builtins/src/panama.rs:2925-2931`: the clamp comment is now false

Not this lane's file. The clamp itself must **stay** (§4.1); only its comment is
stale. OLD — anchor on the TEXT, not the line number; `grep -c "Clamping keeps
this side correct" native-builtins/src/panama.rs` is **1** today. The file is
pure CRLF (5833/5833) and the indent is sixteen spaces. Note that
`sed -n '2925,2932p' | cat -A` shows no `^M` here: msys `sed` runs in text mode
and drops CRs, exactly as F16-1 §0 warns — do not conclude the file is LF from
a `sed` pipeline.

```rust
                // Clamping keeps this side correct (the segment reports the
                // layout's real size, and the tail is the zeroed allocation)
                // without reaching into a file this lane does not own. The
                // matching fix — teach `layout_total_size`/`layout_align` to
                // read `[0]=byteSize, [1]=byteAlignment` — is NOMINATED, and
                // until it lands an aggregate return wider than 8 bytes is
                // TRUNCATED rather than corrupt.
```

NEW:

```rust
                // THE MATCHING FIX LANDED (F27, 2026-08-13).
                // `panama_libffi::layout_total_size` now reads the carrier's
                // own `[0]=byteSize`, and `alloc_return_slot` rounds that up to
                // the layout's alignment because libffi writes the C size of an
                // aggregate (measured: struct(JAVA_LONG, JAVA_INT) is 12 to the
                // JDK and 16 to C). So `ret_slot.len() >= total` always and this
                // clamp no longer truncates anything.
                //
                // IT STAYS ANYWAY. `total` is computed by `p67_layout_size_of`
                // in foreign_ffm.rs and `ret_slot.len()` by `alloc_return_slot`
                // in panama_libffi.rs; the clamp is the LOCAL proof that the
                // unsafe copy below is in bounds. Removing it would make an
                // unsafe block's safety argument depend on two functions in two
                // files continuing to agree, with nothing at the site saying so.
```

### NOM F27-2 — `native-builtins/src/panama.rs`: three `kind < 10` range tests admit `LAYOUT_UNKNOWN`

Not this lane's file. `LAYOUT_UNKNOWN` is `-2`, so `kind < 10` is TRUE for it.
Three sites test the range that way:

* `:2889` — `} else if kind < 10 {` in the downcall return unmarshal;
* `pe_segment_get_impl` / `pe_segment_set_impl` reach their `_ =>` arms with it
  (`Value::Int(0)` and a silent no-op respectively);
* `downcall_layout_carrier_descriptor(:2585)`'s `_ =>` answers
  `"Ljava/lang/foreign/MemorySegment;"` for it.

**None is currently reachable** — `alloc_return_slot` and `layout_to_ffi_type`
both refuse an unknown carrier before any of them runs, and both are on every
path. This is hygiene against a reorder, not a live defect. Ask: spell them
`(0..10).contains(&kind)` and give each an explicit
`plf::LAYOUT_UNKNOWN => …refuse…` arm. Also rename `panama.rs:3583`'s `-1` void
sentinel to a named constant, since it is the reason `LAYOUT_UNKNOWN` had to be
`-2`.

### NOM F27-3 — `native-builtins/src/panama.rs`: `MemorySegment.ofArray` covers four of seven primitives, and heap segments cannot be read

Not this lane's file, and the larger half of §6.

1. `ofArray` is registered for `[I`, `[J`, `[F`, `[D`. **`[B`, `[S` and `[C` have
   no registration**, so those three produce a real
   `HeapMemorySegmentImpl$OfByte/OfShort/OfChar` in `--jdk-only`. After this
   lane they raise `IllegalStateException: Null segment address` instead of
   SIGSEGV-ing; before it they crashed the VM. Adding the three missing rows in
   the shape of the existing `[I` one restores the synthetic carrier and makes
   them work.
2. The general fix is bigger and better: teach `pe_segment_get_impl` /
   `pe_segment_set_impl` to read and write the backing Java array when the
   receiver is a heap segment (`plf::is_real_heap_segment`), instead of
   dereferencing an address it does not have. W7-83 §1 has the measured
   `asByteBuffer` rows for the same receivers and should be read alongside it —
   in particular `MemorySegment.ofArray(new int[4]).asByteBuffer()` throws
   `UnsupportedOperationException` on the oracle, so the element type is not
   uniformly a stride.

### NOM F27-4 — `docs/known-issues/jdk-only/W7-89-memorysession-checkvalidstate.md` §7.1: the attributed cause is wrong

Not this lane's file. §7.1 attributes the `0x10` fault to "a `Buffer.address`
read as a pointer". §6 above shows it is `segment_address` answering a real heap
segment's `length`, and `0x10 == 16 == new byte[16].length`. §12.4 of the same
record then builds on the wrong attribution to connect §7.1 to the tagged-arena-
handle family, which is a different defect with a different signature
(`0x4000_0010_…`, bit 62 set). Ask: mark §7.1 CLOSED with a pointer here, and
narrow §12.4's claim to the direct-`ByteBuffer` half it actually covers.

### NOM F27-5 — `docs/known-issues/jdk-only/INDEX.md`

```
* `F27-1-the-reader-that-answered-eight-and-the-length-it-called-an-address-20260813.md`
  — F16-1 NOM-1 and NOM-2 landed, and NOM-1 was incomplete as written.
  `panama_libffi.rs`'s layout decoder could not see the real JDK's
  `ValueLayouts$Of*Impl` carriers — the ONLY ones `--jdk-only` has — so every
  value layout was an eight-byte integer and `JAVA_FLOAT` travelled in an
  integer register; and `layout_total_size`/`layout_align` answered 8/8 for a
  group layout of any width through an early return that made their group arm
  unreachable. Three MORE readers in the same file decoded the carrier's
  alignment as a size or a count (union width, sequence element count, padding
  width), so landing only the three functions NOM-1 named would have widened
  the disagreement. The `panama.rs` clamp is NOT lifted and the record says why;
  the severe defect on that path was on the other side of it — `alloc_return_slot`
  sized every aggregate return buffer at 8 bytes and libffi writes the full C
  size, an out-of-bounds WRITE present today. NOM-2's four stale force-route
  entries are deleted from both copies, and the "twice in the file" is twice in
  ONE FUNCTION. Plus W7-89 §7.1's fatal `read at address 0x10` diagnosed:
  `segment_address` handed out a real heap segment's byteSize, and 0x10 is 16.
```

---

## 9. Residuals

1. **Nothing Rust here was built, type-checked or run.** `rustfmt` exit 0 on
   scratch copies of both files rules out syntax errors only. The riskiest
   unchecked things are: `ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array`
   in `layout_struct_to_ffi_type` (the method is on `NativeHeapAccess`, a
   supertrait of `NativeContext`, and the file already calls `array_length` /
   `class_name_of_id` the same way, so the resolution pattern is established but
   this exact call is new); the `crate::phases_late::foreign_ffm::p67_layout_carrier_name`
   path (`pub(crate)` in a `pub mod` with no `#[cfg]` on either the module or
   the function, and `panama.rs:2904` already calls a sibling in the shipping
   downcall path, so it compiles in both feature configs — reasoned, not
   compiled); and the eleven new tests' use of `crate::test_utils::mock_ctx`.
2. **`layout_align` still has a silent `1` fallback** for a carrier with no
   positive `Long` in slot 1 and a kind outside 0..10. Every path that can reach
   it has already been refused by `layout_total_size` or `read_layout_kind`, so
   it is unreachable rather than defaulting — but it is a plausible number in a
   function that returns `usize`, and making it `Result` touches five call sites
   across two files.
3. **The C-size rounding in `alloc_return_slot` is reasoned, not measured**
   (§4.2). It cannot be measured without a build. It is one-directional: the
   buffer is never smaller than the JDK size or the ffi-arg minimum.
4. **§6's fix makes the byte/short/char `ofArray` arms REFUSE rather than work.**
   That is an improvement over a fatal SIGSEGV and a regression against nothing
   — the old behaviour was to crash the VM — but a caller who was previously
   killed will now see `IllegalStateException: Null segment address`, whose
   wording does not name the real reason. The named message is in
   `marshal_arg`'s arm only; `pe_segment_access_addr` is `panama.rs`. NOM F27-3.
5. **The four `ofArray` registrations that DO exist copy the array into native
   memory** rather than aliasing it, so a write through the segment is not
   visible through the Java array. Out of scope here, and orthogonal to §6, but
   it is the reason NOM F27-3's item 2 is the better fix than its item 1.
6. **No CratonVM-side measurement of any row in this record.** Every "before"
   column is read from source. The oracle column is a transcript
   (`scratchpad/f27/F27Probe.java`).
