# `VmHeap::Zgc` — per-arm moving-collector safety audit

*Written 2026-08-07 against `dev@a355cb63d`, by a read of `gc/src/vm_heap.rs`
(3118 lines, in full), `gc/src/collector.rs`, `gc/src/zgc.rs`, the G1 and
generational counterparts in `gc/src/g1.rs` / `gc/src/gen_heap.rs`, and every
in-tree caller in `vm/` and `jit/`.*

This is workstream **4b** of
[`docs/feature-designs/zgc-production-implementation-plan.md`](../feature-designs/zgc-production-implementation-plan.md)
("Audit and populate every neutral `VmHeap::Zgc` arm"), which that plan requires
to complete **before** WS 3b (concurrent relocation) lands. Companion audits:
[`g1-audit.md`](g1-audit.md), [`gc-crate-audit.md`](gc-crate-audit.md),
[`tlab-and-card-audit.md`](tlab-and-card-audit.md).

Every claim is anchored to `file:line` as of this commit. Where a claim rests on
a convention rather than a check, that is stated. **No code was changed by this
audit and no compiler was run** — every finding below is derived from source.

---

## 0. Headline

**Read this first: the `zgc` feature does not compile on `dev` today.** Three
independent breakages landed in `gc/src/zgc.rs` on 2026-08-07, hours before this
audit, and none is visible to a default build because `#[cfg(feature = "zgc")]
pub mod zgc;` (`gc/src/lib.rs:111`) means the file is never parsed. Details in
§1. Everything else in this document describes a configuration that currently
cannot be built.

| Classification | Count | Meaning |
|---|---:|---|
| **SAFE** | 33 | Correct whether or not objects move. |
| **NON-MOVING-ONLY** | 16 | Correct today; silently wrong under compaction or a generational split. |
| **ALREADY-WRONG** | 5 | Wrong for today's non-moving collector. |
| **UNKNOWN** | 2 | Not determinable from source; see the rows. |
| **Total literal `VmHeap::Zgc` arms** | **56** | |

Plus the `dispatch!` macro arm (`vm_heap.rs:228`), which expands at **24** call
sites, and **9** wildcard `_ =>` arms that also serve ZGC. Full arithmetic in §2.

### The five ALREADY-WRONG findings

| # | Where | What |
|---|---|---|
| **AW-1** | `gc/src/zgc.rs:2140`, `:1988`, and ~20 field accesses | The `zgc` feature does not compile. §1. |
| **AW-2** | `vm_heap.rs:2154` (`is_addr_live`) | Uses the loose, **interior-accepting** `is_heap_addr` where the exact registry-base test is required — and where the consuming method's own doc (`:2203-2205`) claims it is "exact (registry lookup)". Reproduces the HIB-CV-32 stale-referent-write corruption shape on this backend. §3.1. |
| **AW-3** | `vm_heap.rs:1145`, `:1156`, `:1168` (native-alloc-pressure trio) | Hardwired inert (`false` / `{}`) while `ZgcRealHeap::alloc_object` **aborts the process** on exhaustion (`zgc.rs:1948-1951`). This is the exact G1 defect the trio was introduced to fix. §3.2. |
| **AW-4** | `vm_heap.rs:1977` (`enable_gc_logging`) | Logs "Verbose GC logging enabled (ZGC-real collector)" and enables nothing. §3.3. |
| **AW-5** | `vm_heap.rs:527` + `:615` | `conservative_addr_span` → `None` forces **every stack qword** through `ZgcRealHeap::is_object_address` (a `Mutex` acquire each), and `is_heap_addr`'s interior fallback is an **O(live)** walk under that same mutex, called per operand-stack slot. A concrete competing hypothesis for the 35 `PASS → HANG` classes. §3.4. |

### What contradicted the framing I was given

1. **The count.** 58 grep hits, but two are not dispatch arms (`:228` is the
   macro body, `:278` is a constructor) — so **56** literal arms. The macro
   expands at **24** call sites, not the "~40" the plan states
   (`zgc-production-implementation-plan.md:182`). There are also **9** wildcard
   `_ =>` arms nobody has counted. Total ZGC-reachable dispatch points: **89**,
   plus 3 methods with no match at all (§2). The caller's "~90" was closer than
   the plan's numbers.
2. **`vm_heap.rs:1809` is not a landmine — it is dead code.**
   `g1_concurrent_mark_step` answering `true` for ZGC is flagged in the plan
   (`:197`) and in the task framing as an affirmative arm needing re-derivation.
   It is a G1-only method (`Generational` answers `true` too, `:1807`) and it
   has **zero callers anywhere outside `vm_heap.rs`**. Classified SAFE.
   `is_in_young_addr` (`:2270`) is likewise callerless.
3. **`:2304` / `:2344` do not break at relocation — they break at the
   *generational split* (Phase 2), one phase earlier.** Their soundness argument
   (`:2294-2298`) is "the ZGC mark walks every live region uniformly", which a
   young-only cycle ends. Relocation adds a *second*, separate problem: they
   defer to three **address-keyed** side tables (`loader_pin`, `mirror_pin`,
   `metadata_pin`, consumed at `zgc.rs:2225-2233`) and nothing in `zgc.rs` re-keys
   any of them after a collection. Sequence them into Phase 2, not Phase 3b.
4. **The genuinely worst affirmative arm is not on the plan's list at all.**
   `supports_jit_tlab_skip` (`:551-553`) returns a hardcoded `true` with a
   written six-line soundness argument (`:542-547`) that names *both* premises
   this effort is about to delete: "non-moving STW mark-sweep" and
   "`refill_tlab` never hands ZGC mutators a TLAB". It has no match arm, so it
   does not appear in any `VmHeap::Zgc` grep. §3.5.
5. **The pointer-map landmine is real and is worse than described.** It is not
   only that `remap_after_gc` sees "nothing moved" — it is that
   `vm/src/runtime/interpreter/gc_and_alloc.rs:2287` does
   `pointer_map.get(&ref_addr).copied().unwrap_or(ref_addr)` and then
   `set_field(obj_ref, 0, Value::Object(None))` at `:2304`. An empty map from a
   moving collector routes that write through the **stale pre-GC address**. §3.6.

---

## 1. AW-1 — the `zgc` feature does not compile (blocks everything)

`gc/src/zgc.rs` is not modified in the working tree; these are committed on
`dev`. `git blame` attributes them to two commits from **2026-08-07**, both of
which are *after* `275887cb3`, the commit the 2026-08-07 full-suite baseline was
measured on
([`zgc-real-fullsuite-regression-RETIRED-20260807.md:10`](../fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260807.md)).

| # | Site | Defect | Introduced by |
|---|---|---|---|
| 1 | `zgc.rs:2140` | Unbalanced parentheses — `write_prim_element(base, index, element_type, Value::Object(Some(wrapper));` is missing the closing `)` of the call. A hard parse error. | `d7965af6a` "feat: HEADER_SIZE 24 -> 16" |
| 2 | `zgc.rs:1988-1995` | `ObjectHeader::new` takes **5** parameters (`types/src/heap_types.rs:642-648`); this call passes **6**, including a `self.next_hash()` argument for the `identity_hash_code` field that no longer exists. The sibling calls at `zgc.rs:1589` and `:1952` were updated; this one was not. | `1d2817c75` "delete identity_hash_code from ObjectHeader" |
| 3 | ~20 sites | `ObjectHeader` now has exactly three fields — `class_id`, `shape`, `mark_word` (`heap_types.rs:585-614`). `kind`, `element_type` and `gc_flags` moved into mark-word bits 48..61 behind accessors. `zgc.rs` still reads them as struct fields at `:1732`, `:1738`, `:1763`, `:1806`, `:1856`, `:2095`, `:2101`, `:2110`, `:2117`, `:2123`, `:2177`, `:2219`, `:2223`, `:2255`, `:2265`, `:2269`, `:2340`, `:2343`, `:2359`, `:2361`. | `d7965af6a` |

Note (3) is not cosmetic: `zgc.rs:2177`/`:2223`/`:2361` are the mark and sweep's
read-modify-write of the mark bit. Whatever replaces them must be **atomic**
now that the flags share a word with the lock state and the identity hash — a
non-atomic `|=` on `mark_word` would clobber a concurrently-installed hash or a
thin lock. That is a design decision, not a mechanical port, and it belongs to
whoever fixes this.

**This is R4 ("feature-gate rot") from the plan's risk register happening for
the second time in three days**, and this time it happened *after* `ci.yml`
grew the gate meant to prevent it. The working tree adds
`cargo build -p cratonvm-cli --features zgc` and
`cargo test -p cratonvm-gc --lib --features zgc` to `.github/workflows/ci.yml`;
both will fail on the first run. That is the gate working — but it means the
first CI run after that change lands will be red for a reason unrelated to
whoever lands it.

**Consequence for this audit.** Every arm below is analysed as written. Whether
it *behaves* as written cannot be established until the crate compiles.

---

## 2. Arm arithmetic

| Kind | Count | Where |
|---|---:|---|
| Literal `VmHeap::Zgc(..) =>` dispatch arms | **56** | listed in §4 |
| `grep` hits that are **not** arms | 2 | `:228` (macro body), `:278` (`GcBackend::Zgc => VmHeap::Zgc(..)` constructor) |
| `dispatch!` macro call sites (each expands one Zgc arm) | **24** | `:294 :298 :307 :460 :469 :496 :799 :806 :814 :827 :835 :845 :863 :868 :877 :887 :892 :903 :912 :947 :962 :971 :2104 :2389` |
| Wildcard `_ =>` arms that also serve ZGC | **9** | `:589 :598 :644 :1176 :1187 :1198 :1222 :1241 :2228` |
| **ZGC-reachable dispatch points** | **89** | |
| Methods with **no match at all** that still apply to ZGC | 3 | `:551` `supports_jit_tlab_skip`, `:694` `load_and_forward`, `:1094` `array_data_ptr` |

All 24 macro expansions are pure delegations to `ZgcRealHeap` inherent/trait
methods; the macro itself is SAFE. Two expansions carry moving-collector debt in
their *callee*, not in the arm: `walk_objects` (`:2389` →
`zgc.rs:1704`, iterates the registry under one lock — a snapshot taken without
STW is torn once relocation is concurrent) and `get_header` (`:460`, `:2104` →
`zgc.rs:2003`, returns a `&ObjectHeader` borrowed straight out of the heap, which
a relocating collector can invalidate underneath the borrow).

---

## 3. The findings that matter

### 3.1 AW-2 — `is_addr_live` accepts interior pointers where it must accept only bases

```
vm_heap.rs:2154   VmHeap::Zgc(h) => h.is_heap_addr(addr).is_some(),
```

`ZgcRealHeap` has two lookups. `is_object_address` (`zgc.rs:1667`) is the exact
registry-base test. `is_heap_addr` (`zgc.rs:1677`) tries the same hash lookup and
then, on a miss, **walks every live object's extent** (`zgc.rs:1684-1694`) and
returns the containing base for any interior address.

The arm uses the second. Three things follow.

**(a) The consuming method's documentation is false.**
`watched_pre_gc_addr_survived` justifies delegating to `is_addr_live` with
"G1/ZGC: `is_addr_live` is already exact (live-region membership / **registry
lookup**)" (`vm_heap.rs:2203-2205`). It is not a registry lookup; it is a
registry lookup *or* an extent walk.

**(b) It produces a stale write.** The sweep zeroes each dead object, returns
its span to the arena free list, and then **coalesces adjacent free spans into
maximal blocks** (`zgc.rs:2378-2409`). A later allocation carved from the head of
a coalesced block covers the interior of what used to be several dead objects.
A pre-GC address of one of those dead objects then answers `true` here. The
consumer chain is exact and short:

* `gc_and_alloc.rs:2195-2204` builds `is_marked` from
  `watched_pre_gc_addr_survived`, which for ZGC is this arm.
* `gc_and_alloc.rs:2253-2266` builds `is_stale_young` from
  `pre_gc_addr_did_not_survive` (`vm_heap.rs:2382`, `!self.is_addr_live(addr)`)
  ORed with the same predicate — so a `true` here makes both say "survived".
* `gc_and_alloc.rs:2287`: `let actual_addr = pointer_map.get(&ref_addr).copied().unwrap_or(ref_addr);`
  — the map is always empty on this backend, so `actual_addr` is the stale
  interior address.
* `gc_and_alloc.rs:2294` applies the `num_fields(obj_ref) < 2` shape guard,
  which reads a header at that interior address — arbitrary object payload,
  which can pass.
* `gc_and_alloc.rs:2304`: `set_field(obj_ref, 0, Value::Object(None))` writes a
  null into the middle of an innocent live object.

That is the `bc math-ec 0x4` / HIB-CV-32 corruption shape the guard chain at
`:2211-2252` exists to prevent, reproduced on ZGC through a predicate the
comment believed was exact. The same path at `:2524`
(`set_field(ro, 0, Value::Object(Some(rt)))`, the weak/phantom restore) writes a
*non-null* reference through the same address.

**(c) It is O(live) per call under a mutex.** See AW-5.

**Fix:** `VmHeap::Zgc(h) => h.is_object_address(addr).is_some()`. This is a
one-line change and it should land independently of, and before, any of the ZGC
production phases. The G1 arm (`:2152`, `is_addr_in_live_region`) is
region-granular and therefore *deliberately* loose, but G1's `pointer_map`
carries identity entries for every self-forwarded live object
(`g1.rs:2082`, `:2116`), so the map hit fires first and the loose predicate is
only a fallback. ZGC has no such map, so its predicate is load-bearing alone.

### 3.2 AW-3 — the native-alloc-pressure signal is inert while the allocator aborts

```
vm_heap.rs:1145   VmHeap::Zgc(_) => false,   // young_spill_pressure
vm_heap.rs:1156   VmHeap::Zgc(_) => {}       // clear_young_spill_pressure
vm_heap.rs:1168   VmHeap::Zgc(_) => {}       // note_young_spill_pressure
```

The method doc (`:1125-1138`) states the exact failure this exists to prevent,
for G1: *"without it, a workload that allocates only from inside natives never
reaches ANY safepoint and G1's infallible allocator aborts the process on a heap
full of garbage."*

`ZgcRealHeap` has the same infallible allocator. `alloc_object` (`zgc.rs:1948-1951`)
and `alloc_array` (`zgc.rs:1983-1986`) both do
`eprintln!("FATAL: ZGC(real): out of heap space …"); std::process::abort();`.
And it has no pressure latch at all, so `vm/src/vm/vm_exec.rs:2707` — the
`safe_native_call` boundary that runs the GC natives cannot run themselves —
never fires for ZGC.

This is **not** a moving-collector issue. It is live on the backend as it exists
today, it produces a hard `abort()` with no Java-visible `OutOfMemoryError`, and
it is a plausible mechanism for members of the **22 FAIL** group in the
2026-08-07 comparison — which the plan currently assumes are "probably not all
GC" (`zgc-production-implementation-plan.md:221`). Worth checking those 22 logs
for the `FATAL: ZGC(real): out of heap space` line before spending Phase 5b
triage on them.

**Fix shape:** ZGC needs the G1 treatment — an `AtomicBool` latch on
`ZgcRealHeap`, set when `alloc_raw` succeeds with the arena above the trigger
threshold, mirroring `G1Collector::native_alloc_pressure` /
`clear_native_alloc_pressure` / `note_native_alloc_pressure` (`g1.rs:1779-1795`).
Three trivial methods and three one-line arm changes.

### 3.3 AW-4 — `enable_gc_logging` claims to do something it does not

```
vm_heap.rs:1977-1979   VmHeap::Zgc(_) => {
                           tracing::info!("[GC] Verbose GC logging enabled (ZGC-real collector)");
                       }
```

G1's arm calls `g1.enable_gc_logging()` (`:1970`). ZGC's prints a claim. A user
who passes `--verbose:gc` under `-XX:+UseZGC` is told logging is on and then
gets nothing, and `print_gc_summary` (`:1987`) has no ZGC branch either. Minor
on its own; it matters because it is the *observability* that Phase 0b
(`gc/src/zgc/metrics.rs`) depends on, and R1 of the risk register says the whole
Phase 2 justification is unmeasured. Fold into 0b.

### 3.4 AW-5 — the conservative-root path costs a mutex per stack word and an O(live) walk per operand slot

Two arms compose into this.

```
vm_heap.rs:527    VmHeap::Zgc(_) => None,                    // conservative_addr_span
vm_heap.rs:615    VmHeap::Zgc(h) => h.is_heap_addr(addr),    // is_heap_addr
```

`conservative_addr_span` is documented as "purely an optimization hint"
(`:518`), and structurally it is. But its one consumer,
`vm/src/jit/conservative_roots.rs:4052-4064`, hoists it out of the scan loop
precisely so that the overwhelming majority of stack words — return addresses,
ints, native pointers — are rejected by an inline `lo/hi` compare. With `None`,
that comment's own words apply: *"every word goes through the validator as
before."* For ZGC the validator is `ZgcRealHeap::is_object_address`
(`zgc.rs:1667-1674`), whose first act is `self.registry.lock()`. So a JIT-frame
conservative scan takes **one `parking_lot::Mutex` acquire per 8-byte stack
word**, on every root-gathering pass, on every thread.

`is_heap_addr` is worse. Its interior fallback (`zgc.rs:1683-1695`) iterates the
entire live-object registry and dereferences each base's header — **O(live) per
probe**, again under the registry lock, and it re-takes the lock at `:1684`
having just released the one from `:1679`. Its callers are the per-slot root
scanners: `vm/src/runtime/value_stack.rs:1301`, `:1587`,
`vm/src/memory/roots.rs:200`, `vm/src/runtime/frame.rs:1831`,
`vm/src/runtime/interpreter/gc_and_alloc.rs:4260`. Every ambiguous
long-vs-jobject operand slot that is *not* an exact object base pays a full heap
scan.

The registry field comment (`zgc.rs:1408-1415`) records that exactly this shape
— "`is_object_address` … O(roots × live) … read as a hang at scale" — is why the
registry was changed from a `Vec` to a `FxHashSet`. **That fix covered only the
exact-base fast path. The interior fallback still does the linear walk, and the
per-word mutex was never addressed at all.**

This is a specific, testable, competing explanation for the **35 `PASS → HANG`**
classes that
[`zgc-real-fullsuite-regression-RETIRED-20260807.md`](../fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260807.md)
attributes, as an explicitly-unverified hypothesis, to the missing young
generation. The two hypotheses predict different instruments: the young-gen
hypothesis predicts many full-heap GCs with long mark phases; this one predicts
time spent in root *collection* with GC count flat. Phase 0b's metrics should be
able to separate them in one run, and it is worth doing before Phase 2 is
staffed, per R1. It also gives WS 1d a second justification: real pages make
`conservative_addr_span` answerable (`[arena_base, arena_end)`, exactly as
`g1.rs:7547-7553` does it), which deletes the per-word mutex outright.

### 3.5 The worst affirmative arm has no arm

```
vm_heap.rs:551-553
    pub fn supports_jit_tlab_skip(&self) -> bool {
        true
    }
```

Its doc (`:531-550`) argues ZGC's `true` from two premises, both named
explicitly:

> *"ZGC (INT-3 residual): trivially safe — `ZgcRealHeap` is a **non-moving** STW
> mark-sweep whose sweep walks the allocation-base REGISTRY (never linear
> memory), and `Self::refill_tlab` **never hands ZGC mutators a TLAB**, so
> un-retired tails cannot exist."*

WS 3b deletes the first premise. WS 4a deletes the second. The value it gates is
consumed at `vm/src/runtime/interpreter/gc_and_alloc.rs:230`:

```rust
if !xt::enabled() || !shared.mem.heap.supports_jit_tlab_skip() { … }
```

— the gate on the collector **forcibly taking over an in-JIT peer thread**. And
the arms that are supposed to make that safe are no-ops for ZGC:

```
vm_heap.rs:565   VmHeap::Zgc(_) => {}   // set_jit_tlab_skip_regions  (G1: h.set_jit_tlab_skip_regions)
vm_heap.rs:575   VmHeap::Zgc(_) => {}   // clear_jit_tlab_skip_regions
```

So the composed post-4a/3b behaviour is: the VM is told take-over is safe, it
freezes a peer mid-JIT and publishes the peer's un-retired TLAB tail
(`gc_and_alloc.rs:487`), ZGC **drops the list on the floor**, and the relocation
pass moves objects the frozen peer holds raw pointers to. Silent heap
corruption, and the arm that causes it is a `{}`.

This is a top-three blocker and it does not appear in any `VmHeap::Zgc` grep,
which is presumably why the plan does not list it.

### 3.6 The pointer-map landmine, restated precisely

`ZgcRealHeap::collect_garbage` builds `HashMap::new()` at `zgc.rs:2441` and
returns it in `GcResult` at `:2455`; the type is
`HashMap<usize, usize>` per `collector.rs:110`. The class doc blesses it at
`zgc.rs:1393-1395`.

The plan names `remap_after_gc` and the two map-keyed liveness checks. The
sharper statement is that **an empty map is not neutral — it is an instruction
to use the pre-GC address**, at three sites:

| Site | Code | Effect with an empty map from a *moving* collector |
|---|---|---|
| `gc_and_alloc.rs:2287` | `pointer_map.get(&ref_addr).copied().unwrap_or(ref_addr)` | Nulls slot 0 of whatever now occupies the vacated address. |
| `gc_and_alloc.rs:2471-2497` | `match pointer_map.get(&ref_obj_old) { … None if watched_pre_gc_addr_survived(..) => ref_obj_old, None => continue }` | Either writes a live reference through a dead address (`:2524`), or silently drops the weak/phantom restore for **every** surviving reference. |
| `gc_and_alloc.rs:2541-2545` | `still_a_reference` | `retain_shaped_weak_phantom` (`:2552`) prunes live processor entries. |

Note the second row's two branches are decided by AW-2's predicate. **Fixing the
empty pointer map without fixing `is_addr_live` converts a silent drop into a
wild write.** They must land together.

The compare-to answer already exists in-tree twice: G1 emits identity entries
for self-forwarded survivors (`g1.rs:2082`, `:2116` via `identities(..)` /
`compose_forward_maps`), and `gen_heap` merges old-gen survivors into the map
(`gen_heap.rs:5107`). A relocating ZGC needs the same, and it needs identity
entries for **non**-relocated survivors too, because that is what
`watched_pre_gc_addr_survived`'s `pointer_map.contains_key` fast path
(`vm_heap.rs:2189`) is built on.

---

## 4. Full per-arm table

**Class**: S = SAFE, **NM** = NON-MOVING-ONLY, **AW** = ALREADY-WRONG, U = UNKNOWN.
**Breaks at** names the plan phase that invalidates the arm (`1d` pages, `2`
generational split, `3a` concurrent mark, `3b` relocation, `4a` TLABs).

| # | Line | Method | ZGC arm does | G1 arm does | Class | Breaks at |
|---|---|---|---|---|---|---|
| 1 | 316 | `try_alloc_object` | delegate `h.try_alloc_object` | delegate | S | — |
| 2 | 328 | `try_alloc_object_old` | `None` | `None` | S | — |
| 3 | 346 | `try_alloc_objects_old_batch` | `Vec::new()` | `Vec::new()` | S | 2 (opportunity) |
| 4 | 359 | `try_alloc_object_full` | delegate `try_alloc_object` | delegate `try_alloc_object` | S | — |
| 5 | 377 | `try_alloc_array_full` | delegate `try_alloc_array` | delegate `try_alloc_array` | S | — |
| 6 | 388 | `dbg_first_young_small_ref` | `None` | `None` | S | — |
| 7 | 414 | `alloc_object_with_descriptors` | delegate | delegate | S | — |
| 8 | 435 | `try_alloc_object_with_descriptors` | delegate | delegate | S | — |
| 9 | 451 | `try_alloc_array` | delegate | delegate | S | — |
| 10 | 510 | `is_object_address` | delegate → registry hash hit (`zgc.rs:1667`) | region containment + header-tag validation (`g1.rs:7555`) | S | 3b — the registry must be re-keyed by relocation, in `zgc.rs` |
| 11 | **527** | `conservative_addr_span` | `None` | `Some((arena_base, arena_end))`, lock-free (`g1.rs:7547`) | **AW** (perf) | fix at 1d |
| 12 | **551** | `supports_jit_tlab_skip` *(no arm; hardcoded `true`)* | `true`, argued from "non-moving" + "never hands out a TLAB" | `true`, argued from CSet exclusion + pinning | **NM** | **3b and 4a** |
| 13 | **565** | `set_jit_tlab_skip_regions` | `{}` | real: publishes the skip list (`g1.rs:7371`) | **NM** | **3b/4a** |
| 14 | **575** | `clear_jit_tlab_skip_regions` | `{}` | real (`g1.rs:7379`) | **NM** | **3b/4a** |
| 15 | 615 | `is_heap_addr` | delegate → registry hit, else **O(live)** extent walk (`zgc.rs:1677`) | arena-bounds gate + O(1) region index (`g1.rs:7600`) | **AW** (perf) | fix at 1d |
| 16 | **993** | `pin_critical_region` | `Vec::new()` | `pin_region_for_addr` (`g1.rs:6522`) — real region pin | **NM** | **3b** |
| 17 | 1015 | `get_array_element_unboxing` | delegate `get_array_element` (no unboxing) | same gap | S | — (pre-existing functional gap, not GC) |
| 18 | 1059 | `read_char_array_bulk` | per-element loop | per-element loop (humongous-safe) | S | — |
| 19 | 1110 | `needs_gc` | delegate | delegate | S | 2 |
| 20 | 1121 | `needs_gc_for_jit_allocation` | delegate `needs_gc` | delegate `needs_gc` | S | 2 |
| 21 | **1145** | `young_spill_pressure` | `false` | `native_alloc_pressure()` (`g1.rs:1779`) | **AW** | now |
| 22 | **1156** | `clear_young_spill_pressure` | `{}` | `clear_native_alloc_pressure()` | **AW** | now |
| 23 | **1168** | `note_young_spill_pressure` | `{}` | `note_native_alloc_pressure()` | **AW** | now |
| 24 | **1259** | `collect_garbage` | delegate → returns an **always-empty** `pointer_map` (`zgc.rs:2441`, `:2455`) | real map incl. identity entries (`g1.rs:2257`, `:2344`) | **NM** | **3b** |
| 25 | **1287** | `collect_garbage_with_finalizers` | delegate (`zgc.rs:1510`); returns pre-GC addresses as post-GC ("non-moving, so pre == post") | evacuates and returns post-collection addresses | **NM** | **3b** |
| 26 | **1308** | `write_barrier` | delegate → **no-op** (`zgc.rs:2459`, "Non-generational, non-concurrent") | RSet post-barrier | **NM** | **2 and 3a** |
| 27 | **1333** | `write_barrier_pre` | delegate → trait **default** (empty, `collector.rs:481`); `ZgcRealHeap` does not override | SATB pre-barrier into the thread-local buffer | **NM** | **3a** |
| 28 | **1355** | `write_barrier_keep_alive` | delegate → same empty default | SATB enqueue | **NM** | **3a** |
| 29 | 1368 | `allocated_bytes` | delegate | delegate | S | — |
| 30 | 1381 | `jit_card_table_info` | `None` | `None` | S | 2 (both become live work) |
| 31 | 1401 | `collection_count` | `h.gc_count() as u64` | `h.collection_count()` | S | — |
| 32 | 1414 | `heap_capacity` | delegate → `arena.lock().capacity()` | delegate | S | 1d |
| 33 | 1428 | `young_gen_stats` | `(0, 0)` | `eden_stats()` (`g1.rs:6848`) | S | **2** — `soft_ref_policy_free_mb` has an explicit `young_cap == 0` branch for this (`:1509-1512`) that must be revisited |
| 34 | 1442 | `old_gen_stats` | `(allocated_bytes, heap_capacity)` — whole heap reported as "old" | `old_gen_stats()` (`g1.rs:6862`) | S | 2 |
| 35 | 1537 | `satb_barrier` | `{}` | `satb_pre_barrier(addr)` | S today | **3a** |
| 36 | 1574 | `flush_thread_satb` | `{}` | drains the thread-local SATB buffer | S today | **3a** — and `satb.rs` reuse is the plan's own recommendation (`:162`) |
| 37 | 1584 | `old_gen_needs_gc` | `false` | `false` | S | 2 |
| 38 | 1594 | `old_gen_info` | `(0, 0)` | `(0, 0)` | S | 2 |
| 39 | 1605 | `old_gen_lock` | `None` | `None` | S | 2 |
| 40 | 1619 | `collect_young_to_old_roots` | `Vec::new()` | `Vec::new()` | S | 2 |
| 41 | 1633 | `enable_concurrent_gc` | `{}` | `{}` ("G1 has built-in concurrent marking") | S | 3a |
| 42 | 1668 | `try_alloc_young_probe` | `needs_gc() ? None : Some(())` | identical | S | 2/4a |
| 43 | 1694 | `young_bump_headroom` | `!self.needs_gc()` | `!self.needs_gc()` | S | 4a — moot today because `refill_tlab` (#48) is `None` |
| 44 | 1711 | `young_has_free_block` | `!self.needs_gc()` | `!self.needs_gc()` | S | 4a — same |
| 45 | 1730 | `g1_should_start_marking` | `false` | real IHOP test | S | — (G1-specific by name) |
| 46 | 1740 | `g1_is_marking_active` | `false` | `gc_state.is_marking_active()` | S | — (G1-specific by name) |
| 47 | 1809 | `g1_concurrent_mark_step` | `true` | `concurrent_mark_step()` | S | — **dead: no callers outside `vm_heap.rs`** |
| 48 | **1977** | `enable_gc_logging` | `tracing::info!` only — enables nothing | `g1.enable_gc_logging()` | **AW** | fix at 0b |
| 49 | 2114 | `refill_tlab` | `None` | `refill_tlab()` (`g1.rs:6993`) | S | **4a** — and #12's soundness argument cites this staying `None` |
| 50 | **2154** | `is_addr_live` | `h.is_heap_addr(addr).is_some()` — **interior-accepting** | `is_addr_in_live_region` (`g1.rs:7616`), backed by a real pointer map | **AW** | now — §3.1 |
| 51 | **2208** | `watched_pre_gc_addr_survived` | `self.is_addr_live(addr)` | `self.is_addr_live(addr)`, with a real map hit first | **AW** (inherits #50) + **NM** | now, and **3b** |
| 52 | 2237 | `reclaimed_hole_at` | `None` | `None` | S | — (answerable from the arena free list; diagnostic gap only) |
| 53 | 2257 | `young_inactive_semispace_range` | `None` | `None` | S | 2 |
| 54 | 2270 | `is_in_young_addr` | `false` | `false` | S | 2 — **dead: no callers** |
| 55 | **2304** | `metadata_pin_deferrable` | `true` | `true` | **NM** | **2** (young-only cycles end the "walks every live region" premise) **and 3b** (address-keyed side table, never re-keyed) |
| 56 | **2344** | `mirror_pin_deferrable` | `true` | `true` | **NM** | **2 and 3b** — same |
| 57 | **2382** | `pre_gc_addr_did_not_survive` | `!self.is_addr_live(addr)` | `!self.is_addr_live(addr)` | **AW** (inherits #50) + **NM** | now, and **3b** |

### Non-arm rows in the same blast radius

| Line | Method | Behaviour for ZGC | Class | Breaks at |
|---|---|---|---|---|
| 228 | `dispatch!` macro | pure delegation, 24 sites | S | — (two callees carry debt: `walk_objects`, `get_header` — see §2) |
| **694** | `load_and_forward` | backend-agnostic; reads `ObjectHeader::is_forwarded()` / `forwarding_address()` | **NM** | **3b** — ZGC's design (WS 1e, `gc/src/zgc/forwarding.rs`) is a lock-free forwarding **table**, not a header Brooks pointer. This barrier would silently return the *stale* object. Cross-owner item for 1e/3b. |
| **1094** | `array_data_ptr` | unconditional `Some(obj + ARRAY_DATA_OFFSET)`, no match at all | **NM** | **3b** — hands out a raw interior pointer with no pin; the doc already makes it the caller's problem ("must not let it be collected **or moved**"), and under concurrent relocation no caller can honour that without a barrier. |
| 1001 | `unpin_critical_regions` | `if let VmHeap::G1` — ZGC falls through silently | S | 3b (pairs with #16) |
| 589 | `debug_forwarded_target` | `_ => None` | S | — diagnostic |
| 598 | `debug_minor_gc_count` | `_ => 0` | S | 2 |
| 644 | `liveness_arms` | `_ => (false, false, "n/a")` | S | — diagnostic |
| 1176 | `young_arena_diag` | `_ => (0, 0, 0, 0)` | S | 2 |
| 1187 | `live_bytes_estimate` | `_ => self.allocated_bytes()` | **U** | Correct only if `allocated` tracks live bytes. `zgc.rs:2424` stores `bytes_copied` (= survivors) post-sweep, so it does — **until** an allocation-heavy interval, during which it is live+dead. Would need a run to say whether the GC-overhead productivity metric is misled. Flag for 0b. |
| 1198 | `bytes_promoted_total` | `_ => 0` | S | 2 |
| 1222 | `old_gen_headroom` | `_ => capacity - allocated` | **U** | Consumed by the GC-overhead death-spiral limiter. Its Generational contract is "can the *tenured* space absorb a young drain"; for a single-space collector whole-heap headroom is the same question, but the arena's **free-list fragmentation** is invisible to it (`zgc.rs:2378-2409` coalesces, but a fragmented arena still reports headroom it cannot serve). Needs a measurement, not a source read. |
| 1241 | `young_occupancy` | `_ => (0, 0, 0, 0)` | S | 2 |
| 2228 | `live_holders_of` | `_ => Vec::new()` | S | — diagnostic |

---

## 5. Prioritized remediation

### P0 — fix now, independent of any ZGC phase

| # | Item | Where | Why now |
|---|---|---|---|
| **P0.1** | Make the `zgc` feature compile again | `gc/src/zgc.rs` (§1) | Nothing else in this document is verifiable, and no Phase-1 module can be integration-tested, until it does. The new `ci.yml` steps will be red on their first run regardless of who lands them. |
| **P0.2** | `is_addr_live` → `is_object_address` | `vm_heap.rs:2154` | AW-2. One line. Removes a live corruption path (§3.1) *and* the O(live) walk from the reference-processing hot path. |
| **P0.3** | Add the native-alloc-pressure latch | `zgc.rs` + `vm_heap.rs:1145/1156/1168` | AW-3. Hard `abort()` with no OOME today; check the 22 FAIL logs for `FATAL: ZGC(real)` before Phase 5b triage. |
| **P0.4** | Instrument, then re-test the HANG hypothesis | `gc/src/zgc/metrics.rs` (WS 0b) + `vm_heap.rs:1977` | AW-4/AW-5. §3.4 gives a second, cheaper explanation for the 35 HANG classes than the generational split. R1 of the risk register already says not to staff Phase 2 on an unmeasured inference; this makes the alternative concrete. |

### P1 — blocking before WS 3b (concurrent relocation) lands

Ranked by "how loudly does it fail". Everything here is silent.

| Rank | Arm(s) | G1's answer — the shape to copy | Failure if unfixed |
|---|---|---|---|
| **1** | `collect_garbage` / `collect_garbage_with_finalizers` — the empty `pointer_map` (`vm_heap.rs:1259`, `:1287`; `zgc.rs:2441`) | `g1.rs:2257-2344`: a real map, **including identity entries for self-forwarded survivors** (`identities(..)`, `compose_forward_maps`, `g1.rs:2082/2116`) | Every consumer at `gc_and_alloc.rs:2287`, `:2471-2497`, `:2541` uses the **pre-GC** address. Wild write into the vacated span. §3.6. |
| **2** | `is_addr_live` / `watched_pre_gc_addr_survived` / `pre_gc_addr_did_not_survive` (`:2154`, `:2208`, `:2382`) | `g1.rs:7616` + the map hit that fires first | Must land **with** rank 1: fixing the map alone converts a silent drop into a wild write, fixing the predicate alone leaves every survivor unresolvable. §3.6. |
| **3** | `supports_jit_tlab_skip` + the two skip-region no-ops (`:551`, `:565`, `:575`) | `g1.rs:7371`/`:7379`, plus CSet exclusion and `gc_quiescence::add_pinned_jit_root` | The VM is told forcible in-JIT take-over is safe (`gc_and_alloc.rs:230`) and ZGC then relocates memory a frozen peer holds raw pointers into. §3.5. |
| **4** | `pin_critical_region` → `Vec::new()` (`:993`) | `g1.rs:6522` `pin_region_for_addr` + `unpin_region` | `GetPrimitiveArrayCritical` (`vm/src/native/jni.rs:4614`) holds a detached copy across a relocation; `Release` copies back to the **old** address. |
| **5** | `metadata_pin_deferrable` / `mirror_pin_deferrable` → `true` (`:2304`, `:2344`) | `true`, but G1's `metadata_pin` consumers walk every live region in the same full-mark pass | Two independent breakages: Phase 2's young-only cycles end the uniform-walk premise (`:2294-2298`), and the three side tables (`loader_pin`, `mirror_pin`, `metadata_pin`; consumed `zgc.rs:2225-2233`) are **address-keyed with no re-key path**. A deferred root whose key moved is unreachable → silently reclaimed → the "live object reads back as a different type" shape `vm_heap.rs:2286-2292` documents. |
| 6 | `array_data_ptr` (`:1094`) and `load_and_forward` (`:694`) | — (both are backend-generic) | Raw interior pointers with no pin; and a Brooks-pointer read barrier that a forwarding-**table** design leaves answering with the stale object. Cross-owner: WS 1e. |

Per the plan's R6 (`zgc-production-implementation-plan.md:298`): for any P1 arm
whose moving answer is not yet known, prefer `unimplemented!()` over a neutral
value. A panic inside a default-off feature is cheaper than a UAF. Ranks 3, 4
and 5 are the strongest candidates for that treatment while 3b is in flight —
all three are `{}` / `Vec::new()` / `true`, and none of them will ever fail loudly.

### P2 — follows the phase it belongs to, not blocking

* **Phase 1d (pages):** `conservative_addr_span` (`:527`) becomes answerable —
  copy `g1.rs:7547-7553` verbatim. `heap_capacity` (`:1414`) stops being one
  arena's capacity.
* **Phase 2 (generational):** `young_gen_stats` (`:1428`), `old_gen_stats`
  (`:1442`), `old_gen_needs_gc` (`:1584`), `old_gen_info` (`:1594`),
  `old_gen_lock` (`:1605`), `collect_young_to_old_roots` (`:1619`),
  `jit_card_table_info` (`:1381`), `write_barrier` (`:1308`),
  `is_in_young_addr` (`:2270`), `young_inactive_semispace_range` (`:2257`),
  and the wildcard rows `:598 :1176 :1198 :1241`. Also revisit
  `soft_ref_policy_free_mb`'s `young_cap == 0` special case (`:1509-1512`),
  which is written *for* ZGC's current `(0, 0)`.
* **Phase 3a (concurrent mark):** `write_barrier_pre` (`:1333`),
  `write_barrier_keep_alive` (`:1355`), `satb_barrier` (`:1537`),
  `flush_thread_satb` (`:1574`), `enable_concurrent_gc` (`:1633`). Reuse
  `gc/src/satb.rs` rather than growing a second implementation — and note that
  `ZgcRealHeap` does not override the trait's empty `write_barrier_pre` default
  (`collector.rs:481`), so this is an *addition*, not an edit.
* **Phase 4a (TLABs):** `refill_tlab` (`:2114`), `young_bump_headroom`
  (`:1694`), `young_has_free_block` (`:1711`), `try_alloc_young_probe`
  (`:1668`). Landing 4a *without* rank 3 above is the specific combination that
  corrupts.

### P3 — fine forever, or cleanup

The 33 SAFE rows, minus those already routed above. Two are dead code and can be
deleted or given a `matches!(self, VmHeap::G1(_))`-shaped guard instead of an
arm: `g1_concurrent_mark_step` (`:1809`) and `is_in_young_addr` (`:2270`).
`get_array_element_unboxing` (`:1015`) is a real functional gap shared with G1
and unrelated to GC — it belongs in whatever owns the unboxing accessors, not
here.

---

## 6. What this audit did not establish

* **Whether any of it runs.** The feature does not compile (§1); every behaviour
  above is read from source.
* **The two UNKNOWN rows** (`live_bytes_estimate` `:1187`,
  `old_gen_headroom` `:1222`). Both feed the GC-overhead productivity limiter,
  and both would need a run with Phase-0b instrumentation to judge — a source
  read cannot tell whether an arena that reports headroom can actually serve it.
* **Whether AW-5 or the missing young generation explains the 35 HANG
  classes.** §3.4 argues only that AW-5 is a live, un-eliminated alternative
  with a cheaper fix, and that the two predict different instrument readings.
* **`gc/src/zgc/` is on disk but unwired.** Six modules (`barrier.rs`,
  `forwarding.rs`, `metrics.rs`, `page.rs`, `remembered.rs`, `vaddr.rs`) exist
  untracked with no `mod` declaration reaching them, so none is compiled or
  audited here. Two of them (`page.rs`, `remembered.rs`) are byte-for-byte the
  same size (86 851), which is worth a glance from whoever owns them.
* **`gc/src/zgc.rs`'s address-keyed state** — still explicitly out of scope for
  [`gc-crate-audit.md`](gc-crate-audit.md) §5.4 and still owed. §3.1's coalesce
  interaction and the three unre-keyed pin tables in P1 rank 5 are two entries
  that audit will need.
