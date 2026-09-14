# F16-1 — one layout encoding, the padding the JDK never inserts, and the union that discarded its members

**2026-08-13, lane F16.** Lands F9-1's N2 and N3. Patches
`native-builtins/src/panama.rs`,
`native-builtins/src/phases_late/foreign_ffm.rs` and `vm/src/vm/tests.rs`,
which this lane owns. All patches are in the working tree, uncommitted.

**This lane may not build or run CratonVM, and did not.** Every JDK fact below
is `javap` / `java` on this host (Microsoft build **25.0.3+9-LTS**) or the JDK
source at `C:\craton\jdk25src`, and is quoted at the point it is used. Every
claim about CratonVM's behaviour — before and after — is **PREDICTED** from
source. All three files were parse-checked (`rustfmt --edition 2021 --emit
stdout` on scratch copies, exit 0 for each), which rules out syntax errors and
nothing else; none was type-checked.

**Line endings — and a check that lied.** All three source files are CRLF and
end CRLF: 5833/5833, 4112/4112, 77738/77738 CR/LF, with **0 mid-line CRs** in
each. But two of them were LF for part of this lane's work, and the check I was
using said otherwise. `grep -c $'\r' <file>` **matches every line regardless**
in this shell — `$'\r'` is not expanded, so the pattern degenerates and the
count comes back equal to the line count, which is exactly what "pure CRLF"
looks like. It reported 6346/6346 for a file that had just had every CR
stripped.

The CRs were stripped by `sed -i` and by an `awk` rewrite, both of which run in
text mode here. Anything that rewrites a whole file through msys `sed`/`awk`
will silently drop CRs; the `Edit` tool does not (`foreign_ffm.rs`, edited only
that way, never lost one). Restored with `perl -i -pe 's/\r?\n$/\r\n/'` and
re-verified.

**Use `tr -cd '\r' < f | wc -c` against `tr -cd '\n' < f | wc -c`**, or
`head -2 f | cat -A` and look for `^M$`. Do not use `grep -c $'\r'`. A green
line-ending check that cannot go red is worth nothing, and this one cannot.

---

## 0. Verdict

| claim | verdict |
|---|---|
| F9-1 N2: two live encodings of one layout surface, chosen by registration order | **CONFIRMED, AND ONE ENCODING DELETED.** The JDK-shaped 4-slot carrier wins; `panama.rs` mints no layout outside its own `#[cfg(test)]` module (§1, §2) |
| F9-1's "`register_pe_panama` runs after `register_p67_foreign_memory` on **both paths**" | **HALF WRONG, AND THE HALF THAT IS WRONG MATTERS.** `register_pe_panama` has ONE call site and it is inside `#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`. Real-JDK/`--jdk-only` mode never registered panama's layouts at all — so the two modes were already running different implementations (§1.2) |
| F9-1 N3: `structLayout` auto-pads where HotSpot throws | **CONFIRMED, RE-MEASURED, AND FIXED** — and it was the smaller of two arithmetic defects (§3) |
| the JDK does not round a struct's TOTAL size up to its alignment | **NEW. `structLayout(JAVA_LONG, JAVA_INT)` is 12 on the oracle and was 16 here** — a call the oracle ACCEPTS, so no refusal was hiding it, and nothing tested it (§3.2) |
| `paddingLayout` | **NEW DEFECT.** A one-slot carrier with no alignment, read back as "alignment = size", so the padded idiom the JDK *requires* — `struct(JAVA_BYTE, paddingLayout(3), JAVA_INT)` — answered **12** where the oracle answers **8** (§3.3) |
| `unionLayout` in the shipping registrar | **NEW DEFECT, AND IT WAS UNOBSERVABLE.** It DISCARDED its members and answered a one-slot carrier holding `Long(0)`; `UnionLayout` had no `byteSize` registration to read it back with, so the fabricated 0 could only surface as an `AbstractMethodError` (§3.4) |
| `sequenceLayout(-1, …)` | **NEW.** Accepted, and answered a negative `byteSize`. The JDK throws (§3.5) |
| `Arena.allocate(ValueLayout)` and `Arena.allocate(MemoryLayout)` | **NEW, AND THE MOST SERIOUS.** Both sized the allocation from a reader that could not read the carrier they were handed: one byte for a `JAVA_LONG`, eight bytes for a struct of any width (§4) |
| `p67_group_member_offset` | **SECOND COPY OF THE OFFSET RULE, corrected before it could drift** — and my union fix made its union arm reachable for the first time (§5) |
| five tests "PREDICTED to flip" | **SIX changed, TWO now assert an exception where they asserted a number, FOUR were re-spelled. Four NEW tests added.** Full table in §6 |

---

## 1. The two encodings, and which mode actually saw which

### 1.1 What they were

| | minter | shape |
|---|---|---|
| **loser** | `panama.rs::pe_make_layout` | 3 slots — `[0]=Int(kind)`, `[1]=Int(byteSize)`, `[2]=name`; groups 6 slots `[kind, size, members, names, offsets, align]` |
| **winner** | `phases_late/foreign_ffm.rs::p67_layout_object` | 4 slots — `[0]=Long(byteSize)`, `[1]=Long(byteAlignment)`, `[2]=payload`, `[3]=name` |

The winner is not a preference. It is the JDK's own field order:

```
$ sed -n '52,54p' jdk25src/java.base/jdk/internal/foreign/layout/AbstractLayout.java
    private final long byteSize;
    private final long byteAlignment;
    private final Optional<String> name;
```

so slots 0 and 1 read identically on a CratonVM carrier and on a real JDK
layout object. The loser's `[0]=Int(kind)` reads as *nothing* on a real one.

The winner is also the reachable one. `ValueLayout.JAVA_INT` is a FIELD:

```
$ javap -p java.lang.foreign.ValueLayout            # 25.0.3+9-LTS
  public static final java.lang.foreign.ValueLayout$OfInt JAVA_INT;
$ javap -c F16Desc     # static Object e(){ return ValueLayout.JAVA_INT; }
  0: getstatic Field java/lang/foreign/ValueLayout.JAVA_INT:
                    Ljava/lang/foreign/ValueLayout$OfInt;
```

`foreign_ffm.rs` registers a `<clinit>` that populates those static fields, so
a plain `getstatic` finds an object. `panama.rs` registered nine rows under
`()Ljava/lang/foreign/ValueLayout;` — a METHOD descriptor for a FIELD, naming a
return type that appears nowhere in JDK 25. **No classfile can reach them.**
Their only caller in the tree was `vm/src/vm/tests.rs`.

### 1.2 The ordering fact F9 got half right, and why the correction matters

F9-1 §5.1 says `register_pe_panama` runs after `register_p67_foreign_memory`
"on both paths through `lib.rs`". Measured:

```
$ grep -rn "register_pe_panama" --include=*.rs . | grep -v ^./target
  native-builtins/src/lib.rs:24181:    register_pe_panama(registry);      <- the ONLY call site
  native-builtins/src/panama.rs:289:pub(crate) fn register_pe_panama(...)
$ grep -n "^pub fn register_synthetic_overrides" native-builtins/src/lib.rs
  21612:pub fn register_synthetic_overrides(registry: &mut NativeMethodRegistry) {
$ sed -n '21611p' native-builtins/src/lib.rs
  #[cfg(feature = "synthetic-jdk")]
```

`:24181` is inside `register_synthetic_overrides`, and no other `pub fn`/`fn`
opens between `:21612` and it. `register_p67_foreign_memory` has two call
sites: `lib.rs:10053` inside `register_essential_natives_with_shims` (the
always-on real-JDK path) and `phases_late.rs:5631` inside
`register_phase67_natives`, which `lib.rs:24121` calls — also inside
`register_synthetic_overrides`, and **before** `:24181`.

So the true picture is:

* **`--jdk-only` / real-JDK:** `foreign_ffm` only. panama's layouts were never
  registered. The 4-slot carrier has always been the one in play.
* **synthetic-JDK:** both, panama last, so panama won every shared key.

This is a **stronger** statement than "registration order decides", and it
changes the diagnosis. The divergence was not latent — the two shipping modes
were running two different implementations of `MemoryLayout.structLayout`, with
different arithmetic, and the mode you booted decided which. It also means
`vm/src/vm/tests.rs` (which is `VmConfig::default()` = synthetic) was testing
the implementation that **`--jdk-only` never runs**. Every arithmetic defect in
§3 below is in the one that ships in `--jdk-only`, and it had no test at all.

## 2. What was deleted

From `panama.rs`:

* the nine `()Ljava/lang/foreign/ValueLayout;` rows, and `byteSize`,
  `byteAlignment`, `name` and two `withName` overloads reading them —
  `register_pe_value_layout` is now a banner and nothing else;
* the entire group-layout family: `structLayout`, `unionLayout`,
  `sequenceLayout` and `paddingLayout` each registered TWICE (once JDK-true,
  once under a fabricated `…)Ljava/lang/foreign/MemoryLayout;` return that
  javac never emits), plus `byteSize`/`byteAlignment`/`name`/`withName`/
  `memberLayouts` on `StructLayout` and `GroupLayout`, `byteOffset` on
  `StructLayout`, and `withName`/`varHandle`/`name`/`byteSize` on
  `MemoryLayout`;
* the functions behind them — `pe_struct_layout`, `pe_union_layout`,
  `pe_sequence_layout`, `pe_memory_layout_var_handle`,
  `pe_memory_layout_path_target`, `pe_memory_layout_width`,
  `layout_members_as_list`, `pe_layout_name`, `pe_layout_name_value`,
  `pe_layout_with_name`, `pe_optional`;
* two of panama's own unit tests, `test_85_3_downcall_struct_layout` and
  `panama_struct_layout_preserves_named_members` (§6.3).

`pe_make_layout` and `PE_VALUE_LAYOUT_NAME_SLOT` survive as `#[cfg(test)]` —
panama's segment get/set tests use the kind tag as a fixture, and
`panama_libffi::read_layout_kind` still decodes it. Marking them
`#[cfg(test)]` is what makes "one production encoding" a compile-time fact
rather than a convention.

**KEPT DELIBERATELY:** `MemoryLayout$PathElement.groupElement/sequenceElement`.
`foreign_ffm.rs` decodes path elements but mints none, because in real-JDK mode
`groupElement("c")` runs the JDK's own bytecode. Synthetic mode has no such
bytecode, so these two rows are its only source, and the 2-field carrier they
build is a shape `p67_classify_path_element` explicitly accepts.

`LAYOUT_PADDING`, `LAYOUT_SEQUENCE`, `LAYOUT_STRUCT` and `LAYOUT_UNION` are no
longer imported at file scope (their last non-test use was a stale comment);
the test module imports them itself.

## 3. The arithmetic — what the surviving implementation had wrong

The rule, from the oracle's own source, is nine lines:

```java
// jdk25src/java.base/jdk/internal/foreign/layout/StructLayoutImpl.java
public static StructLayout of(List<MemoryLayout> elements) {
    long size = 0;
    long align = 1;
    for (MemoryLayout elem : elements) {
        if (size % elem.byteAlignment() != 0) {
            throw new IllegalArgumentException(
                "Invalid alignment constraint for member layout: " + elem);
        }
        size = Math.addExact(size, elem.byteSize());
        align = Math.max(align, elem.byteAlignment());
    }
    return new StructLayoutImpl(elements, size, align, align, Optional.empty());
}
```

No padding is inserted, and the total is not rounded. Measured (`java
F16Probe`, `java F16Align`, 25.0.3+9-LTS):

```
structLayout(JAVA_BYTE, JAVA_INT)   -> IAE: Invalid alignment constraint for member layout: i4
structLayout(JAVA_INT,  JAVA_LONG)  -> IAE: ... : j8
structLayout(JAVA_BYTE, JAVA_SHORT) -> IAE: ... : s2
structLayout(pad(1), JAVA_INT)      -> IAE: ... : i4
structLayout(JAVA_BYTE, seq(2,INT)) -> IAE: ... : [2:i4]
structLayout(JAVA_BYTE, JAVA_INT.withName("x")) -> IAE: ... : i4(x)

structLayout(JAVA_LONG, JAVA_INT)   -> byteSize=12 align=8
structLayout(JAVA_INT,  JAVA_BYTE)  -> byteSize=5  align=4
structLayout(JAVA_SHORT, JAVA_BYTE) -> byteSize=3  align=2
structLayout()                      -> byteSize=0  align=1
structLayout(JAVA_BYTE, pad(3), JAVA_INT)  -> byteSize=8  align=4
structLayout(JAVA_INT,  pad(4), JAVA_LONG) -> byteSize=16 align=8
structLayout(JAVA_BYTE, JAVA_INT.withByteAlignment(1)) -> byteSize=5 align=1
unionLayout(JAVA_INT, JAVA_LONG)    -> byteSize=8  align=8
unionLayout(JAVA_BYTE, JAVA_INT)    -> byteSize=4  align=4
sequenceLayout(10, JAVA_INT)        -> byteSize=40 align=4
sequenceLayout(0,  JAVA_INT)        -> byteSize=0  align=4
sequenceLayout(-1, JAVA_INT)        -> IAE: The provided elementCount is negative: -1
paddingLayout(3)                    -> byteSize=3  align=1
```

### 3.1 Silent auto-padding — F9-1 N3, confirmed and fixed

`foreign_ffm`'s `structLayout` did `offset = ((offset + align - 1) / align) *
align` per member. It now checks `size % member_align != 0` and raises
`IllegalArgumentException` with the JDK's wording. A member whose slot 0 is not
a `Long` is named and refused rather than defaulted (§7).

### 3.2 The total was rounded up — NOT in F9-1, and worse in one respect

The old body ended `total_size = ((offset + max_align - 1) / max_align) *
max_align`. The JDK does not do this:

```
structLayout(JAVA_LONG, JAVA_INT) -> byteSize=12   (this VM: 16)
```

**This is a call the oracle ACCEPTS** — long at offset 0, int at offset 8, both
naturally aligned — so unlike §3.1 there is no refusal downstream to mask it.
It is a plain wrong number, on a legal layout, and no test in the tree touched
it. Now covered by `struct_layout_does_not_pad_the_total`.

### 3.3 `paddingLayout` had no alignment to read

`paddingLayout` minted a ONE-slot carrier, `[0]=Long(size)`. Every reader in
the file expects the four-slot head, and the member decode's fallback was
"alignment = size". So `paddingLayout(3)` claimed **alignment 3**, and the
idiom the JDK *requires* came out wrong:

```
structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT)
  oracle : byteSize=8  align=4
  CratonVM: byte@0 (size 1); pad align 3 -> @3, size 6; int align 4 -> @8,
            size 12; total align_up(12,4) = 12
```

**12 vs 8, on the correct JDK usage.** The fix is the four-slot carrier with
alignment pinned to 1, plus `byteSize`/`byteAlignment` registrations on
`PaddingLayout`, which had **none** — `paddingLayout(3).byteSize()` resolved to
the abstract interface declaration and raised `AbstractMethodError: … has no
Code attribute`. Covered by
`padding_layout_is_byte_aligned_and_pads_a_struct_to_eight`.

### 3.4 `unionLayout` discarded its members, and nothing could see it

The shipping body was, in full:

```rust
|ctx, _args| {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/UnionLayout", 1)?;
    ctx.set_field(obj, 0, Value::Long(0));
    Ok(Some(Value::Object(Some(obj))))
}
```

`_args` — the members are not read. Every union is size 0.

**And the fabricated 0 was unobservable**, which is why it survived: the
group-accessor loop covers `GroupLayout` and `StructLayout` only, so
`UnionLayout` had no `byteSize`, no `byteAlignment`, no `memberLayouts`, no
`name`, no `withName`. In `--jdk-only`, `unionLayout(JAVA_INT,
JAVA_LONG).byteSize()` raised `AbstractMethodError` before it could report the
wrong number. `union_layout_takes_max_size` passed all along because it ran in
synthetic mode against **panama's** union, which did compute a maximum — the
one that has now been deleted.

This is the sharpest instance of §1.2's point: the test covered the
implementation `--jdk-only` does not run, and the one it does run was a stub.

Fixed: `[max_size, max_align, members, name]`, no alignment constraint (every
member sits at offset 0, so `size % align` is vacuous — confirmed, `union(BYTE,
INT)` is accepted where the struct is refused), plus the missing accessors.

### 3.5 `sequenceLayout` accepted a negative count

`sequenceLayout(-1, JAVA_INT)` produced `byteSize = -4`. Now refused with the
JDK's wording, and the multiply is `checked_mul` to match
`Math.multiplyExact` in `SequenceLayoutImpl`'s constructor. The accepted cases
(40 for `(10, JAVA_INT)`, 0 for `(0, …)`) were already right and are unchanged.

## 4. The two allocation sites — found while converting consumers

Both are in `panama.rs`, both sized an allocation from a reader that could not
read the carrier it was given, and both predate this lane.

**`Arena.allocate(Ljava/lang/foreign/ValueLayout;)`** read
`match ctx.get_field(layout, 1) { Value::Int(n) => n as i64, _ => 1 }` — slot 1
as an `Int`, the deleted carrier's `byteSize`. Every layout that reaches it
carries `Long(byteAlignment)` there, so the `Int` arm never matched and the
`_ => 1` default took over: **`arena.allocate(ValueLayout.JAVA_LONG)` reserved
one byte for an eight-byte value**, and returned a segment that passes its own
bounds check at every offset the caller then writes.

**`Arena.allocate(Ljava/lang/foreign/MemoryLayout;)`** and the by-value struct
RETURN path used `panama_libffi::layout_total_size` / `layout_align`. Those
start from `read_layout_kind`, which resolves a `Long`-slot-0 carrier by
matching its CLASS NAME against a list of the nine `ValueLayout$Of*` spellings.
No GROUP class is on that list, so it falls to `_ => LAYOUT_LONG` and
`layout_total_size` returns **8 for a struct of any width**, at alignment 8.
**This was already true for every `--jdk-only` run**, since the layout in hand
there has always been `foreign_ffm`'s; it only looked correct under synthetic,
where the kind-tagged carrier happened to answer.

Both now read the layout's own recorded size and alignment.

**One deliberate half-measure, named at the site.** The struct-return path
copies into the freshly sized allocation from `ret_slot`, which
`panama_libffi` sizes with its OWN `layout_total_size` — the function still
returning 8. Correcting only this side would make the `copy_nonoverlapping`
read past the end of `ret_slot`: an out-of-bounds READ introduced by fixing one
half of a pair. The copy length is therefore clamped to `ret_slot.len()`, so an
aggregate return wider than 8 bytes is **TRUNCATED rather than corrupt** until
NOM-1 lands. That is a smaller lie than the one it replaces and it is written
down in the code.

## 5. `p67_group_member_offset` — the second copy of the rule

`foreign_ffm` computes member offsets in a second place, for `byteOffset` and
the var-handle path walk. It did `offset = align_up(offset, member_align)` —
the same padding `structLayout` did.

Two things follow. First, it is now *redundant*: `structLayout` refuses any
layout in which the running offset is not already a multiple of the next
member's alignment, so `align_up` is a provable no-op for every struct that can
exist. Second, it is a rule written twice, and it would have gone on padding
after the primary stopped. It is now the plain running sum the JDK uses,
measured:

```
struct(b1, x3, i4, j8) -> b=0, i=4, l=8
struct(j8, i4)         -> l=0, i=8      (size 12)
```

**And my union fix made its union arm reachable for the first time.** While
`unionLayout` discarded its members, `p67_group_members` answered `None` and
this function never ran on a union. Now that a union carries them, it does —
and it would have accumulated offsets for a layout whose members are all at
zero. Measured and fixed:

```
u = unionLayout(JAVA_BYTE.withName("b"), JAVA_INT.withName("i"), JAVA_LONG.withName("l"))
u.byteOffset(groupElement("b")) = 0
u.byteOffset(groupElement("i")) = 0
u.byteOffset(groupElement("l")) = 0
```

This is a defect *introduced by* fixing §3.4 and caught by asking what the fix
made reachable, not by the fix itself. **It has no test** — see §8.3.

## 6. The tests — what changed and in which direction

### 6.1 Descriptors: 14 sites in 8 tests

All fourteen `()Ljava/lang/foreign/ValueLayout;` sites now carry the spelling
`javap` prints (`Ljava/lang/foreign/ValueLayout$OfByte;`, `$OfInt;`,
`$OfLong;`, `$OfDouble;` by field). Rewritten mechanically and counted:
`rewrote 14 descriptors`, and `grep -c '"()Ljava/lang/foreign/ValueLayout;"'`
is now **0**. The five group-layout call sites moved from the fabricated
`…)Ljava/lang/foreign/MemoryLayout;` to `StructLayout` / `UnionLayout` /
`SequenceLayout`.

The block comment above `panama_value_layout_constants_pe` — F9's "just fix it
is not available" note — is replaced with what resolved it.

### 6.2 Per test

| test | before | after | direction |
|---|---|---|---|
| `panama_value_layout_constants_pe` | `JAVA_INT`/`JAVA_LONG` under the fabricated descriptor; `byteSize` = 4 / 8 | `$OfInt;` / `$OfLong;`; same 4 / 8 | **unchanged verdict**, re-spelled. `byteSize` on plain `ValueLayout` is newly registered in `foreign_ffm` to keep it answerable |
| `panama_memory_segment_get_set_pe`, `panama_memory_segment_fill_pe`, `panama_function_descriptor_pe`, `panama_segment_double_roundtrip` | fabricated descriptor | `$Of*` descriptor | **PREDICTED unchanged.** These drive `MemorySegment` get/set, which goes through `read_layout_kind`; a `$Of*`-classed object resolves by class name to the same kind |
| `panama_struct_layout_pe2` | asserts 16 / 8 for `structLayout(JAVA_INT, JAVA_LONG)` | asserts `IllegalArgumentException`, then asserts 16 / 8 for the PADDED call | **changes shape.** The 16/8 moves to the call that actually produces it |
| `struct_layout_byte_int_alignment` | asserts 8 / 4 and an offsets array at slot 4 | asserts `IllegalArgumentException` | **changes shape.** The 8/4 moves to the new padding test |
| `panama_sequence_layout_pe2` | reads slot 1 for the size | calls `byteSize()` / `byteAlignment()`; 40 / 4 | **unchanged verdict** |
| `struct_layout_single_field` | reads slots 1 and 5 | calls `byteSize()` / `byteAlignment()`; 4 / 4 | **unchanged verdict** |
| `union_layout_takes_max_size` | reads slot 1; passed against panama's union | calls `byteSize()` / `byteAlignment()`; 8 / 8 | **verdict unchanged, subject changed.** It now exercises the implementation `--jdk-only` runs, which was a stub |

**Four NEW tests**, all with the oracle's numbers quoted at the site:
`struct_layout_does_not_pad_the_total` (12, §3.2),
`padding_layout_is_byte_aligned_and_pads_a_struct_to_eight` (3/1 and 8/4,
§3.3), `sequence_layout_rejects_negative_element_count` (§3.5), and the
refusal arm inside `panama_struct_layout_pe2`.

**Every slot-index read of a layout in these tests is gone.** They call
`byteSize()` / `byteAlignment()` now, so a future carrier change breaks the
implementation rather than silently re-pointing the assertion.

### 6.3 Two panama unit tests deleted with the function they tested

`test_85_3_downcall_struct_layout` called `pe_struct_layout` on `{int, long}`
and asserted 16 / 8 — a call the oracle refuses, i.e. a test freezing the VM's
own wrong answer as a specification. `panama_struct_layout_preserves_named_members`
asserted member names survive into a `names` array at slot 3, a slot the
authoritative carrier does not have (names are resolved from each member's own
`name()`, so there is no second copy to drift). Both are recorded at the
deletion site with the transcript.

## 7. The `_ => 0` default, and what replaced it

The defect F9 named was a defaulting reader: `match ctx.get_field(m, 0) {
Value::Int(k) => k, _ => 0 }`, where `LAYOUT_BYTE == 0`, so an unrecognised
carrier decoded as a one-byte layout and `sequenceLayout(10, JAVA_INT)`
answered 10.

`p67_member_size_align` has **no default arm**. It returns `Option`, and both
group factories turn `None` into an `IllegalArgumentException` naming the
member's index, its class, and the `Value` actually found in slot 0. The one
remaining fallback is deliberate and documented: a carrier with a size but no
recorded alignment is treated as a value layout whose alignment is its size,
which is the JDK's rule for every `ValueLayout` constant except the
`_UNALIGNED` ones — and those DO record a separate 1 (measured:
`JAVA_INT_UNALIGNED` byteSize=4 align=1, and `struct(JAVA_BYTE,
JAVA_INT_UNALIGNED)` is accepted at size 5 align 1).

`p67_layout_render` reproduces `MemoryLayout::toString` for the exception
message. Measured: `b1 z1 c2 s2 i4 j8 f4 d8 a8`, `x3` for padding, `[2:i4]`
for a sequence, `[i4i4]` for a struct, `[b1|i4]` for a union, `i4(x)` for a
named layout, `1%i4` for `JAVA_INT_UNALIGNED`. Value layouts, padding and names
are exact; group forms are reconstructed. **The message is a diagnostic, not a
contract** — what a caller can `catch` is the KIND, and that is exact. The
tests assert the kind strictly and the message loosely, for that reason.

## 8. NOMINATIONS

### NOM-1 — `native-builtins/src/panama_libffi.rs` (NOT this lane's file): `read_layout_kind`, `layout_total_size`, `layout_align`

`read_layout_kind` is the reconciliation layer for two encodings that now has
only one left to reconcile, and it has two problems.

1. Its class-name list carries the nine `java/lang/foreign/ValueLayout$Of*`
   spellings and **not** the real JDK's own
   `jdk/internal/foreign/layout/ValueLayouts$OfIntImpl` family — which is what
   a real-JDK object is. Those fall to `_ => LAYOUT_LONG`: an eight-byte read
   where four were meant. Note the drifted twin: `p67_layout_carrier_name`
   already handles both spellings with `.contains("OfInt")`, so the two
   name-matchers in this codebase disagree. Fix by delegating, not by adding
   nine more strings.
2. `layout_total_size` / `layout_align` answer **8 / 8 for every group
   layout**, because no group class is on that list either. They should read
   `[0]=Long(byteSize)` and `[1]=Long(byteAlignment)` directly — the same two
   slots a real `AbstractLayout` declares — and fall back to the kind only for
   a value layout.

`layout_total_size` also sizes `ret_slot` (`panama_libffi.rs:642`), which is
why §4's struct-return fix had to clamp its copy. **Landing this nomination
un-truncates aggregate returns wider than 8 bytes**; the clamp can then go, and
the comment at the site says so.

### NOM-2 — `vm/src/runtime/interpreter/native_override.rs` (NOT this lane's file): four stale force-route entries

`is_ffm_memory_layout_native_override` lists both spellings of each group
factory. The four `…)Ljava/lang/foreign/MemoryLayout;` entries —

```
("sequenceLayout", "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;")
("structLayout",   "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;")
("unionLayout",    "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;")
("paddingLayout",  "(J)Ljava/lang/foreign/MemoryLayout;")
```

— name descriptors javac does not emit and no registration now answers. They
are **harmless** (the predicate would force-route a call that cannot occur; the
JDK-true spellings beside them are all still registered and still answered), so
this is hygiene, not a defect. The same four appear twice in the file
(`:1807-1821` and `:3822-3836`), which is itself worth a look: two copies of
one table.

### NOM-3 — `docs/known-issues/jdk-only/INDEX.md` (NOT this lane's file)

```
* `F16-1-one-layout-encoding-and-the-padding-the-jdk-never-inserts-20260813.md`
  — F9-1 N2 and N3 landed. `java.lang.foreign`'s two incompatible layout
  encodings are ONE: `panama.rs` mints no layout outside `#[cfg(test)]`, and
  `phases_late/foreign_ffm.rs` owns the family in both JDK modes on the JDK's
  own `[byteSize, byteAlignment, …]` shape. Correcting F9-1's ordering claim:
  `register_pe_panama` is synthetic-only, so the two modes were already running
  DIFFERENT implementations — and every arithmetic defect was in the one
  `--jdk-only` ships, which had no test. `structLayout` now refuses an
  under-aligned member as HotSpot does and no longer rounds the total up
  (`struct(long,int)` is 12, not 16); `paddingLayout` is 4-slot with alignment
  1 (the JDK's own padded idiom answered 12 instead of 8); `unionLayout` no
  longer DISCARDS its members and answers 0; `sequenceLayout` rejects a
  negative count. Plus two allocation sites sized from a reader that could not
  read the carrier — one byte for a `JAVA_LONG`, eight for a struct of any
  width. Six tests changed (two now assert an exception where they asserted a
  fabricated number), four added, two panama unit tests deleted with the
  function they pinned.
```

## 9. Residuals

1. **Nothing Rust here was built, type-checked or run.** `rustfmt` exit 0 on
   scratch copies of all three files rules out syntax errors only. The riskiest
   unchecked things are: the error-match patterns in the new tests
   (`MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalArgumentException{..}))`
   — derived from `types/src/error.rs:89-93`, not observed); the `&mut dyn
   NativeContext` → `&dyn NativeContext` reborrows into `p67_member_size_align`
   / `p67_layout_render` (the same coercion the pre-existing
   `p67_layout_size_of(ctx, e)` call in `sequenceLayout` relies on); and the
   deletion of eleven functions from `panama.rs`, which was verified by
   `grep` for each name rather than by the compiler.
2. **`register_pe_value_layout` and `register_pe2_struct_layouts` are now
   near-empty and keep their names.** The first is a banner with `let _ = vl;`;
   the second holds only the two `PathElement` factories. Both carry a
   "DO NOT RE-ADD A LAYOUT FACTORY HERE" note explaining that a row added there
   would silently replace the JDK-true one for every synthetic-JDK run. Renaming
   them touches `lib.rs`, which this lane does not own.
3. **§5's union-offset fix has no test**, and it is the one change here that
   fixes a defect the rest of this lane's work *created*. Writing the test needs
   `withName` on a `$Of*` receiver and the `PathElement` factories, and I could
   not confirm from source that the first is registered on every `$Of*` class —
   so I did not write a test I could not predict the outcome of. **A
   `byteOffset` test over a named union and a named struct is the highest-value
   follow-up in this area**, and §5 carries the oracle's numbers for it.
4. **`p67_layout_render`'s group forms are reconstructed, not measured
   round-trip.** A struct's `[i4i4]` is built from the member array; if a
   member is itself un-renderable the part comes out empty. It only ever
   appears inside an exception message.
5. **The `--jdk-only` arithmetic is now correct by construction but untested in
   that mode.** Every test in `vm/src/vm/tests.rs` runs under
   `VmConfig::default()`, which is synthetic. What changed is that both modes
   now run the SAME code, so a synthetic-mode test finally has evidential value
   for `--jdk-only` — which it did not have before this lane. That is the main
   reason for collapsing the family rather than repairing both halves.
6. **Aggregate FFI returns wider than 8 bytes are truncated** until NOM-1
   lands (§4). Before this lane they were allocated 8 bytes and the segment
   *claimed* 8, so callers read a short struct; now the segment reports the
   right size and the tail is zeroed. Neither is correct; the second is
   diagnosable.
