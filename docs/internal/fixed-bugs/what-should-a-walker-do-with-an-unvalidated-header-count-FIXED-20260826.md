# What should a GC walker do with a header count it cannot validate?

**Status: ANSWERED and IMPLEMENTED, 2026-08-26.** Filed 2026-08-24 as an open
design question out of
`known-issues/hibernate/hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md`
§2.6, which fixed one instance and deliberately left the general case alone
rather than change fourteen call sites blind.

The answer is **D then C**, exactly as §4 recommended — with one correction to
the reasoning that made D look risky at the time, and two residuals the original
survey never looked at.

---

# Part I — the answer

## 1. The premise that blocked D was false

The 2026-08-24 census landed as *counted, not refused*, and said so in
`FLAT_WALK_GIVEN_ARRAY`'s own doc comment:

> Refusing would be a behaviour change, and this helper has fifteen call sites
> of which several do not visibly pre-branch on kind — if any of them relies on
> this walk to reach an array's references, skipping would silently drop
> marking work, which is a worse defect than the one being guarded.

That claim was never checked. **It is false.** All fifteen call sites
pre-branch on `header.kind() == ObjectKind::Array` and run their own 8-byte
element walk in that arm. In file order:

| # | caller | its array arm |
|---|---|---|
| 1 | `G1ParallelEvacuator::process_object` | `HEADER_SIZE + i*8`, forwards each element |
| 2 | `G1ParallelEvacuator::seed_source_region` | same |
| 3 | `record_outgoing_rset_edges` (the capped one) | `holder_walkable_slots`-clamped |
| 4 | `scan_and_evacuate_refs` | `holder_walkable_slots`-clamped |
| 5 | `scan_source_region_for_cset_refs` | `holder_walkable_slots`-clamped |
| 6 | `collect_outgoing_cross_region_edges` | `ARRAY_DATA_OFFSET + k*8` |
| 7 | `rset_completeness_counts` | same |
| 8 | `verify_no_dangling_into_cset_within` | same |
| 9 | `dbg_verify_no_unrewritten_forward` | same |
| 10 | `dbg_scan_for_zeroed_refs` | same |
| 11 | `dbg_verify_reachable_integrity` | same, plus its own extent clamp |
| 12 | `debug_assert_no_reference_into_spans` | same |
| 13 | `update_object_refs` | same |
| 14 | `assert_rset_covers_every_cross_region_edge` | same |
| 15 | `dbg_root_census` | same — but see §5: it was not going through the helper at all |

So **no caller reaches the flat walk for an array on purpose.** Refusing drops
no marking work anyone was relying on, because every arm that wants an array's
references already has one. The hedge was protecting against a caller that does
not exist.

The check that would have settled it is fourteen `sed -n` windows. It cost less
than the paragraph that argued for not doing it.

## 2. What D actually means once the premise is corrected

If every caller pre-branches on kind and the helper *still* sees `Array`, then
the kind the caller read and the kind the header reports are **not the same
kind** — a torn or concurrently-rewritten header, which is precisely §3's
kind-confusion hazard arriving through the only door it has.

In that state the flat walk is wrong whatever it does, so *not doing it* is the
cheap correct answer:

```rust
if header.kind() == ObjectKind::Array {
    let n = FLAT_WALK_REFUSED_ARRAY.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 8 || n.is_power_of_two() { tracing::warn!( /* … */ ); }
    return;
}
```

One tag comparison, on a tag already loaded, at the single choke point both
entry points share. The counter stays: a refusal nobody counts is an anecdote.

## 3. C: the name is the contract

`for_each_flat_object_reference` is now
**`for_each_flat_object_reference_trusting_header`**. Nothing else changed —
same body, same callers, same code generated.

The point is that the fourteen uncapped call sites now *say* what they assume.
A reader of `update_object_refs` no longer has to open the helper to discover
that its bound is a header field nobody validated; the call reads

```rust
for_each_flat_object_reference_trusting_header(obj_ptr, header, 0, |slot, raw, compact| { … });
```

and a caller that acquires geometry has a visibly better neighbour to move to
(`…_capped`, with `holder_walkable_slots`).

That is the whole of option C: no runtime cost, no behaviour change, fourteen
silent assumptions turned into fourteen readable ones.

## 4. Why not A or B

Unchanged from the original page, and still right:

* **A (push geometry to every caller)** — several callers genuinely have no
  geometry to thread. It converts the question rather than answering it.
* **B (an extent oracle in the walker)** — a lookup per object on the marking
  path, and the callers that *do* have geometry already hold a cheaper answer.
  B stays the right answer if a cheap oracle ever exists.

Also unchanged: extending §2.6's region clamp to callers that would have to
*acquire* region state is still refused. It trades a rare read-past-an-object
for a routine lock on a GC hot path.

---

# Part II — two residuals the original survey never looked at

Both are the same shape as §1 — *a flat walk whose count is a second,
unvalidated load of a header* — in code the "fifteen call sites" survey did not
cover, because neither goes through the helper.

## 5. A sixteenth flat walk, hand-rolled, inside g1 itself

`G1Collector::dbg_root_census` (`CRATONVM_G1_DBG_ROOTCENSUS=1`) walked a
non-array holder itself:

```rust
for s in 0..h.num_slots() as usize {                                   // BEFORE
    let v = unsafe { std::ptr::read(data.add(s * SLOT_SIZE) as *const Value) };
    if let Value::Object(Some(o)) = v { /* … */ }
}
```

Three defects the helper does not have, in four lines:

1. **No compact dispatch.** It ignores `GC_FLAG_COMPACT` and strides a compact
   object's *packed* body at the legacy 16-byte pitch — the identical
   COMPACT-LAYOUT PARITY divergence (G1-9) that was fixed in the parallel
   evacuator, reintroduced by copy.
2. **Unchecked transmute.** `std::ptr::read(… as *const Value)` on a
   swept-and-reused cell produces a `Value` with an out-of-range discriminant,
   and the `if let` compiles to `[table + disc*4]` with no bounds check. That is
   the mechanism §2.1/§2.3 of the hibernate page documented and screened
   everywhere else.
3. **A bare `num_slots`.** Not even the `1 << 24` plausibility clamp its
   siblings carry.

Fixed by routing it through
`for_each_flat_object_reference_trusting_header`, which supplies all three. A
census is exactly the kind of caller that should not be growing its own copy of
a walk.

Its `slot1` diagnostic peek is screened the same way and now refuses a compact
receiver, where the second 16-byte cell is not a slot at all.

## 6. The semi-space collector was the reader §2.5's pass missed

`gc/src/gc.rs` (the Cheney semi-space collector) has the *same* TOCTOU
`concurrent_mark::scan_object` was fixed for on 2026-08-24, in three places —
`collect`, `collect_with_finalizers`' Phase-2 scan, and its Phase-3b rescan:

```rust
let total_size = object_total_size(header);
if total_size < HEADER_SIZE || scan_cursor + total_size > to_space.used() { break; }   // validated
…
let num_slots = (header.num_slots() as usize).min(1 << 24);                            // BEFORE
for slot_idx in 0..num_slots {
    let value = unsafe { std::ptr::read(slot_ptr as *const Value) };                   // unchecked
```

`total_size` is computed, *validated against to-space*, and then never used for
the walk. The count that is walked is a second load of the header, bounded only
by a plausibility clamp with a reach of 16 M slots — 256 MB of stride. §2.5's
own words apply verbatim.

Fixed with §2.5's own fix, inverting the arithmetic that was already checked:

```rust
let num_slots = total_size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;                    // AFTER
```

**Exact, not conservative.** For a legacy (non-compact) object
`object_total_size` *is* `HEADER_SIZE + num_slots * SLOT_SIZE`
(`types/src/field_layout.rs::object_body_size`), and the legacy arm is the only
one this branch serves — the compact arm above it walks `ref_offsets`. So on a
sound heap the derived count equals the old one slot for slot; on a torn header
the walk can no longer outrun bytes the scan already approved. The `1 << 24`
bound is unchanged in effect: it lives inside `object_body_size`, which returns
`IMPLAUSIBLE_BODY_SIZE` above it, which the guard above then rejects.

The three `Value` decodes are now `read_value_cell_checked`, matching every
other legacy-cell reader.

---

# Part III — evidence

## 7. What was measured

`FLAT_WALK_REFUSED_ARRAY` is **expected to be ZERO**, and is zero everywhere it
has been looked for. The refusal warns on first sight (`n <= 8`, then powers of
two) at `tracing::warn!`, which the VM's default `EnvFilter` admits to stderr,
so the marker string `flat 16-byte-slot walk REFUSED an ARRAY header` is a
reliable zero/non-zero test on any run log.

MEASUREMENTS_PLACEHOLDER

## 8. What closes this page

The original §5 asked for one of two things. Both are delivered:

* **D implemented and its refusal counted**, so "how often does a flat walk get
  an array?" has a number instead of an anecdote. The number is zero across the
  runs in §7 — the *expected* result, and what makes C sufficient for the
  residual risk: the hazard §3 described is now unreachable through this helper
  regardless of how often it would otherwise have fired.
* **The decision about the fourteen is written down.** They are acceptable
  as-is, *and* they now say so in their own call text. The count they trust is
  still unvalidated; what changed is that (a) the one failure mode with a
  recorded producer cannot pass through them any more, and (b) nobody has to
  read the helper to learn that the rest is trust.

Deliberately **not** claimed: this does not bound a genuinely corrupt
*non-array* `num_slots` at the fourteen sites. Option B would; option B is not
free; and no crash has been attributed to that case. If one ever is, B is the
next step and this page is where its argument already lives.

## 9. Two things deliberately left alone, with the reason

**`gc/src/region.rs`'s `scan_object_refs` / `update_object_refs`** are the same
shape again — kind pre-branch, unbounded `num_slots`, no compact dispatch — and
are **not** fixed. `RegionHeap` is `#[deprecated]`, has no production consumer
(G1CORE-11: "the real region-based collector is `G1Collector` in `g1.rs`"), is
not re-exported from the crate root, and its evacuation is documented as
conservatively unsound by construction. Hardening a prototype kept for its unit
tests would make it look maintained. Noted so the next sweep does not have to
rediscover it.

**ZGC's three forked `enumerate_references` walks** (`gc/src/zgc.rs` ~7903, and
the forks its own doc comments name at ~8711 and ~10575) each transmute a legacy
cell with `std::ptr::read(slot as *const Value)` and take an unbounded
`num_slots` — the same two defects §5 and §6 fix elsewhere, and squarely inside
§0.5 item 2's named reader set. They are **not** fixed here, on purpose: ZGC has
been the default collector since 2026-08-10 and this is its mark path, so
`read_value_cell_checked`'s atomic 16-byte read is a per-slot cost that G1's flat
walk could absorb and this one might not. That needs a measured before/after in
CPU time on an idle host, which is its own piece of work, not a drive-by at the
end of this one. Filed separately.

## 10. Related

* `known-issues/hibernate/hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md`
  §2.5 (the concurrent marker's TOCTOU on the same kind of count — §6 above is
  the reader that pass missed) and §2.6 (the one caller fixed).
* `internal/fixed-bugs/corrupt-value-cell-producer-was-a-string-array-FIXED-20260822.md`
  — the kind-confusion producer, and the telemetry that identified it
  (`receiver_class=java/lang/String receiver_kind=Array`).

## 11. The original page, preserved

Everything below is the 2026-08-24 text as filed. Its §4 recommendation was
right and is what was implemented; its §5 is what this work answers. The one
paragraph that did not survive contact is the census's "several callers do not
visibly pre-branch on kind" — see Part I §1.

---

## 1. The shape

`gc/src/g1.rs::for_each_flat_object_reference` walks a legacy object's 16-byte
`Value` slots:

```rust
for index in first_index..header.num_slots() as usize { … }
```

`num_slots` comes from the object header. The walker takes `&ObjectHeader` and
a raw pointer, and has no way to check that
`HEADER_SIZE + num_slots * SLOT_SIZE` is inside anything. It cannot: it does not
know the region, the arena, or the object's allocated size. **It trusts its
caller, and nothing in its signature says so.**

Fifteen call sites. One — `record_outgoing_rset_edges`, the one with a recorded
crash — now passes a region-derived bound through
`for_each_flat_object_reference_capped`. The other fourteen pass nothing and
walk on `num_slots` alone.

## 2. Why this is not a mechanical edit

The obvious fix ("clamp it everywhere") does not survive contact with the
callers. Most of them do not hold region geometry: they are verifiers, census
walks, evacuation visitors and debug dumps that were handed an object and a
header. To clamp, each needs an answer to *"how many bytes does this object
actually own?"* — and that answer lives in three different places depending on
who is asking:

| caller shape | what it can cheaply prove |
|---|---|
| holds `&[G1Region]` + arena base | region span and allocation cursor — the strongest bound (`holder_walkable_slots`) |
| inside a collector with an old-gen | `old_gen.contains(last_byte)` — the concurrent marker's bound |
| holds only `(ptr, &header)` | **nothing** |

The third row is the design question. There is no heap-wide "how big is this
object" oracle a walker can call without a lock or a region lookup, and adding
one on the marking path is exactly the kind of per-object cost a concurrent
collector cannot absorb casually.

## 3. Why the shape is dangerous specifically here

Not all unvalidated counts are equally likely to be large. This one has a
structural reason to be:

**`NUM_SLOTS_OFFSET == ARRAY_LENGTH_OFFSET == 4`** — the same `shape` dword
(`types/src/heap_types.rs`, `the_shape_word_took_over_the_identity_hash_offset`).
So an **array** misread as a flat object does not yield a garbage `num_slots`;
it yields the array's **length**, which is a plausible-looking positive integer
that can be arbitrarily large, and which the flat walk then multiplies by a
16-byte stride.

That is not hypothetical either. The producer defect behind the corrupt-`Value`
crashes was exactly a wrong-kind read —
`internal/fixed-bugs/corrupt-value-cell-producer-was-a-string-array-FIXED-20260822.md`
caught it with `receiver_class=java/lang/String  receiver_kind=Array`. A
`String[]` was being read through the flat-object path. When that happens, the
"count" the walker trusts is an array length.

So the failure mode is not "a corrupt header produces nonsense". It is "a
**kind confusion** produces a well-formed count with the wrong meaning", and
every guard that validates the count's *plausibility* rather than its *extent*
will pass it.

## 4. Options

**A. Push geometry down to every caller.** Thread a bound into all fifteen.
Honest and explicit, but several callers genuinely have no geometry to thread,
so it converts the question rather than answering it.

**B. Give the walker an extent oracle.** A `fn object_extent(ptr) -> Option<usize>`
the walker calls itself. Correct at every site, but it is a lookup per object on
the marking path, and the callers that *do* have geometry already have a cheaper
answer.

**C. Make the uncapped form impossible to reach by accident.** Rename it
`…_trusting_header` (or require an explicit `Unbounded` argument) so every call
site states which it is. Costs nothing at runtime, changes no behaviour, and
turns fourteen silent assumptions into fourteen readable ones. This is the
cheap half of A.

**D. Validate kind before walking.** The specific hazard in §3 is a *kind*
confusion, so a walker that refuses to do a flat walk on a header whose
`kind() == Array` removes the large-count case at its root, independently of
extent. Cheap (one tag read, already loaded), and narrower than the general
problem — it does not bound a genuinely corrupt non-array count.

**Recommendation: D then C.** D removes the failure mode that has actually
occurred, for one tag comparison. C makes the residual risk visible at every
site without pretending to have bounded it. B is the "right" answer if a cheap
extent oracle ever exists; A is the most work for the least clarity.

Deliberately not recommended: extending §2.6's region clamp to callers that
would have to acquire region state to use it. That trades a rare
read-past-an-object for a routine lock on a GC hot path, which is a worse bug in
a different place.

## 5. What would close this page

* D implemented and its `else`-branch refusal counted, so the "how often does a
  flat walk get an array?" question has a number instead of an anecdote. If the
  count is zero across the suites for a while, §3's hazard is closed empirically
  and C is enough for the rest.
* Or a decision that the fourteen are acceptable as-is, written down with the
  reasoning — which is a legitimate outcome and better than the current state,
  where they are unbounded by default and nobody has said whether that is
  intended.

## 6. Related

* `known-issues/hibernate/hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md`
  §2.5 (the concurrent marker's TOCTOU on the same kind of count) and §2.6 (the
  one caller fixed).
* `internal/fixed-bugs/corrupt-value-cell-producer-was-a-string-array-FIXED-20260822.md`
  — the kind-confusion producer, and the telemetry that identified it.
