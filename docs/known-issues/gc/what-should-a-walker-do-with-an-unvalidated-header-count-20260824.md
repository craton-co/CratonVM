# What should a GC walker do with a header count it cannot validate?

**Status: OPEN DESIGN QUESTION, not a known defect.** Filed 2026-08-24 out of
`known-issues/hibernate/hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md`
§2.6, which fixed one instance and deliberately left the general case alone
rather than change fourteen call sites blind.

Nothing here is currently known to be crashing. This is about a shape that has
produced at least two crashes already and is still present in fourteen places.

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
