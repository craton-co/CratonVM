# Value representation and compressed oops

*Session: `value-repr-and-compressed-oops`, 2026-07-26. Base:
`arch/wave1-integration-20260726` merged at `473bb3f93`.*

CratonVM objects cost 2-3x their HotSpot equivalents. This note establishes
where that cost actually is, what compressed oops do and do not buy against a
32-byte header, exactly what is left before they can be turned on, and how the
interpreter's per-slot type tag can be removed now that verification retains
what it proves.

Three claims in this area were wrong before this pass, in both directions. They
are corrected below with the evidence.

---

## 1. Compressed oops: verified state

### 1.1 What was believed, and what is true

`ARCHITECTURE.md` used to say compressed oops were "implemented and tested but
not wired into the live heap", with "narrow-oop field layout, JIT load/store
barriers, GC root re-encoding, and klass-pointer compression" remaining. A
concurrent truth-pass this wave already corrected the *wiring* half of that
(`docs/internal/arch-2026-07-26/docs-truth-pass.md` §6). The **remaining-work
list itself had never been checked**, and it is wrong in three separate ways.

| Listed as remaining | Reality |
|---|---|
| Narrow-oop field layout | **Done.** `FieldStorageKind::size_runtime` / `alignment_runtime` (`types/src/field_layout.rs:94`, `:104`) drive `build_compact_layout` (`classloading/src/class.rs:997`), and `class_manager.recompute_all_compact_layouts()` re-lays every bootstrap class at init (`vm/src/vm/vm_init.rs:883`). |
| JIT load/store barriers | **Half done, and the half that is missing is not a barrier.** `aaload`/`aastore` already emit real narrow code (`jit/src/x64.rs:17132`, `:17173`). The compact `getfield`/`putfield` fast paths *bail out* instead (`x64.rs:2119` + four call sites). One emitter narrows nothing and is not gated at all — see §1.2. |
| GC root re-encoding | **Must never be done.** Frame locals, operand-stack slots and JIT stack spills are not heap slots. They are bounded by stack depth, not live-set size, so narrowing them buys no footprint and costs an encode/decode on every `aload`/`astore`. `vm/src/jit/conservative_roots.rs` and `vm/src/jit/xt_root_scan.rs` walk 8-byte words and are already correct. Actioning this item would introduce a bug. |
| Klass-pointer compression | **Nothing to compress.** `ObjectHeader::class_id` is already a `u32`. `CompressedOops::encode_klass` / `decode_klass`, `NarrowKlass` and `CompressedOopArray` have zero callers workspace-wide and exist only for their own unit tests. |

The load-bearing correction: **the gate is not off for throughput reasons.**
`gc/src/compressed_oops.rs` itself claimed "a throughput regression, not
incompleteness". A sweep of every reference-slot access in the workspace found
two correctness holes on the only backend the gate permits.

### 1.2 Blockers — both CLOSED 2026-08-06

> **Status update (2026-08-06).** B1 and B2 below were both closed while
> retiring `docs/internal/beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`.
> The descriptions are kept verbatim because they are the derivation; the
> resolution is recorded under each. `gc/src/compressed_oops.rs`'s header and
> `warn_known_unsound`'s stderr text were updated in the same change.

**B1. `jit/src/x64.rs:14639` `emit_load_string_value_ptr`.**
Emits an unconditional 64-bit `MOV dst, [base + compact_offset]` for the
`String.value` `byte[]` reference field. The `getfield`/`putfield` arms are
gated on `narrow_oops_block_inline_fields()` (`x64.rs:16495`, `:22032`,
`:22497`); this one is not. Under narrow oops it loads 4 bytes of narrow oop
plus 4 bytes of the adjacent `coder`/`hash` field and dereferences the result.
Ten call sites, all in the `STRING_ACCESS` / `STRING_SEARCH` intrinsic regions
(`x64.rs:25018, 25067, 25273, 25278, 25416, 25423, 25549, 25647, 25654`), so
every inlined `charAt` / `length` / `isEmpty` / `hashCode` / `indexOf` /
`equals` / `compareTo` is a deterministic wild-pointer SIGSEGV. The *offset* is
already narrow-correct — `StringFieldLayout::new` (`jit/src/lib.rs:3175`)
resolves it through `cratonvm_types::compact_field_slot` — only the width and
decode are wrong.

*One-line unblock:* short-circuit `try_resolve_string_intrinsic`
(`jit/src/lib.rs:3931`) on `narrow_oops_enabled()`, losing the intrinsics.
*Real fix:* a narrow arm in the emitter, mirroring `emit_narrow_ref_aload_regs`
(`x64.rs:17132`).

**B2. `gc/src/gen_heap.rs:8261` `mark_young_to_old_refs` and `:8369`
`rewrite_stretch_conservatively`.**
Both scan an unparseable heap stretch in aligned 8-byte words looking for
old-gen object bases. A pair of adjacent narrow oops never matches a base, so
marks are missed (premature reclamation) and references to moved objects are
left unrewritten (dangling). These are fallback paths — but they are the paths
that run when the parseable walk has already failed, i.e. precisely when
correctness matters.

**RESOLVED 2026-08-06 (B1).** `emit_load_string_value_ptr`
(`jit/src/x64/objects.rs`) has a narrow arm, `emit_load_narrow_ref_field`,
mirroring `emit_narrow_ref_aload_regs`. It is selected per call site by a new
`StringFieldLayout::value_compact_is_narrow`, which is true only when a
`CompactLayout` was actually registered for the String class — the
no-registered-layout fallback points `value_compact_offset` at the LEGACY
16-byte cell payload, which is never narrowed and must keep its wide load. The
emitter borrows R11 for the rebase inside a balanced `PUSH`/`POP` (the register
allocator has already parked values in the extended registers at these sites,
unlike the array emitters, and the pair straddles no `CALL` and no RSP-relative
access). The one-line refusal in `try_resolve_string_intrinsic` — and the
throughput it cost — is gone. Verified: `StringHot`, a hot loop over
`charAt`/`length`/`isEmpty`/`hashCode`/`indexOf`/`equals`/`compareTo` over
LATIN1, UTF-16 and empty receivers, is byte-identical to HotSpot under
`CRATONVM_COMPRESSED_OOPS=1`.

**RESOLVED 2026-08-06 (B2).** Both scans take a second pass over the stretch at
4-byte granularity under `narrow_oops_enabled()`, decoding each half as a narrow
oop. The mark pass is unconditionally safe — over-marking always was, which is
the property the wide pass already relies on, and the base validation is what
stops a coincidence from writing a mark bit into a live object's payload. The
rewrite pass inherits the wide pass's false-positive trade and *more* of it (32
bits of entropy instead of 64), which is accepted for the same reason — an
unrewritten reference is a certain dangling pointer — and is bounded by
`oldgen_compact_enabled` being off by default, so `compact_map` is empty and
neither pass runs at all unless `CRATONVM_OLDGEN_COMPACT=1` is set.

**Conclusion: compressed oops must still stay off**, but for §1.3's reason
(G1/ZGC unmigrated behind a load-bearing backend check), not for §1.2's — and
for a second reason §1.2 was hiding: **it is worth 4.7 %, not 20-30 %.**
Measured 2026-08-06, interleaved A-B-B-A on spring-beans
`BeanRegistrationsAotContributionTests`, peak RSS 1850 MB wide vs 1763 MB
narrow, wall time indistinguishable. The throughput cliff this page's earlier
measurement recorded (wide finished in 1677 s, narrow did not finish inside
2700 s) was the B1 stopgap refusing the String intrinsics, not compression.
That matters for §2's ordering: with the reference width worth only ~5 %, the
32-byte-to-16-byte `ObjectHeader` shrink is the item with the leverage, and
this one is a prerequisite for it rather than a win on its own.
`enable_for_live_heap` still warns on the opt-in path; the text now names what
is actually left.

### 1.3 Held off only by the backend check

`gc/src/g1.rs` is entirely unmigrated — roughly twenty sites including the main
mark loop (`:5211`, whose comment still reads "Reference array: 8-byte compact
slot per element"), every evacuation path (`:583`, `:707`, `:3777`, `:3920`,
`:7990`), remembered-set construction (`:1994`, `:4077`), the flat-object
read/write helpers (`:94`, `:113`) and `get_array_element` / `set_array_element`
(`:7392`, `:7443`). `gc/src/zgc.rs:1788` misses the instance-field arm (its
array arm was migrated). `gc/src/region.rs:957`/`:993` likewise, though
`RegionHeap` is not re-exported from `gc/src/lib.rs`.

None of these is reachable, because `vm/src/vm/vm_init.rs:862` refuses the gate
unless the backend is `Generational`. **That check is load-bearing and must not
be relaxed** before G1 and ZGC are migrated. It is worth a comment at each of
those files' tops; today the coupling is only stated in `vm_init.rs`.

### 1.4 Verified-safe things that look like bugs

`native-builtins/src/lib.rs:25404` `array_index_scale_for_name` reports **8** for
reference arrays and `arrayBaseOffset` reports **16**
(`unsafe_natives_ext.rs:1884`). Under real compressed oops HotSpot reports 4, so
this looks like an obvious narrow-oop bug. It is not. CratonVM's `Unsafe` array
model is a self-consistent fiction: the synthetic byte offset is never
dereferenced, `unsafe_array_index_from_offset` (`unsafe_natives_ext.rs:1401`)
decodes it back to an element index with the *same* hardcoded scale table, and
the actual access then goes through the narrow-aware `get_array_element`.
Changing the reported scale to 4 without changing the decoder in lockstep would
break `ConcurrentHashMap` and `AtomicReferenceArray`, which do all their table
access through `Unsafe` with `ASHIFT` derived from `arrayIndexScale`.

Similarly, `System.arraycopy` needs no narrow work: the JIT intrinsic deopts on
reference-element arrays (`x64.rs:23617`), `vm_exec.rs:5538 bulk_array_copy`
bails on `ArrayElementType::Reference`, and `native-builtins/src/lang_system.rs:76`
goes per-element. Reference-field CAS is likewise safe — `compare_and_swap_field`
(`vm_exec.rs:8477`) runs under `with_cas_lock` through the width-agnostic
`get_field_volatile` / `set_field_volatile`, so there is no 64-bit CAS landing on
a 4-byte slot.

### 1.5 Ordered wiring plan

Files marked *(not mine)* were specified, not landed, in this session.

**Phase 0 — landed here**
0.1 `gc/src/compressed_oops.rs` header rewritten with the verified state, the
    two blockers, the three not-needed items and the priority order.
0.2 `enable_for_live_heap` warns on stderr that the run has known correctness
    holes (§1.2). Not a new gate; the existing opt-in made honest.
0.3 `estimate_savings_percent` no longer returns HotSpot's 25-30 % figure,
    which assumes a 12-byte header. See §2.

**Phase 1 — close the correctness blockers**
1.1 *(not mine — `jit/`)* B1. Either gate `try_resolve_string_intrinsic`
    (`jit/src/lib.rs:3931`) on `narrow_oops_enabled()`, or add a narrow arm to
    `emit_load_string_value_ptr` (`jit/src/x64.rs:14639`).
1.2 *(not mine — `gc/src/gen_heap.rs`)* B2. Make the conservative stretch scan
    at `:8261` and the rewrite at `:8369` step by `ref_element_size()` and
    decode through `narrow_oop::decode` when narrow oops are on.
1.3 *(not mine — `jit/src/x64.rs:17141`, `:17178`)* Replace the hardcoded
    `SHL/SHR 3` in the narrow array emitters with `narrow_shift()`, or assert
    `narrow_shift() == 3` there. Correct today only because
    `enable_for_live_heap` pins shift 3, while
    `CompressedOops::determine_shift` returns 0 under 4 GiB.
1.4 *(not mine — `vm/src/runtime/serviceability.rs:1460`)* The hprof *instance*
    dump reads ref fields as `*const u64` and emits garbage object ids; switch
    to `read_ref_slot_unaligned`, as the array dump at `:1548` already does.

**Phase 2 — recover the throughput the gate costs**
2.1 *(not mine — `jit/src/x64.rs`)* Narrow arms for the compact `getfield`
    (`:22113`) and `putfield` (`:22569` old-value test, `:22583` store;
    `emit_inline_body_compact_ref_putfield` `:14878`/`:14894`;
    `emit_inline_fresh_ctor_compact_ref_putfield` `:14950`). The SATB
    old-value null test can stay a 4-byte `CMP DWORD [..], 0` — no decode
    needed to test for null.
2.2 *(not mine)* Delete `narrow_oops_block_inline_fields` (`x64.rs:2119`) and
    its four call sites so no future inline field path re-inherits the bail.

**Phase 3 — unblock the other collectors**
3.1 *(not mine — `gc/src/g1.rs`)* Migrate all ~20 sites to
    `read_ref_slot`/`write_ref_slot`/`ref_element_size`. Start with
    `for_each_flat_object_reference` (`:94`) and `write_flat_object_reference`
    (`:113`), which most visitors funnel through.
3.2 *(not mine — `gc/src/zgc.rs:1788`)* Instance-field arm of
    `enumerate_references`.
3.3 Only then may `vm/src/vm/vm_init.rs:862` accept a non-generational backend.

**Phase 4 — coverage, then flip**
4.1 *(not mine — `jit/tests/`)* There is no narrow-oop JIT test at all. Add
    fixtures that `narrow_oop::enable(base, 3)` before compiling and exercise
    `aaload`/`aastore`, compact `getfield`/`putfield`, the String intrinsics
    and inline TLAB `new`, plus a GC-stress run proving the conservative frame
    sweep still finds every root.
4.2 Note that several existing tests assert `element_byte_size(Reference) == 8`
    (`types/src/heap_types.rs:586`, `:712`, `types/src/lib.rs:121`,
    `gc/src/heap.rs:2022`, `vm/src/vm.rs:52837`). They fail if any test in the
    same process enables narrow oops — the new fixtures need process
    isolation, or those assertions need to move behind
    `narrow_oops_enabled()`.
4.3 Flip `use_compressed_oops` to default-true only after 4.1 passes and the
    warning in 0.2 is deleted.

---

## 2. The honest footprint arithmetic

`ObjectHeader` is 32 bytes (`types/src/heap_types.rs:18`): `class_id` u32,
`kind`/`element_type`/`gc_age`/`gc_flags` 4 bytes, `identity_hash_code` i32,
`shape` u32, `forwarding_ptr` 8, `mark_word` 8. HotSpot's is 12.

Object bodies use the compact layout (on by default,
`CRATONVM_COMPACT_REF_FIELDS`), so each field takes its natural width, and the
body is rounded to the 8-byte object grid. Narrow oops take references from 8
to 4. Working the actual shapes:

| object | wide | narrow | saving |
|---|---|---|---|
| `java.lang.Integer` (autoboxed ⇒ legacy 16-byte cell) | 48 | 48 | **0 %** |
| `java.lang.String` `{byte[] value, byte coder, int hash}` | 48 | 48 | **0 %** |
| `ArrayList` `{Object[], int, int}` | 48 | 48 | **0 %** |
| binary-tree node `{left, right}` | 48 | 40 | 16.7 % |
| `HashMap.Node` `{int hash, K, V, next}` | 64 | 48 | 25 % |
| `Object[16]` | 160 | 96 | 40 % |
| `Object[n]`, large `n` | `32+8n` | `32+4n` | → 50 % |

Two things fall out, and both matter more than the headline number.

**Reference arrays are where compressed oops pay, and they pay a lot.** The
saving there is not eroded by rounding and approaches 50 %. That is directly
relevant to the 21.2x HashMap row: the table is an `Object[]` and gets close to
half its bytes back, while each `Node` gets 25 %.

**On small objects the 8-byte object alignment eats the saving entirely.**
`String` drops from 13 body bytes to 9 — both round to 16. `ArrayList` drops
from 16 to 12 — both round to 16. Boxed primitives have no reference fields at
all and save nothing by construction. These are among the most numerous objects
in any real heap. The single-reference case is the common case, and it is
exactly the case where narrow oops save zero.

So the honest expected saving on a mixed Java heap is **well under HotSpot's
published 20-30 %** — that range assumes a 12-byte header, where the same
absolute bytes are a much larger fraction. `estimate_savings_percent` now
reports a 50 % *ceiling* with the derivation in its doc comment, rather than a
fabricated point estimate, and
`documented_object_footprint_arithmetic_holds` computes the table above so the
prose cannot drift from the numbers.

### 2.1 Interaction with the 32 → 16 byte header shrink

A sibling is separately scoped to fold `forwarding_ptr` and
`identity_hash_code` into the mark word, halving `HEADER_SIZE`.

**They do not touch the same code.** Narrow oops change body widths via
`ref_field_size()`; the header shrink changes `HEADER_SIZE`,
`FORWARDING_PTR_OFFSET`, `MARK_WORD_OFFSET` and the header writers. The JIT
bakes `HEADER_SIZE + offset` as a compile-time constant read from
`cratonvm_types`, so it follows automatically in both cases.

**They are not independent in payoff.** Applying the same table with a 16-byte
header:

| object | today | narrow only | header only | both | HotSpot |
|---|---|---|---|---|---|
| tree node `{left, right}` | 48 | 40 | 32 | 24 | 24 |
| `HashMap.Node` | 64 | 48 | 48 | 32 | 32 |
| `String` | 48 | 48 | 32 | 32 | 24 |
| `Integer` | 48 | 48 | 32 | 32 | 16 |

The header shrink is worth roughly twice what narrow oops are worth on ordinary
objects, and unlike narrow oops it helps *every* object including the three
that narrow oops help by 0 %. Together they reach HotSpot parity on the two
shapes that dominate the regressed benchmarks.

**The header shrink should land first**, for four reasons:

1. It has no pending correctness blockers. Narrow oops have two (§1.2).
2. It is unconditional — no gate, no backend restriction, no JIT bail-out. It
   improves the default path immediately, which is the only path the benchmark
   rows measure.
3. It is larger. Measuring narrow oops on top of a 32-byte header risks
   attributing a real-but-small effect to noise and concluding, wrongly, that
   compressed oops are not worth finishing.
4. Narrow oops' remaining work is concentrated in `jit/src/x64.rs`, the
   busiest and most contended file in the repository. Sequencing it after a
   change that touches `types` and `gc` reduces the collision surface.

**Two couplings to respect when the header shrinks:**

* A **forwarding pointer folded into the mark word must stay a full 64-bit
  address.** It is a heap address and would be tempting to narrow — do not.
  `assert_region_encodable` and the GC fixup paths assume a decoded address,
  and the mark word's low state bits (`MARK_STATE_MASK`) leave no room for a
  shifted encoding plus a tag.
* The inflated-monitor pointer (`INFLATED_PTR_MASK`) points into the Rust
  allocator, not the Java heap. It is outside the narrow-oop window entirely
  (`enable_for_live_heap` anchors the base 8 GiB below the lowest heap region)
  and must never be encoded. `narrow_oop::encode` would panic on it, which is
  the correct outcome but a confusing one to debug.

---

## 3. Untagged slots: removing the parallel `kinds` array

### 3.1 The problem, and why the obvious fix is closed

`vm::runtime::value_stack::ValueStack` carries `slots: Vec<CompactValue>` plus a
parallel `kinds: Vec<u8>`; `vm::runtime::frame::Frame` mirrors it with
`local_kinds`. Both exist for exactly one consumer — the GC root scan — and for
one reason: a 64-bit `long` uses all 64 bits, so NaN-boxing has no bits left to
tag it with. `CompactValue::int(0)` and
`CompactValue::long(0xFFFC_0000_0000_0000)` are bit-identical.

A sibling proved this cannot be fixed by making `CompactValue` smarter; five
round-trip tests now pin it. The history is instructive: CratonVM *used* to
re-tag colliding longs into `SUB_LONG_LO`/`SUB_LONG_HI`, which was lossy and
corrupted `BouncyCastle`'s `LongArray.modSquare` (the `0xfffd…` shape). The
encoder now stores longs verbatim, which is spec-correct and makes the
collision structural.

### 3.2 The static fix

Bytecode verification already proves, at every instruction start, which local
slots and which operand-stack slots hold a reference. `classloading/src/type_maps.rs`
(landed this wave) retains it: `local_oops_at(pc)`, `stack_oops_at(pc)`,
`stack_depth_at(pc)`, `frame_map_at(pc)` — `O(log n)`, allocation-free,
lock-free, panic-free, explicitly safe to call from a stop-the-world root scan.

Given the map, the slot needs no tag: the GC reads the oop bit, and the
interpreter reads the type from the opcode it is executing.

### 3.3 What landed: `RawSlot` (`types/src/value.rs`)

`RawSlot` is a `#[repr(transparent)]` `u64` with **no marker bits at all**. The
encoding is identical to the value half of `encode_value`'s `(u64, u8)` pair, so
a consumer migrating from `(vals, tags)` storage keeps its bit patterns and only
changes where the tag comes from.

| Java type | 64 bits hold |
|---|---|
| `int` | the `i32`, zero-extended |
| `long` | the `i64`, **verbatim, all 64 bits** |
| `float` | `f32::to_bits`, zero-extended |
| `double` | `f64::to_bits`, verbatim including NaN payloads |
| reference | a **bare pointer**; `0` is `null` |
| `returnAddress` | the pc, zero-extended |
| uninitialized | `0` |

Two properties do the work:

* **`RawSlot::from_long(x).bits() == x as u64` for every `x`.** `CompactValue`'s
  collision is structural — it must carve tag patterns out of the same 64 bits
  the payload needs. `RawSlot` reserves none, so there is nothing to collide
  with. `rawslot_long_is_bit_exact_for_every_tag_pattern` checks twelve hostile
  patterns including the NaN-box marker and the BouncyCastle `lxor` shape.
* **A reference is a bare pointer, not NaN-boxed.** A moving collector relocates
  a root with `set_oop` — a plain store — instead of rebuilding a NaN box
  through `update_object_ptr`, which can fail.

The API: `SlotType {Int, Long, Float, Double, Reference, ReturnAddress,
Uninitialized}` with `from_descriptor_byte`; constructors and accessors;
`oop()` / `set_oop()` for the GC; `decode(SlotType)` and
`decode_by_descriptor(u8)`; `encode(Value) -> (RawSlot, SlotType)`; and
`to_compact(SlotType)` / `from_compact(cv, SlotType)` so a frame can be
converted one array at a time.

`CompactValue`'s in-memory layout is untouched, as required: the frame pool and
the three GC scanners that transmute `Vec<u64> ↔ Vec<CompactValue>` keep
working, and `RawSlot` is layout-identical to both so the same pooled buffers
back untagged frames with no reallocation
(`rawslot_is_layout_compatible_with_u64_and_compact_value`).

### 3.4 Where the throughput actually comes from

Deleting one byte per slot is the small part. The real win is what `oop()` does
*not* do. The current tagged root scan, per slot, runs:

1. `plausible_heap_pointer` — three ALU ops, cheap;
2. `object_ref_payload_is_known` — a two-level atomic bitmap probe: two
   dependent loads plus a bit test, over 2 MiB of leaf per GiB of address space
   that ever hosted an object. In a root scan that is a near-guaranteed cache
   miss, and it is on the path *per slot*;
3. degradation bookkeeping on the reclassification path.

All three are heuristics substituting for the type information the verifier
already had. `RawSlot::oop()` does none of them — the map is the authority, so
there is nothing to guess and nothing to degrade *to*. A `debug_assert!` catches
a caller whose map and frame have drifted apart.
`rawslot_decode_reference_needs_no_provenance_record` pins the difference: an
address this process never constructed an `ObjectRef` for is rejected by
`decode_value` and accepted by `RawSlot`.

`decode(SlotType::Reference)` keeps the pure-ALU `plausible_heap_pointer` guard,
because that path hands an `ObjectRef` to code that will dereference it and a
corrupted frame must degrade to null rather than fabricate a wild pointer. Only
the memory-touching probe is dropped.

### 3.5 Consumer changes — specified, not landed

All in `vm/` (a sibling's files this wave).

1. **`vm/src/memory/roots.rs`** — root scan. Replace the `kinds`/`local_kinds`
   consultation with `MethodTypeMaps::frame_map_at(frame.last_instr_pc)`. Obey
   the two clamping rules `type_maps.rs:575-593` states as mandatory:
   `FrameOopMap::locals_for_runtime_len` (`Frame::locals` can be *longer* than
   `max_locals` — see `effective_max_locals`) and
   `FrameOopMap::stack_for_runtime_depth` (the interpreter pops an invoked
   method's arguments before pushing the callee frame, so the caller's runtime
   depth is below the verifier's depth at the invoke pc). Check
   `locals_fully_described()`; scan conservatively when it is false, and when
   `frame_map_at` returns `None` — `None` means *unproven*, never *no
   references*.
2. **`vm/src/runtime/value_stack.rs`** — change `slots: Vec<CompactValue>` to
   `Vec<RawSlot>` and delete `kinds`, `KIND_UNKNOWN`/`KIND_LONG`/`KIND_DOUBLE`
   and `kind_of_value`. `push`/`pop` become untyped stores; the typed pushes
   (`push_long`, `push_double`, …) keep their names but stop writing a kind
   byte. `compact_vec_to_u64` and the pool round-trip are unaffected —
   `RawSlot` has the same layout.
3. **`vm/src/runtime/frame.rs`** — same for `locals` / `local_kinds`. This also
   removes the reason `Frame` needs the `Vec<u8>` half of the pool tuple.
4. **Boundaries.** Every place that currently calls `to_value()` on a slot
   needs a type source. Interpreter opcode handlers have it implicitly;
   call/return and field access use `decode_by_descriptor`. Migrate one array
   at a time via `to_compact` / `from_compact` rather than in one commit.

### 3.6 The one thing the maps cannot do

`MethodTypeMaps` records **oop-vs-not** plus stack depth. That is exactly, and
only, what the GC needs — it must distinguish "reference" from "everything else"
and nothing finer. So items 1-3 above can proceed today.

It is *not* enough to reconstruct a fully-typed `Value` at an arbitrary pc,
because the maps do not separate `int` / `long` / `float` / `double`. Every
consumer listed in §3.5 has a better source (opcode, descriptor). A consumer
with no such context — JVMTI local-variable inspection, a debugger, a heap-dump
frame walker — cannot be served.

Closing that needs a 2-bit-per-slot kind table in `classloading/src/type_maps.rs`
alongside the 1-bit oop table (`Int32 | Int64 | Float32 | Float64`; references
are already covered by the oop bit). At the module's own stated cost model that
is +2 bits per slot per recorded pc, roughly tripling row size for locals and
stack — a real cost for 15k classes, and not worth paying until a consumer
needs it. **Deliberately not attempted here.** It is the correct follow-up if
JVMTI local-variable support is ever prioritised.

---

## 4. `intern.rs`

`types/src/intern.rs` is the *symbol* pool — class, method, field and descriptor
names. It is not the Java `String` literal pool; that is
`vm/src/vm/realms/heap_realm.rs:46` `string_pool: RwLock<FxHashMap<String, ObjectRef>>`.

### 4.1 Locking — no global mutex, and no change needed

The pool is a `parking_lot::RwLock<FxHashMap<Arc<str>, ()>>` with a read-guard
fast path and FxHash rather than SipHash. Both are correct choices already made
by an earlier audit. There is a single global lock word, so a Spring-scale
startup interning on eight loader threads will contend on it, and sharding by
hash would remove that. I did **not** land sharding: it is unmeasured, this
session cannot build or benchmark, and the call sites are class-loading and
first-resolution paths (`vm/src/vm/vm_exec.rs:8330`, `:8374`,
`vm/src/runtime/interpreter.rs:18948`, `vm/src/native/jni.rs:2177`) that are
memoized afterwards — not per-invoke. Recommend measuring before changing.

### 4.2 Unbounded growth — a real leak with a stale justification

The module documented never-freeing as "intentional because JVM class metadata
lives for the entire VM lifetime". **That premise no longer holds.** CratonVM
unloads classes (`gc/src/class_unloading.rs`, with
`vm/tests/class_loader_unload_regression.rs` added this wave), and a
Spring/Mockito/ByteBuddy suite generates thousands of synthetic proxy, lambda
and CGLIB classes. Each contributes a class name, method names and descriptors
that outlive the class forever. The growth is proportional to *generated* class
count, not live class count, and is unbounded.

Nothing could reclaim them, and one specific thing made reclamation impossible:
the free `intern(&str) -> &'static str` transmutes a borrow of a pool-owned
`Arc<str>` to `'static`. Any eviction would dangle it.

Landed:

* **`StringPool::prune_unreferenced() -> usize`.** Under the write lock, retains
  only entries with `Arc::strong_count > 1`. A count of 1 means the pool is the
  sole owner, so nothing can observe the removal. The write lock excludes every
  reader, and `Arc::clone` increments before the clone exists, so observing 1
  under exclusion proves no clone exists — the direction that matters. Observing
  a stale 2 merely defers the entry to the next prune.
* **`intern()`'s backing `Arc` is pinned** in a separate never-cleared `PINNED`
  list, so its `&'static str` promise no longer depends on the map entry
  surviving. `pinned_len()` exposes the count.

The pinning costs nothing, because a workspace-wide search found **`intern()` has
no production callers at all** — every VM, classloading and JIT call site uses
`intern_arc`. `intern` is reachable only from this module's own tests and the
`lib.rs` re-export. It should probably be deleted outright; that is a
public-API removal and was left for a separate decision. `pinned_len()`
returning non-zero in production is the tripwire that someone reached for it.

Tests cover the eviction contract, 2000-class generated-name churn, prune →
re-intern, idempotence, and that a `&'static str` survives a global prune.

**Consumer change, specified not landed:** call
`cratonvm_types::global_pool().prune_unreferenced()` from the class-unloading
sweep in `gc/src/class_unloading.rs` (or from the GC pause that drives it) —
`O(n)` under the write lock, so a pause or unload sweep, never a hot path.

### 4.3 The Java string pool is a different, unaddressed problem

`heap_realm.rs:46`'s `string_pool` maps `String -> ObjectRef` and *is* a strong
GC root (`vm/src/memory/roots.rs:255-259`, with the update counterpart at
`vm/src/memory/gc.rs:653`). Entries are never removed. Every `ldc` string
constant and every `String.intern()` result is immortal, exactly as in HotSpot
before `-XX:+UseStringDeduplication` / weak interned-string references. HotSpot's
string table holds **weak** references and clears entries whose strings become
unreachable; CratonVM's holds strong ones. For a long-running server that
interns dynamic content this is an unbounded heap leak, and it is a separate,
larger piece of work (the entries must become weak roots, which requires the
collector to support weak-root clearing over that table). Not in scope for this
session's files; recorded here so it is not lost.

---

## 5. Summary of what landed

| File | Change |
|---|---|
| `types/src/value.rs` | `RawSlot` + `SlotType` (§3.3) and 11 tests |
| `types/src/lib.rs` | re-export `RawSlot`, `SlotType` (one line — without it the new API is unreachable outside the crate) |
| `types/src/intern.rs` | `prune_unreferenced`, `intern` pinning, `pinned_len`, corrected lifetime docs, 5 tests |
| `gc/src/compressed_oops.rs` | verified-state header, unsoundness warning on the opt-in path, honest savings model, 2 tests |

Nothing was turned on. Compressed oops remain off **because they are unsound**,
not because nobody flipped the switch — see §1.2. The header shrink should land
before them (§2.1).
