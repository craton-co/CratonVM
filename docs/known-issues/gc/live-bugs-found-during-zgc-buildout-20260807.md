# Live bugs found incidentally during the ZGC buildout, 2026-08-07

**Status: MIXED — 4 fixed today (2 of them only partially), 2 open, 1
downgraded to latent.** Every entry below has been re-verified against the
working tree on `dev` as of this write-up; the `file:line` anchors are that
tree, not the agent reports.

## Why these are in one document

A large parallel effort built out the ZGC subsystem on 2026-08-07. The agents
doing it were reading, auditing and mirroring code that has **nothing to do
with ZGC** — the object header (`types/src/heap_types.rs`), the JIT read
helpers (`vm/src/jit/helpers.rs`), the shared array-element decode path
(`gc/src/heap.rs`), and the Generational and G1 field accessors — because a new
collector has to reproduce whatever contract those already implement. Mirroring
a contract is the cheapest way to discover that the contract is wrong.

The defects they turned up are therefore **not ZGC defects**. Several affect
the default (Generational) collector, the JIT, and the mark word on every
build. They were recorded only in ephemeral agent reports; this file is where
they stop being ephemeral.

Companion documents:

- `docs/feature-designs/zgc-production-implementation-plan.md` — the plan the
  buildout followed, and where the ZGC-side follow-ups live.
- `docs/known-issues/springboot/zgc-real-fullsuite-regression-20260807.md` —
  the 49 HANG / 22 FAIL classes from the first ZGC full-suite run. **Entry 7
  below changes how those FAIL logs should be triaged.**

## Table

| # | Finding | Affected component | Reachable on default collector today | Status | Confidence |
|---|---|---|---|---|---|
| 1 | `MARK_QUARTET_MASK` was two bits short, truncating `gc_age >= 4` | Object header (`types/src/heap_types.rs`) — every collector, every build | **Was yes** (Gen + G1 both age survivors after copying the mark word) | **fixed-today** | High |
| 2 | JIT reference degrades were silent and uncounted (6 sites) | JIT read helpers (`vm/src/jit/helpers.rs`) | Yes — JIT is on by default | **fixed-today (partial)** — counted, but still a *second* counter | High |
| 3 | `read_prim_element`'s plausibility degrade was silent | Shared array-element read (`gc/src/heap.rs`) — used by Gen, G1 and ZGC | Yes | **fixed-today (partial)** — counted; the degrade itself remains | High |
| 4 | `enumerate_references`' compact arm is not narrow-oop-aware | ZGC marker (`gc/src/zgc.rs:1969`) | **No — latent.** Narrow oops are refused for every non-Generational backend, and Generational's own walkers are clean | **open (latent)** | High |
| 5 | `get_field` / `set_field` stride 16-byte cells through a compact-sized body | `gc/src/zgc.rs` **and `gc/src/gen_heap.rs` and `gc/src/g1.rs`** | **Yes — the identical fall-through is in the default collector** | **open** | High |
| 6 | `is_addr_live`'s ZGC arm accepted interior addresses | `gc/src/vm_heap.rs:2235` | No — ZGC arm only | **fixed-today** | High |
| 7 | ZGC native-alloc-pressure arms inert while its allocator `abort()`s | `gc/src/vm_heap.rs` + `gc/src/zgc.rs` | No — ZGC only | **fixed-today (partial)** — 2 of 3 `VmHeap` arms are still inert against a latch that now exists | High |

## 1. `MARK_QUARTET_MASK` was two bits short — FIXED TODAY

**Anchor:** `types/src/heap_types.rs:402` (the constant), `:432-433`
(`AGE_SHIFT` / `AGE_BITS`), `:359-396` (the fix's own post-mortem note).

**Mechanism.** The mask was `0x3FFF << 48` — bits 48..61. `gc_age` is
`AGE_BITS << AGE_SHIFT` = bits 60..63. Bits 62-63, the top half of the age,
fell **outside** the "quartet". Two consequences, both confirmed by reading the
rebuild helpers:

1. `ObjectHeader::quartet_of` (`:1243`) is `mark & MARK_QUARTET_MASK`, and
   `make_thin_locked` (`:1270`), `make_inflated` (`:1305`), `make_forwarded`
   (`:1361`) and `make_hashed` (`:1436`) all rebuild a word from it. Any
   `gc_age >= 4` was truncated to `age & 3`. `MAX_GC_AGE` is 15 (`:501`), so
   **12 of the 16 ages were lossy**: thin-locking, inflating, forwarding or
   hashing an object silently rewound its tenuring age and perturbed the next
   promotion decision.
2. `!MARK_QUARTET_MASK` did not mean "not the quartet" — it retained bits
   62-63. `inflated_monitor` (`:1321`) and `forwarding_target` (`:1373`) mask
   with exactly that, so `set_gc_age(n >= 4)` (`:950-960`) applied to an
   already-`INFLATED` word yielded `monitor_ptr | (1 << 62)`: a non-null wild
   pointer handed to `monitor_ptr_from_mark`
   (`vm/src/threading/monitor.rs`), which only null-checks it. The finder rated
   this consequence *medium*; on re-reading the two mask sites it is **high** —
   the arithmetic is unconditional. What kept it rare is ordering: the `make_*`
   helpers emit words with those bits already clear, so the corruption needs the
   age written *after* the state.
   A third consequence the fix note found and this review confirms:
   `try_thin_lock` screens an unlocked word with
   `cur & !types::MARK_QUARTET_MASK != MARK_NEUTRAL`, so an object that reached
   age 4 could **never satisfy that test again** and every subsequent lock on it
   inflated a monitor permanently — the exact failure the mask was introduced to
   prevent for arrays, reintroduced for aged objects.

**Was it reachable on the default collector?** Yes, and not marginally.
G1's default `promotion_age` is 15, so ages 4..14 are its *ordinary*
young-survivor range; `SharedEvac::evacuate` copies the source mark word to the
destination and then ages the copy, and `gen_heap.rs`'s young non-moving sweep
ages survivors in place. Either produces "state written, then age written".

**Why nothing caught it:** every round-trip test used age 3 — the largest value
that fits in bits 60..61.

**The fix, as landed:** the constant is now `0xFFFFu64 << MARK_QUARTET_SHIFT`
(`:402`) — bits 48..63, the whole 16-bit span rather than the union of the four
fields, so the two unowned bits 54-55 are carried through every rebuild and a
future fifth field can claim them without auditing a caller. Four **compile-time**
`const _: () = assert!` guards (`:440-455`) now require each of `kind`,
`element_type`, `gc_flags` and `gc_age` to lie wholly inside the mask, plus
three more (`:459-470`) requiring the mask not to overlap the state tag, the
identity hash or the thin-lock payload. Runtime tests at `:3005-3143` assert the
partition, and `:3146-3250` walks every age in `0..=MAX_GC_AGE` through each
rebuild helper.

**The three disagreeing numbers are now reconciled**, and this is worth recording
because it is what let the bug hide: the constant's comment said "13 bits", its
value spanned 14, and the fields spanned 16. The doc at `:704-713` records the
same three-way disagreement (48..61, 48..62, "13 bits"). All three now read
"bits 48..63, 16 wide". A related off-by-one in the JIT-placement comment
(`gc_age` described as "bits 59..63", a five-bit range disagreeing with
`AGE_SHIFT = 60`) was corrected at `:413-416`.

## 2. The JIT's reference degrades were silent and uncounted — FIXED TODAY (partial)

**Anchor:** `vm/src/jit/helpers.rs:4867-4919` (the analysis),
`:4928` (`JIT_REF_DEGRADATIONS`), `:4950` (`jit_decode_ref_word`), `:5024`
(the increment). The six call sites are now `:5070` (`jit_aaload`/element),
`:5380`, `:5416`, `:5448` (`jit_getfield`'s three arms), `:6307`, `:6333`
(`jit_getstatic`'s two).

**Mechanism.** All six arms carried the same three lines verbatim:

```rust
if plausible_heap_pointer(raw) { raw as i64 } else { 0 }
```

That `else { 0 }` hands Java `null` for a slot that held bits. Their
interpreter twin does the same thing but feeds
`cratonvm_types::compact_value::note_object_degradation`
(`types/src/compact_value.rs:330`) — a process-wide counter plus a one-shot
stderr line, read back through the `pub object_degradation_count` (`:302`).
The six JIT arms fed **nothing**.

**Concrete failure.** `object_degradation_count()` reads `0` while compiled
code drops live references. Worse than incomplete: it reads *clean* for
precisely the configuration where the underlying root-coverage gap is most
likely to fire, because JIT'd frames are the ones the deposited root snapshot
misses. A root-coverage regression that only manifests under JIT is invisible
to the instrument built to catch it.

**Provenance.** `git blame` puts all six in
`6a04b0e3c173e5cb4f47287b013fe302a71c0d77` (2026-06-29, "degrade stale
references to null at every decode boundary"). That commit's own closing
paragraph is the key context and is quoted here because it settles what these
filters *are*:

> The underlying GC root-coverage gap (live blocked-thread frame objects swept
> by the non-moving young sweep) remains and is the only complete fix for the
> JIT intrinsic paths that have no single decode chokepoint.

They are a heuristic compensating for a known-live bug, not an integrity guard
on trusted data.

**What landed today.** A single `#[inline(always)]` chokepoint
`jit_decode_ref_word` (`:4950`) whose fast path is byte-for-byte the old
predicate, with everything new in a `#[cold] #[inline(never)]` callee
(`:4983`) that is only reached once the predicate has already failed — so this
costs nothing per field access on any build or collector. It counts
(`:5024`), and it separates a ZGC colored word (a `#[cfg(feature = "zgc")]`
hard failure naming the missing barrier) from genuine garbage (the old
degrade, now counted).

**Still open.** There are now **two** counters where there should be one.
`note_object_degradation` is `pub(crate)` (`types/src/compact_value.rs:330`),
so `helpers.rs` cannot call it, and the file's own TODO at `:4900-4903` says
so. Unifying them is a one-word change — `pub(crate)` → `pub` — plus swapping
the increment at `helpers.rs:5024`. Until then a triager must read
`jit_ref_degradation_count()` (`:4935`) *and*
`object_degradation_count()`; zero on one is not zero overall.

**The root cause is untouched.** The root-coverage gap the commit message names
is still the only complete fix. Both counters are instruments on a live wound.

## 3. `read_prim_element`'s plausibility degrade — FIXED TODAY (partial)

**Anchor:** `gc/src/heap.rs:1658-1714` (the analysis), `:1732`
(`REF_ELEMENT_DEGRADATIONS`), `:1757` (`decode_ref_element_word`), `:1787`
(the cold arm), `:1818` (the increment).

**An agent was editing this file during the review; the state recorded here is
the current working tree.**

**Mechanism.** Same commit, same idiom, on the `Reference` arm of
`read_prim_element` — the array-element read for **every** collector.
`Heap::get_array_element` / `get_array_element_unboxing`,
`GenerationalHeap`'s twins (`gen_heap.rs:3518`, `:4072`, `:4092`) and
`zgc.rs:2608` all funnel through it. On the default collector a live
`Object[]` element could be nulled with no trace anywhere: no counter, no log,
no assert, and `object_degradation_count()` reading a reassuring 0. Three
independent reviewers called it the most dangerous unmigrated read in the tree,
and `docs/feature-designs/zgc-reference-slot-representation.md` names it as
such.

**What landed today.** The same chokepoint shape as entry 2:
`decode_ref_element_word` (`:1757`) with the old predicate as its fast path,
and a cold callee (`:1787`) that splits three cases —

- `raw == 0`: an ordinary null element. **Not counted.**
  `plausible_heap_pointer(0)` is `false`, so a plain null reaches the cold arm
  too; counting it would put the counter in the millions on a clean run and
  make "non-zero means a live object was nulled" false on first use. The
  `raw == 0` test is load-bearing, not a micro-optimisation.
- a structurally well-formed ZGC colored word: hard failure naming the missing
  barrier (`#[cfg(feature = "zgc")]`, so a default build does not compile the
  branch at all).
- stale/garbage bits: the old degrade, now counted at `:1818`.

**Still open, and the reason this is only a partial fix:** the degrade itself
survives. A stale reference still becomes a Java `null` on the default
collector; it is now merely *visible*. Reading
`ref_element_degradation_count()` (`:1738`) as non-zero means the root-coverage
gap from entry 2 is live in that run. There is also a TODO at `:1729-1731`:
`VmHeap::print_gc_summary` already reports `COMPACT_OOP_MAP_MISSING` and this
counter belongs on the same line.

## 4. `enumerate_references`' compact arm is not narrow-oop-aware — OPEN, LATENT (downgraded)

**Anchor:** `gc/src/zgc.rs:1969`.

```rust
let slot = unsafe { base.add(HEADER_SIZE + offset as usize) };
let raw = unsafe { std::ptr::read(slot as *const u64) };
```

**Mechanism.** A plain 8-byte read. The real read path,
`read_compact_field` (`types/src/field_layout.rs:978-993`), branches on
`narrow_oops_enabled()` and loads an `AtomicU32` + `decode` when narrow oops
are on, because `ref_field_size()` / `ref_element_size()`
(`types/src/narrow_oop.rs:67`, `:77`) are **4** in that mode. Under narrow oops
the ZGC marker would read a 4-byte encoded reference plus 4 bytes of the
neighbouring field and trace the resulting garbage as an object address.

**Assessment: latent, not live.** `vm/src/vm/vm_init.rs:1396-1401` gates
compressed oops behind `gc_backend != GcBackend::Generational`, printing
`"compressed oops requested but the selected GC backend is not generational -
running with 64-bit references"` and continuing wide. ZGC can never observe
narrow oops today. It becomes live the moment anyone removes that gate — which
the ZGC production plan contemplates.

**The cross-collector check — this is the part that matters, and it partly
refutes the concern as framed.** The finding was raised on the hypothesis that
the same pattern exists in an *ungated* collector. It does and it does not:

- **`gc/src/g1.rs:95`** — `for_each_flat_object_reference`, G1's central
  compact-object reference walker, contains the byte-identical
  `std::ptr::read(slot as *const u64)`. So do `g1.rs:624`, `:747`, `:2214`,
  `:4139`, `:4291`, `:4474`, `:4583`, `:5679`, `:8798` and the
  `data_start.add(k * 8)` array walks. **But G1 is gated by the same
  `vm_init.rs:1397` check as ZGC.** It is exactly as latent, not worse.
- **`gc/src/gen_heap.rs` — the only collector that can actually see narrow
  oops — is clean.** Every reference read on its scan paths goes through
  `read_ref_slot` (`types/src/narrow_oop.rs:203`, which branches on the flag)
  with an `ref_element_size()` / `ref_field_size()` stride:
  `gen_heap.rs:13319`, `:13343`, `:15515`, `:15589`, plus `:6172`, `:7908`,
  `:13932`. `old_gen.rs:1375-1406` and `concurrent_mark.rs:870-906` are the
  same. The plain `*(x as *const u64)` reads elsewhere in `gen_heap.rs` are
  mark-word (word0) reads and 16-byte `Value` cell reads, neither of which is
  a narrow slot.

**Conclusion: no ungated collector has the defect.** The narrow-oop
implementation is coherent — the one backend permitted to use it is the one
that was audited for it, exactly as `vm_init.rs:1388-1390` claims. What is
worth recording is that **the gate is now the only thing holding three
collectors' worth of wide reads together**, so it must not be relaxed
per-backend without an audit of the ~12 `g1.rs` sites and `zgc.rs:1969` first.

**A fix, if wanted now:** replace the read at `zgc.rs:1969` with
`cratonvm_types::narrow_oop::read_ref_slot(slot)`. The same file already does
this correctly in its census arm (`zgc.rs:2317`, `:2409`), which is a useful
in-file precedent — and a sign the two arms were written by different hands.

## 5. `get_field` can read past an allocation — OPEN, and live on the default collector

**Anchor:** `gc/src/zgc.rs:2530-2547` (`get_field`), `:2550-2573`
(`set_field`), `:2432-2466` (`alloc_object`). **And, discovered while
verifying this entry:** `gc/src/gen_heap.rs:3502` + `:3540`, and
`gc/src/g1.rs:7906-7909` + `:7955`.

**Mechanism.** `alloc_object` (`zgc.rs:2433-2443`) sizes the body with
`compact_object_body_size(...)`, falling back to `num_fields * SLOT_SIZE` only
when that returns `None`; it sets `GC_FLAG_COMPACT` (via `set_compact_shape`,
`:2458-2460`) only in the `Some` case. So a flagged object's body is
**compact-sized** — `layout.body_size`, typically far smaller than
`num_slots * 16`.

`get_field` then asks `compact_object_field_storage(header, index)`
(`types/src/field_layout.rs:953-965`), which returns `None` if *either*
`is_compact_object(header)` is false *or*
`class_layout_for_fields(class_id, num_slots)` misses. On `None` it falls
through to:

```rust
let ptr = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
std::ptr::read(ptr as *const Value)
```

For a flagged-but-unresolvable object those two `None` reasons are conflated,
and striding 16-byte cells through a compact-sized body reads out of bounds.
`set_field` (`:2569-2573`) *writes* out of bounds by the same path — into the
neighbouring object.

**How does an object end up flagged-but-unlaid-out?** This is not
hypothetical, and the tree already documents the state under the name
`HIB-DCAST-LATEPHASE.1`. The layout registry is append-only, but
`class_layout_for_fields` is keyed on `(class_id, field_count)` and validated
against `LAYOUT_GENERATION`; **a class redefinition racing the registry**
leaves a live, correctly-flagged compact instance whose `(class_id,
num_slots)` no longer resolves. `gen_heap.rs:15488-15507` describes exactly
this and records that it reached a `SIGSEGV` against the real
`DefaultCatalogAndSchemaTest` workload (Hibernate/ByteBuddy proxies churn
redefinitions). Note also that `compact_object_body_size`
(`field_layout.rs:941-944`) refuses a layout owned by a different
`layout_domain` while the *read* path does not check the domain at all — a
second, independent route into the same divergence.

**ZGC's own census already names this as a latent bug in `get_field`.**
`zgc.rs:2281-2290` explains why the census must use the raw header bit and
*not* imitate the read path:

> Striding 16-byte cells through a compact-sized body because the layout lookup
> failed would read past the allocation — `get_field`'s fall-through does
> exactly that, and it is a latent bug there that this instrument must not
> reproduce.

**The finding was scoped to ZGC. It is not a ZGC bug.** The identical
fall-through is in both other collectors:

- **`gen_heap.rs` (the default).** `compact_field_slot(header, index)`
  (`:15421-15441`) has the same two-reasons-one-`None` shape, and on `None`
  `get_field` falls through to `slot_ptr(obj_ref, index)` at `:3540` —
  `HEADER_SIZE + index * SLOT_SIZE` through a compact-sized body. The
  preceding guards (`:3303` suspect header, `:3322` `index >= num_slots`)
  do not help: `num_slots` on a compact object is the *field count*, so an
  in-range index still lands past the body.
- **`g1.rs`.** `:7907-7909` is literally
  `.unwrap_or((index * SLOT_SIZE, SLOT_SIZE))`, consumed at `:7955`. The
  humongous arm (`:7917-7943`) is bounds-checked by `humongous_copy`; the
  ordinary single-region arm is not.

**So the answer to "is it reachable on the default collector today" is yes.**
The sibling GC walkers in `gen_heap.rs` were hardened against precisely this
state (`:15488-15520`, `:15576-15600` — they now test
`is_compact_object(header)` directly and *skip* the object rather than fall
back to legacy striding), and `object_body_size` was given
`IMPLAUSIBLE_BODY_SIZE` as its corrupt-signal (`field_layout.rs:1124-1187`)
for the same reason. **The mutator-side field accessors were never given the
same treatment.**

**What a fix involves.** Adopt the walkers' discrimination in all six
accessors (`get_field` / `set_field` on each of the three collectors): test
`is_compact_object(header)` — the per-object header bit, independent of the
registry — *before* the fall-through, and when it is set but the layout will
not resolve, return `Value::Object(None)` / drop the write and log, exactly as
`for_each_ref_slot` now skips. The legacy fall-through must be reachable only
for objects whose compact bit is clear. The cheapest correct shape is a shared
helper in `gc/src/heap.rs` next to `compact_oop_scan`, since all three
collectors currently carry their own copy of the same three lines.

## 6. `is_addr_live` used an interior-accepting predicate — FIXED TODAY

**Anchor:** `gc/src/vm_heap.rs:2235` (the function), `:2264-2301` (the arms
and the fix note), `:2301` (the ZGC arm).

**Mechanism.** The ZGC arm called `is_heap_addr`, whose interior fallback
(`zgc.rs:1731-1743`) walks object extents, where an exact registry-base test
was required. The consumer's own doc already *claimed* it was a registry
lookup — the code and its documentation had diverged.

**Concrete failure.** The ZGC sweep zeroes each dead object, returns its span
to the arena free list, then **coalesces** adjacent free spans. A later
allocation carved from the head of a coalesced block covers the interior of
what used to be several dead objects, so a *dead* object's pre-GC base becomes
an interior address of an innocent *live* object — and the extent walk answered
`true`. Reference processing read that as "the referent survived"; because
ZGC's `pointer_map` is always empty (`zgc.rs:2489`, non-moving), the consumer
at `vm/src/runtime/interpreter/gc_and_alloc.rs:2287` falls back to the stale
address and performs `set_field(obj, 0, Value::Object(None))` on it — **a null
written into the middle of a live object**, and at the weak/phantom restore
site a non-null reference written there. That is the HIB-CV-32
stale-referent-write corruption shape reproduced on this backend.

**Fix, as landed:** `VmHeap::Zgc(h) => h.is_object_address(addr).is_some()`
(`vm_heap.rs:2301`) — the exact registry-base test at `zgc.rs:1715`, one
locked hash probe. It is also strictly faster: `is_heap_addr`'s fallback is
O(live) *under the registry mutex*, and this predicate runs once per tracked
reference per collection.

**Worth preserving:** the Generational arm
(`h.is_live_old_gen_addr(addr) || h.is_live_young_survivor(addr)`) and the G1
arm (`h.is_addr_in_live_region(addr)`, deliberately region-granular) are
**not** wrong. G1 emits identity `pointer_map` entries for every self-forwarded
live object, so the map hit fires first and the loose predicate is only a
fallback. ZGC has no such map, so its predicate is load-bearing alone and must
be exact. Do not "harmonise" these three arms.

## 7. ZGC's native-alloc-pressure arms were inert while its allocator aborts — FIXED TODAY (partial)

**Anchor:** `gc/src/vm_heap.rs:1167-1174` / `:1178-1194` / `:1199-1236` (the
three arms), `gc/src/zgc.rs:1521` (the latch field), `:1676` / `:1685` /
`:1700` (the accessors), `:1765` (the arming edge in `alloc_raw`), `:2973`
(the disarm), `:2447-2449` and `:2482-2484` (the aborts).

**Mechanism.** `ZgcRealHeap`'s allocator is **infallible**:

```rust
let ptr = self.alloc_raw(total).unwrap_or_else(|| {
    eprintln!("FATAL: ZGC(real): out of heap space for object ({total} bytes)");
    std::process::abort();
});
```

(`zgc.rs:2447-2450`; the array twin at `:2482-2485`.) A workload that allocates
only from inside native wrappers reaches no safepoint of its own, so the one
hook that can collect on a native's behalf (`vm/src/vm/vm_exec.rs:2707`,
driven by `VmHeap::young_spill_pressure`) never fired on this backend — all
three `VmHeap` arms were hardwired `false` / no-op. The process died by
`abort()` on a heap full of garbage, with **no Java-visible `OutOfMemoryError`
ever thrown**. G1 closed the identical defect with
`G1Collector::native_alloc_pressure` (`g1.rs:1396`, armed by
`note_region_consumed_locked` at `:1765-1774`).

**ACTIONABLE, and the reason this entry exists at all.** The abort is a
`SIGABRT` with a single stderr line and no Java stack. Anyone triaging the
**22 ZGC FAIL classes** in
`docs/known-issues/springboot/zgc-real-fullsuite-regression-20260807.md`
should `grep 'FATAL: ZGC(real)'` over those logs **before** assuming a test
bug or a VM defect elsewhere. A pre-fix run that hit this looks like an
arbitrary crash, not like an OOM.

**What landed today, and the desync between two agents' fixes.** Two
independent changes landed and they do not fully meet:

- `gc/src/zgc.rs` **grew the real latch**: a `native_alloc_pressure: AtomicBool`
  field (`:1521`, init `false` at `:1604`), `native_alloc_pressure()` (`:1676`),
  `clear_native_alloc_pressure()` (`:1685`), `note_native_alloc_pressure()`
  (`:1700`), armed from `alloc_raw` on the threshold crossing (`:1764-1766`)
  and disarmed at the end of `collect_garbage` (`:2973`), with four unit tests
  at `:3859-3981` including one that proves it cannot reopen the `gc_rearm` GC
  storm.
- `gc/src/vm_heap.rs` **did not adopt it.** `young_spill_pressure` returns
  `h.needs_gc()` (`:1172`) — a *computed* stand-in, not the latch;
  `clear_young_spill_pressure` is a no-op (`:1192`); and
  `note_young_spill_pressure` is a no-op (`:1234`). Its doc at `:1150-1152`
  and its TODO at `:1217-1232` both still assert that "`ZgcRealHeap` has no
  pressure field and this file cannot add one" — **which is no longer true.**

**Net effect.** The abort-on-a-heap-full-of-garbage case *is* now covered,
because `needs_gc()` answers the same question as G1's arming edge. What is
still dropped is an *externally* noted event: a caller that spilled below the
trigger and wants the next boundary to collect anyway. `vm_heap.rs:1212-1215`
argues no such caller exists outside `gc/`, which the grep confirms — so this
is a gap in the mechanism rather than a live defect, but it is a gap held open
by a stale comment.

**Remaining work is mechanical and is already specced in-tree.** Follow
`vm_heap.rs:1217-1232` steps (1)-(4), all of which `zgc.rs` has now done, and
make the three arms plain delegations like G1's, with
`young_spill_pressure` becoming `h.native_alloc_pressure() || h.needs_gc()` so
the latch **adds to** the occupancy test rather than replacing it. Then delete
the three stale claims in `vm_heap.rs`. Separately, the aborts at
`zgc.rs:2449` / `:2485` should become a Java `OutOfMemoryError` — pressure
signalling reduces how often they are reached but does not remove them.
