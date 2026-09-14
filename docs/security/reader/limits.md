# `reader` resource limits and integer-safety audit

Scope: the `cratonvm-reader` crate — the class-file, descriptor, signature and
jimage parsers. This is the first code an untrusted `.class` file reaches, so
every count, length and index it reads is attacker-controlled.

This document covers three C2 review items:

- **P0 — "Fuzz class/JAR parsers for resource exhaustion."** Constant-pool
  counts, attribute lengths, recursive types, StackMap frames, signatures and
  path names must have explicit limits and bounded behaviour on adversarial
  input.
- **P1 — "Add integer and allocation limit tests despite checked-arithmetic
  claims."** Every conversion, multiplication, alignment and host-size boundary
  must be verified by test, not asserted by comment. Acceptance: *"Boundary
  corpus covers 32/64-bit limits and fails before allocation/write."*
- The **signature-cache shape bug** found (and worked around rather than fixed)
  by the `fuzz_jni_descriptor` fuzz target.

All limits now live in one module, [`reader/src/limits.rs`](../../../reader/src/limits.rs),
together with the checked-arithmetic helpers that enforce them. Nothing in the
reader panics on malformed input: every rejection is a
`ClassReaderError` (`reader/src/class_reader_error.rs`).

---

## 1. The rule

> A size read off the wire must be validated against the **bytes that remain in
> the input** before it is used to allocate.

A `u2` count is "bounded" only in the sense that it cannot exceed 65 535. That
is not a useful bound when a two-byte header can ask for 65 535 fat records
from a file that contains nothing else — the reservation is megabytes, and the
failure only happens on the read that follows it. Because every entry of every
table in the class-file format costs **at least one byte**, a declared count
greater than the remaining byte count is provably a lie and can be rejected
before the allocator is touched.

Three helpers implement this, all in `reader/src/limits.rs`:

| Helper | Line | Purpose |
|---|---|---|
| `checked_span(label, count, entry_size)` | `limits.rs:168` | `count * entry_size` with overflow rejected, never wrapped. A wrapped product yields a *small* byte span that `read_bytes` happily satisfies — the parser would then read a short prefix as if it were the whole table. |
| `ensure_count_fits(label, count, min_entry_bytes, remaining)` | `limits.rs:196` | Rejects a declared count the remaining input cannot possibly hold. Conservative: it uses the *shortest legal* encoding of an entry, so it never rejects an input the full parse would accept. |
| `bounded_capacity(count, min_entry_bytes, remaining)` | `limits.rs:227` | Reservation size = `min(count, PREALLOC_CAP, remaining / min_entry_bytes)`. Under-reserving is free (the `Vec` grows); over-reserving is the entire attack. |
| `wire_len_to_usize(label, u32)` | `limits.rs:239` | Explicit `u32 → usize` narrowing instead of `as usize`. Lossless on 32- and 64-bit hosts today; the function is the single place to look if the reader is ported to a narrower target or a field is widened. |
| `checked_end(label, start, len)` | `limits.rs:252` | `start + len` for byte ranges derived from wire values. A wrapped `end` produces a *reversed* range that downstream slicing would panic on rather than reject. |

---

## 2. Limit inventory

### 2.1 JVMS-mandated

These come from the class-file format itself. Rejecting past them is required
for conformance; HotSpot raises `ClassFormatError` at the same points.

| Limit | Value | Source | Enforced at | Test |
|---|---|---|---|---|
| `MAX_CONSTANT_POOL_COUNT` | 65 535 | JVMS §4.1 (`u2` field) | `class_reader.rs:311` (`validate_count`) | `limits.rs::constants_are_internally_consistent` |
| `constant_pool_count >= 1` | 1 | JVMS §4.1 (slot 0 is a sentinel) | `class_reader.rs:305` | `class_reader.rs::constant_pool_count_of_one_is_the_empty_pool_boundary` |
| `MIN_CONSTANT_POOL_ENTRY_BYTES` | 3 | derived from every `cp_info` encoding | `class_reader.rs:322` | `limits.rs::constant_pool_floor_is_a_sound_lower_bound` |
| Long/Double may not occupy the last slot | — | JVMS §4.4.5 | `class_reader.rs` (`CONSTANT_Long`/`Double` arms) | pre-existing |
| `MAX_ARRAY_DIMENSIONS` | 255 | JVMS §4.3.2 | `field_type.rs:89` | `field_type.rs::deeply_nested_array_is_rejected_not_overflow` (accept at 255, reject at 256) |
| `MAX_CODE_LENGTH` | 65 535, and `> 0` | JVMS §4.7.3 | `attribute.rs:1671` (decode) and `attribute.rs:796` (eager shape walk) | `attribute.rs::code_length_boundaries_are_enforced_in_both_directions` |
| Field/method/interface/attribute counts | 65 535 | JVMS §4.1, §4.5–4.7 (`u2` fields) | `class_reader.rs:157/184/202/632` | `class_reader.rs::section_count_guards_reject_impossible_declarations` |
| Per-attribute length must not exceed the remaining buffer | — | structural | `class_reader.rs:663`, `attribute.rs:1607` | `class_reader.rs::read_attributes_rejects_length_exceeding_buffer`, `attribute.rs::attribute_length_at_u32_extremes_is_rejected_not_truncated` |
| Attribute body must be consumed exactly | — | structural (prevents misalignment of the next attribute) | `attribute.rs::decode_attribute_with_source_arc` | pre-existing |
| No trailing bytes after the class attributes | — | structural | `class_reader.rs` (`read_class_shared`) | pre-existing |

### 2.2 Defensive

No JVMS basis. These exist purely to bound the work an adversarial input can
demand. All are set far above anything `javac` emits, so a legitimate class
never reaches them.

| Limit | Value | Rationale | Enforced at | Test |
|---|---|---|---|---|
| `PREALLOC_CAP` | 1024 elements | Ceiling on any single `Vec::with_capacity` sized from a wire count. | every `bounded_capacity` call, plus the remaining `.min(PREALLOC_CAP)` sites in `attribute.rs` | `limits.rs::capacity_saturates_at_prealloc_cap_not_at_the_declared_count`, `class_reader.rs::prealloc_cap_constant_exists_and_is_bounded` |
| `MAX_ATTRIBUTE_DEPTH` | 16 | Bounds `Code`-in-`Code` / `Record`-in-`Record` self-nesting (stack-overflow DoS). | `attribute.rs:972` + the depth check in `decode_attribute_body` | `attribute.rs::deeply_nested_code_attribute_is_rejected`, `::deeply_nested_record_attribute_is_rejected` |
| `MAX_ANNOTATION_DEPTH` | 256 | Bounds the `annotation` ⇄ `element_value` mutual recursion (`@`/`[` values). | `attribute.rs:1742` + checks in `decode_annotation_depth` / `decode_element_value_depth` | `attribute.rs::deeply_nested_element_value_array_is_rejected`, `::annotation_nesting_is_accepted_up_to_the_cap_and_rejected_one_past_it` (accept + reject twins) |
| `MAX_SIGNATURE_DEPTH` | 256 | Bounds nested generic type arguments and array dimensions in `Signature` attributes. | `signature.rs:122` / `:281`, latched via the sticky `depth_exceeded` flag | `signature.rs::deeply_nested_signature_is_rejected_not_overflow`, `::cached_and_uncached_agree_on_depth_bomb` |
| `MAX_SWITCH_ENTRIES` | 16 384 | `tableswitch.high/low` and `lookupswitch.npairs` are `s4`; without a cap `low = i32::MIN+1, high = i32::MAX` reserves ~8.6 GB. | `instruction.rs:542` / `:591`, plus a remaining-bytes check at `:558` / `:603` | `instruction.rs` switch tests |
| `MAX_STACK_MAP_ENTRIES` | 65 535 | One frame per bytecode offset; derived from `MAX_CODE_LENGTH`. | `stack_map.rs:151` | `stack_map.rs::hostile_frame_count_is_rejected_and_honest_counts_still_parse` |
| `SIGNATURE_CACHE_CAP` | 8192 entries | Bounds the memoized signature cache; FIFO eviction. | `signature.rs` (`SignatureCacheInner::insert`) | `signature.rs::reinsert_keeps_order_and_map_in_lockstep` |
| StackMap absolute offset ≤ `u16::MAX` | 65 535 | A delta run that overshoots is a corrupt file; the accumulator is `u32` + `saturating_add` so a debug build cannot panic before the bound check fires. | `stack_map.rs::absolute_offsets` | `stack_map.rs::absolute_offset_accumulator_boundary` (accept at 65 535, reject at 65 536) |

### 2.3 Per-entry wire sizes

These are the multipliers in every `count × entry_size`. Centralising them
means the multiplier and the parse loop cannot drift apart.

| Constant | Bytes | Structure |
|---|---|---|
| `EXCEPTIONS_ENTRY_SIZE` | 2 | `Exceptions.exception_index_table[]` (JVMS §4.7.5) |
| `LINE_NUMBER_ENTRY_SIZE` | 4 | `LineNumberTable` entry (§4.7.12) |
| `INNER_CLASS_ENTRY_SIZE` | 8 | `InnerClasses.classes[]` (§4.7.6) |
| `LOCAL_VARIABLE_ENTRY_SIZE` | 10 | `LocalVariableTable` / `LocalVariableTypeTable` (§4.7.13, §4.7.14) |
| `METHOD_PARAMETER_ENTRY_SIZE` | 4 | `MethodParameters.parameters[]` (§4.7.24) |
| `EXCEPTION_TABLE_ENTRY_SIZE` | 8 | `Code.exception_table[]` (§4.7.3) |
| `LOCALVAR_TARGET_ENTRY_SIZE` | 6 | `localvar_target.table[]` (§4.7.20.1) |

---

## 3. Audit table — attacker-controlled input driving allocation, multiplication, cast or recursion

Line numbers are as of this change. "Guard before" is what the code did prior
to this work; "guard now" is what it does after.

### 3.1 Allocations sized from a wire count

| Site | Count source | Guard before | Guard now |
|---|---|---|---|
| `class_reader.rs:331` constant pool | `constant_pool_count` (`u2`) | `Vec::with_capacity(count)` — unclamped; 65 535 × `ConstantPoolEntry` ≈ 1.5 MB from a 10-byte file | `ensure_count_fits` at `:322` against `MIN_CONSTANT_POOL_ENTRY_BYTES`; the exact-size reservation is then provably proportional to the input **(added)** |
| `class_reader.rs:166` interfaces | `interfaces_count` (`u2`) | `.min(PREALLOC_CAP)` | `ensure_count_fits` + `bounded_capacity` (2 bytes/entry) **(tightened)** |
| `class_reader.rs:191` fields | `fields_count` (`u2`) | `.min(PREALLOC_CAP)` | `ensure_count_fits` + `bounded_capacity` (8 bytes/entry) **(tightened)** |
| `class_reader.rs:209` methods | `methods_count` (`u2`) | `.min(PREALLOC_CAP)` | `ensure_count_fits` + `bounded_capacity` (8 bytes/entry) **(tightened)** |
| `class_reader.rs:642` attributes | `attributes_count` (`u2`) | `.min(PREALLOC_CAP)` | `ensure_count_fits` + `bounded_capacity` (6-byte header) **(tightened)** |
| `class_reader.rs:257` mUTF-8 decode | `CONSTANT_Utf8` length (`u2`) | `with_capacity(bytes.len())` — bounded by the bytes already read | unchanged; already proportional to input |
| `attribute.rs:1152` `Exceptions` | `u2` | `.min(PREALLOC_CAP)` | `bounded_capacity` (2 bytes/entry) **(tightened)** |
| `attribute.rs:1174` `LineNumberTable` | `u2` | `.min(PREALLOC_CAP)` | `bounded_capacity` (4) **(tightened)** |
| `attribute.rs:1191` `InnerClasses` | `u2` | `.min(PREALLOC_CAP)` | `bounded_capacity` (8) **(tightened)** |
| `attribute.rs:1453` `LocalVariableTable` | `u2` | `.min(PREALLOC_CAP)` | `bounded_capacity` (10) **(tightened)** |
| `attribute.rs:1472` `LocalVariableTypeTable` | `u2` | `.min(PREALLOC_CAP)` | `bounded_capacity` (10) **(tightened)** |
| `attribute.rs:1493` `MethodParameters` | `u1` | `.min(PREALLOC_CAP)` | `bounded_capacity` (4) **(tightened)** |
| `attribute.rs:1707` `Code.exception_table` | `u2` | `.min(PREALLOC_CAP)` | `bounded_capacity` (8) **(tightened)** |
| `attribute.rs:1591` nested attribute table | `u2` | `.min(PREALLOC_CAP)` | `bounded_capacity` (6-byte header) **(tightened)** |
| `attribute.rs` `BootstrapMethods`, `NestMembers`, `PermittedSubclasses`, `Record`, `Module`(+`requires`/`exports`/`opens`/`uses`/`provides`/`*_to`), `ModulePackages`, `LoadableDescriptors`, `Runtime{Visible,Invisible}[Parameter,Type]Annotations`, `element_value` arrays, `type_path` | `u2`/`u1` | `.min(PREALLOC_CAP)` | unchanged — already bounded at 1024 elements, and each loop iteration performs a bounds-checked read that fails immediately on a truncated body. See §5. |
| `attribute.rs:1908` `localvar_target` raw copy | `u2` table length | `Vec::with_capacity(2 + byte_count)` from the *declared* length | reserves from `2 + body.len()`, i.e. from the bytes `read_bytes` actually produced **(tightened)** |
| `stack_map.rs:150` frames | `number_of_entries` (`u2`) | `.min(65535)` only — 65 535 × `StackMapFrame` ≈ 3.6 MB from a 2-byte attribute | `bounded_capacity` with a 1-byte minimum frame **(tightened)** |
| `stack_map.rs:305` verification types | `u2` (`num_locals` / `num_stack` / append count) | `.min(65535)` only | `bounded_capacity` with a 1-byte minimum entry **(tightened)** |
| `instruction.rs:562` `tableswitch.offsets` | `high - low + 1` (`s4` arithmetic) | `MAX_SWITCH_ENTRIES` + `count*4 > remaining` check | unchanged — already correct |
| `instruction.rs:607` `lookupswitch.pairs` | `npairs` (`s4`) | `npairs < 0` reject, `MAX_SWITCH_ENTRIES`, `npairs*8 > remaining` check | unchanged — already correct |
| `quickened.rs:178` decoded op arrays | `code.len()` | derived from the real buffer length, capped at `u32::MAX` | unchanged — proportional to input |
| `jimage.rs:488` whole-image read | file metadata | trusted local file (`modules`), not attacker-supplied over the network | unchanged; see §5 |

### 3.2 Multiplications of a count by an element size

All of these were mathematically safe (a `u2`/`u1` count times a ≤10-byte
element cannot overflow a 32-bit `usize`), but all were written as a bare `*`.
They now go through `checked_span`, so the safety no longer depends on an
invariant a future refactor could silently break by widening a count field.

| Site | Expression before | Now |
|---|---|---|
| `attribute.rs:1150` | `num_exceptions * 2` | `checked_span("Exceptions", …)` |
| `attribute.rs:1173` | `table_length * 4` | `checked_span("LineNumberTable", …)` |
| `attribute.rs:1190` | `num_classes * 8` | `checked_span("InnerClasses", …)` |
| `attribute.rs:1452` | `table_length * 10` | `checked_span("LocalVariableTable", …)` |
| `attribute.rs:1471` | `table_length * 10` | `checked_span("LocalVariableTypeTable", …)` |
| `attribute.rs:1492` | `parameters_count * 4` | `checked_span("MethodParameters", …)` |
| `attribute.rs:1706` | `exception_table_length * 8` | `checked_span("Code exception_table", …)` |
| `attribute.rs:811` (eager shape walk) | `exception_table_length * 8` | `checked_span("Code exception_table", …)` |
| `attribute.rs:1903` | `6 * table_length` | `checked_span("type_annotation localvar_target", …)` |
| `jimage.rs:500` | `table_length * 4` | already `checked_mul` — unchanged |

### 3.3 Narrowing casts of wire values

| Site | Cast | Status |
|---|---|---|
| `class_reader.rs:661` `attribute_length` | `u32 → usize` | now `wire_len_to_usize` **(tightened)** |
| `attribute.rs:1606` nested `attribute_length` | `u32 → usize` | now `wire_len_to_usize` **(tightened)** |
| `attribute.rs:825` nested length (shape walk) | `u32 → usize` | now `wire_len_to_usize` **(tightened)** |
| `attribute.rs:1669` / `:795` `code_length` | `u32 → usize` | now `wire_len_to_usize`, then range-checked against `MAX_CODE_LENGTH` **(tightened)** |
| `buffer.rs:69` `read_i32` | `u32 as i32` | intentional bit-pattern reinterpretation of `CONSTANT_Integer` / branch offsets; no truncation possible |
| `buffer.rs:89` `read_f64` | `i64 as u64` | intentional bit-pattern reinterpretation |
| `constant_pool.rs` `get(index as usize)` (11 sites) | `u16 → usize` | widening; every access is a bounds-checked `slice::get` |
| `verified_code.rs` `pc as u32` / `as usize` (13 sites) | both directions | guarded by the `code.len() > u16::MAX` reject at `verified_code.rs:97` |
| `stack_map.rs::absolute_offsets` | `u32 → u16` | explicit `> u16::MAX` check before every downcast |
| `instruction.rs` `high as i64 - low as i64 + 1` | `i32 → i64` | widening before the subtraction, so `i32::MIN`/`i32::MAX` cannot wrap |
| `jimage.rs` (33 sites) | `u32`/`u64 → usize` | section offsets built with `checked_mul`/`checked_add` and range-checked against `data.len()` at `jimage.rs:518` |

### 3.4 Byte-range arithmetic

| Site | Before | Now |
|---|---|---|
| `class_reader.rs:693` attribute body end | `start + length` | `checked_end` **(tightened)** |
| `attribute.rs:1230`/`:1238` `StackMapTable` view | `body_offset + buf.position()`, `start + length` | `checked_end` on both, then `ByteView::try_new` **(tightened)** |
| `attribute.rs:1532`/`:1536` `Unknown` view | same | `checked_end` on both **(tightened)** |
| `attribute.rs:1687`/`:1695` `Code` bytecode view | same | `checked_end` on both **(tightened)** |
| `buffer.rs:104`/`:119` `read_bytes`/`skip` | already `checked_add` | unchanged |
| `stack_map.rs::read_u16` | already `checked_add` | unchanged |
| `byte_view.rs::try_new` | rejects `start > end` and `end > len` | unchanged; `ByteView::new` (the panicking constructor) is `pub(crate)` and `#[deprecated]` so no downstream crate can reach it |

### 3.5 Recursion

| Recursion | Depth bound | Enforced at |
|---|---|---|
| `decode_attribute_body` ⇄ `decode_attributes_vec` ⇄ `decode_code_body` (`Code`-in-`Code`, `Record`-in-`Record`) | `MAX_ATTRIBUTE_DEPTH` = 16 | `attribute.rs::decode_attribute_body` entry check |
| `decode_annotation_depth` ⇄ `decode_element_value_depth` (`@` and `[` element values) | `MAX_ANNOTATION_DEPTH` = 256 | both functions check on entry |
| `SigParser::parse_type_sig` (nested generics, array dimensions) | `MAX_SIGNATURE_DEPTH` = 256 | `signature.rs:281`, with a sticky `depth_exceeded` flag so a partial parse cannot be returned as success |
| `FieldType::parse_partial_depth` (array dimensions) | `MAX_ARRAY_DIMENSIONS` = 255 | `field_type.rs:89` |
| `MethodDescriptor::parse` | inherits `FieldType`'s bound | `method_descriptor.rs` |

---

## 4. The signature-cache shape bug

### What was wrong

`reader/src/signature.rs` memoizes parsed signatures in one global map. The key
was the signature **string alone**, and the "this string does not parse" verdict
(`ParsedSignature::Invalid`) carried no indication of *which grammar* had
rejected it:

```rust
// before
map: FxHashMap<Arc<str>, ParsedSignature>
```

The three public entry points parse three **different languages** that agree on
some strings and disagree on others:

| String | field signature | method signature | class signature |
|---|---|---|---|
| `Ljava/lang/Object;` | valid | invalid | valid |
| `()V` | invalid | valid | invalid |
| `Lsup;Liface;` | invalid (trailing garbage) | invalid | valid (superclass + one superinterface) |

Because `Invalid` was shape-agnostic, the first shape to reject a string
poisoned the other two. `parse_field_signature_cached("Lsup;Liface;")` cached
`Invalid`; a later `parse_class_signature_cached` on the same string hit that
entry and returned `None` — while the uncached `parse_class_signature` returns
`Some`. The cached and uncached paths silently disagreed.

Impact: any consumer of generic signatures (`ResolvableType`, reflective
`getGenericSuperclass`, JVMTI agents) could see a valid class signature reported
as absent, depending purely on whether some earlier caller had probed the same
string under a different shape. The `Some(_)` "different shape" arm that
existed in each `*_cached` function only handled a cached *success* of the wrong
shape; it never ran for the `Invalid` case, which is the one that aliases.

### The fix

The cache key now carries the shape:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SigShape { Class, Method, Field }
type SigKey = (SigShape, Arc<str>);

map: FxHashMap<SigKey, ParsedSignature>,
order: VecDeque<SigKey>,
```

- `cache_probe` (`signature.rs:655`) takes the shape and looks up `(shape, sig)`.
- Each `*_cached` function probes and inserts under its own shape
  (`signature.rs:667`, `:704`, `:735`).
- `SignatureCacheInner::insert`'s recency-queue dedup compares the shape as well
  as the bytes, so the same string cached under two shapes owns two independent
  FIFO slots and neither can evict the other.
- The `Some(_)` wrong-shape arm is now genuinely unreachable and is kept only as
  a defensive fall-through (re-parse, do not touch the cache).

### Regression tests

All in `reader/src/signature.rs`, each using a signature string unique to
itself so the shared global cache cannot race with sibling tests:

| Test | What it pins |
|---|---|
| `field_invalid_verdict_does_not_poison_class_lookup` | The exact reported bug: parse as a **field** signature first (rejects, caches `Invalid`), then as a **class** signature — which must succeed and return one superinterface. Fails without the fix. |
| `class_invalid_verdict_does_not_poison_method_lookup` | The same aliasing in the class → method direction (`()L…;`). |
| `method_invalid_verdict_does_not_poison_field_lookup` | The method → field direction (`L…;`). |
| `valid_verdict_is_not_shared_across_shapes_either` | The dual: a cached *success* under one shape must not be served to another. |
| `shapes_occupy_independent_cache_slots` | Unit-level — three shapes of one string produce three map entries and three recency slots, and re-inserting one shape refreshes only that shape's slot. |

Every test asserts the uncached verdict as ground truth first, so it cannot
pass by both paths being wrong in the same way.

### The fuzz workaround can now be relaxed

`fuzz/fuzz_targets/fuzz_jni_descriptor.rs` documents this bug in its
"Cache-comparison caveat" (module docs, around line 42) and works around it by
(a) comparing exactly one shape per input and (b) calling
`signature::clear_signature_cache()` immediately before and after the
comparison.

With the shape-keyed cache, **both parts of the workaround are unnecessary for
correctness**:

- The target can compare *all three* shapes on the same input in one iteration.
- The cache flush is no longer needed to prevent a cross-shape verdict leaking
  in; a verdict cached for shape *X* is now invisible to shapes *Y* and *Z*.

Keeping the flush is still reasonable purely as a memory-flatness measure for
long fuzzing runs (the target says so itself), but the *correctness* rationale
in the caveat is obsolete and the caveat paragraph should be deleted. That file
is owned by the fuzzing work-stream and was deliberately not modified here.

---

## 5. Remaining unbounded or weakly-bounded paths

Recorded honestly rather than claimed fixed.

1. **`attribute.rs` tables still guarded only by `PREALLOC_CAP`.**
   `BootstrapMethods` (and its nested `bootstrap_arguments`), `NestMembers`,
   `PermittedSubclasses`, `Record` components, all of `Module`'s sub-tables,
   `ModulePackages`, `LoadableDescriptors`, the annotation vectors, the
   `element_value` array vector, and `type_path` reserve
   `min(count, 1024)` elements without also dividing by a minimum entry size.
   The worst case is ~1024 elements (tens of kilobytes) per table, and each
   loop iteration immediately performs a bounds-checked `read_u16`, so a
   truncated body fails after at most one reservation. This is bounded, but it
   is bounded by a constant rather than by the input length. Converting them to
   `bounded_capacity` is mechanical follow-up work.

2. **Nested-attribute `remaining()` is the outer body's, not the nested body's.**
   `decode_attributes_vec` shares one `ClassFileBuffer` across nesting levels,
   so `buf.remaining()` at a nested level counts bytes past the nested
   attribute's declared end. Every use of it here is an *upper* bound for a
   reservation, so the direction is safe — but it is looser than it looks.

3. **`jimage.rs` reads the whole image into memory** (`jimage.rs:488`,
   `Vec::with_capacity(file_size)`). The `modules` file is a trusted local
   artifact shipped with the runtime, not attacker-supplied input, so this is
   deliberate. If jimage parsing is ever exposed to untrusted archives, the
   header-derived section sizes (already `checked_mul`/`checked_add`-guarded at
   `jimage.rs:500`–`:518`) would need a total-size policy on top.

4. **`FieldType::Object` class names are unbounded in length.** A descriptor's
   `L…;` name is copied into a `String` with no length cap. It is bounded in
   practice by the `CONSTANT_Utf8` `u2` length (64 KiB) when the descriptor
   comes from a class file, but `FieldType::parse` is a public API that also
   accepts strings from elsewhere (JNI `GetMethodID`, agents). No cap is
   enforced; the allocation is proportional to the caller's own input.

5. **Path names.** The P0 item lists "path names" as a fuzz surface.
   `jimage.rs::location_path` builds a `String` whose capacity is the exact sum
   of the four already-read string-table slices, so it is proportional to input.
   JAR/zip path handling lives outside this crate (`vm`/`classloading`) and was
   out of scope for this change.

6. **`signature.rs` accepts some non-JVMS shapes.** `parse_class_signature`
   will accept e.g. `[I` as a "class signature" because it parses a
   `SuperclassSignature` with `parse_type_sig` rather than requiring a
   `ClassTypeSignature`. This is a lenience bug, not a resource-exhaustion one,
   and predates this change; it is noted here because the shape-keyed cache
   tests exercise the boundary between the three grammars.

---

## 6. Test inventory (P1 acceptance)

> *"Boundary corpus covers 32/64-bit limits and fails before allocation/write."*

Every "must reject" test below has a "must accept" twin at the adjacent value,
so the corpus cannot pass vacuously.

**`reader/src/limits.rs`** — the helpers themselves:

- `checked_span_accepts_the_largest_u16_count_at_every_entry_size` — `u16::MAX ×` each of the seven entry sizes.
- `checked_span_rejects_usize_max_times_two` / `_rejects_half_max_plus_one` — `usize::MAX` overflow, with the accepting twin at `usize::MAX / 2`.
- `checked_span_handles_zero_and_one` — zero-length and unit cases, plus a zero entry size.
- `checked_span_at_u32_and_i32_boundaries` — `u32::MAX`, `i32::MAX`, and `i32::MIN`'s bit pattern read as an unsigned length.
- `count_exceeding_remaining_bytes_is_rejected_before_allocation` — 65 535 entries in 40 bytes.
- `count_exactly_filling_the_remaining_bytes_is_accepted` — exact fit, plus off-by-one in both directions.
- `zero_count_always_fits_even_in_an_empty_buffer`.
- `count_fits_rejects_overflowing_product_rather_than_wrapping`.
- `count_fits_treats_zero_entry_size_as_one_byte`.
- `capacity_never_exceeds_what_the_input_can_hold` / `capacity_is_zero_when_nothing_remains` / `capacity_saturates_at_prealloc_cap_not_at_the_declared_count` / `capacity_handles_zero_entry_size_without_dividing_by_zero`.
- `wire_len_conversion_is_exact_at_the_u32_boundaries` — `0`, `1`, `u32::MAX`, `i32::MIN as u32`, `i32::MAX as u32`.
- `checked_end_rejects_wraparound` — `usize::MAX` boundary with both twins.
- `constants_are_internally_consistent`, `constant_pool_floor_is_a_sound_lower_bound`.

**`reader/src/class_reader.rs`**:

- `constant_pool_count_beyond_remaining_bytes_is_rejected_before_allocating` — a 10-byte file declaring 65 535 pool slots; asserts the rejection message is the count-vs-remaining one, i.e. that it fired *before* the pool `Vec` was reserved.
- `constant_pool_count_within_remaining_bytes_is_not_rejected_by_the_size_guard` — must-accept twin.
- `constant_pool_count_of_one_is_the_empty_pool_boundary` — zero-length pool, and the `count == 0` JVMS violation.
- `section_count_guards_reject_impossible_declarations` — `interfaces`/`fields`/`methods`/`attributes` at 65 535, at the exact fit, one past it, and at zero.
- (pre-existing) `read_attributes_rejects_length_exceeding_buffer`, `truncated_class_file_returns_error`, `empty_input_returns_error`.

**`reader/src/attribute.rs`**:

- `table_attributes_reject_a_count_larger_than_their_body` — all six fixed-size tables at `u16::MAX`/`u8::MAX` with an empty body, at exactly one entry (accept), at one-more-than-present (reject), and at zero (accept).
- `code_length_boundaries_are_enforced_in_both_directions` — accepts `code_length == 1`, rejects `0`, `65 536`, `i32::MAX`, `i32::MIN as u32`, `u32::MAX`.
- `code_exception_table_count_is_bounded_by_the_body` — 65 535 entries in an empty table (reject) vs. one entry present (accept).
- `attribute_length_at_u32_extremes_is_rejected_not_truncated` — nested `attribute_length` at `u32::MAX`, `i32::MIN as u32`, `i32::MAX as u32`, `1_000_000`, with an honestly-sized twin.
- `annotation_nesting_is_accepted_up_to_the_cap_and_rejected_one_past_it`.
- `type_path_and_localvar_target_counts_stay_bounded`.
- (pre-existing) `deeply_nested_element_value_array_is_rejected`, `deeply_nested_code_attribute_is_rejected`, `deeply_nested_record_attribute_is_rejected`.

**`reader/src/stack_map.rs`**:

- `hostile_frame_count_is_rejected_and_honest_counts_still_parse` — `u16::MAX` frames in a 2-byte attribute, three real frames (accept), off-by-one, empty table, truncated and empty inputs.
- `full_frame_type_counts_are_bounded_by_the_remaining_bytes` — both `num_locals` and `num_stack` at `u16::MAX`, with an accepting twin and a zero-length twin.
- `absolute_offset_accumulator_boundary` — accepts exactly `u16::MAX`, rejects `u16::MAX + 1`, accepts the largest legal pair, and rejects a 64-frame maximal-delta run.
- (pre-existing) `read_u16_overflow_returns_error`, `read_u16_boundary`.

**`reader/src/buffer.rs`** (pre-existing): `read_bytes_overflow_returns_error`
drives `position` to `usize::MAX - 1` and asserts `checked_add` fires before
the bounds check.

**`reader/src/field_type.rs`** (pre-existing):
`deeply_nested_array_is_rejected_not_overflow` — accepts exactly 255
dimensions, rejects 256 and 100 000.
