# bintrees18: the non-moving young-sweep saga — three layered bugs, and why selective promotion can't be made correct

Status: **bintrees18 corruption + thrash FIXED and committed; the remaining
wrong-result is proven fundamental (needs precise JIT stack maps).**

This document is the consolidated, decisive record of the `bintrees18`
investigation. It supersedes the open hypotheses in
`jit-osr-main-corruptor-investigation.md` and complements
`jit-safepoint-revert.md`. Every conclusion below was reached by **building and
running**, not by reasoning alone; each hypothesis was eliminated with a named,
gated instrument.

The reproducer (all runs `--java-home <jdk-25> --Xmx 8g`):

```
./target/release/cratonvm.exe --stack-dump-on-timeout 0 --Xmx 8g \
  -cp bench BenchSuite bintrees18
```

`bench/BenchSuite.java` `binaryTrees`: builds a depth-(N+1) stretch tree, a
long-lived depth-N tree, and many short-lived trees; `check(n)` counts nodes
(leaf → 1, internal → 1 + check(l) + check(r)). Golden checksums (must match
HotSpot): bt10=135854, bt14=3222190, bt16=14985902, **bt18=67674804**.

`FORCE_MOVING` (`CRATONVM_DBG_FORCE_MOVING=1`) completes bt18 correctly in ~27s
and is the control throughout.

---

## TL;DR

`bintrees18` was **three independent bugs** in the non-moving young-gen sweep
(`gc/src/gen_heap.rs::sweep_young_non_moving`), which runs instead of the moving
Cheney collector whenever GC quiescence is active (live JIT frames present):

1. **THRASH** — the GC trigger keyed on the raw bump cursor, not live occupancy.
   **FIXED** (`6a027e0`). bt10/14/16 now pass.
2. **"CORRUPTION"** — a mark-clearing re-walk ran before freed spans were
   published to the free list, desyncing into a live node's interior cell. Not
   real corruption; a false positive from a hole-unaware walker. **FIXED**
   (`9ff6b09`). bt18 `inconsistent header` warnings 2 → 0.
3. **PROMOTION-AT-SCALE** — the non-moving sweep can't tenure the depth-18 live
   set out of young. Tackled via **selective promotion** + an **allocator probe
   fix** (probe fix landed; bt18 now *completes* in ~33s). Selective promotion
   itself is gated `CRATONVM_SELECTIVE_PROMOTE`, **default-off, KNOWN-BUGGY**:
   it yields a wrong checksum at bt18 scale, proven to be the
   **register-invisibility** problem — unfixable without precise JIT stack maps.

Two big theories were **refuted** before the real bugs were found: "the sweep
zeroes a live Node" and "a missing JIT safepoint-spill". See below.

---

## Bug 1 — THRASH: GC trigger on the bump cursor (FIXED, `6a027e0`)

### Symptom
`bintrees18` → `rc=124` (perpetual timeout). Smaller trees (bt10/14/16) also
timed out under the non-moving path.

### Root cause
`needs_gc()` triggered on `young_from.used()`, which returns the **raw bump
cursor** (`Arena::used()` = `self.cursor`), ignoring the free list. The
non-moving sweep reclaims dead objects into `Arena::free_list` (which
`Arena::alloc` *does* reuse) but **cannot retreat the cursor** — it can't
relocate survivors while conservative JIT roots are live. So once the cursor
reaches capacity, `used()` stays pinned near capacity and `needs_gc()` is
permanently true → a young GC fires on essentially every allocation. The moving
collector is immune because Cheney resets the cursor into a fresh to-space each
cycle.

### Fix
Trigger on live occupancy `used() - free_list_bytes()` instead of raw `used()`.
No-op on the moving path (free list empty there); the alloc-failure→GC-retry
path remains the fragmentation backstop.

### Result
bt10/14/16 pass on the non-moving path (rc=0, golden checksums); sieve250k /
matrix600 unaffected.

---

## Bug 2 — "CORRUPTION" was a hole-unaware walker desync (FIXED, `9ff6b09`)

### Symptom
Non-deterministic `GC: inconsistent header — kind=Object but array_length=1
(num_slots=384, class_id=4)` warnings.

### How it was localized
`CRATONVM_DBG_HDR_BT` dumped the bytes around each misread header plus a
backtrace. Decoding a wide window showed the surrounding Nodes were **perfectly
valid** (72-byte, `num_slots=2`, consecutive identity hashes). The misread
address was a *live* Node's **interior `Value::Object` field cell** (`class_id=4`
is the `Value::Object` discriminant, not 0) — i.e. a linear walker had desynced
off the object grid by 16 bytes, not real heap corruption.

### Root cause
`clear_all_mark_bits_in_arena` (the defence-in-depth mark-clear re-walk at the
end of `sweep_young_non_moving`) ran **before** the just-swept dead spans were
published to the free list. The main sweep loop zeroed each dead object in place
but only collected the spans in a local `dead_regions` vec; the mark-clear
re-walk skips only free-*list* regions, so it strode into a not-yet-published
zeroed span, decoded it as a `num_slots=0` (40-byte) object, and overshot into a
live Node's field cell when the span wasn't a multiple of `HEADER_SIZE`. The
main sweep loop never desynced because it knew each object's size before zeroing
it. Cheney is immune (it resets from-space each cycle, so holes never
accumulate).

### Fix
Publish `dead_regions` to the free list **before** `clear_all_mark_bits_in_arena`,
and harden the two other hole-unaware linear from-space walkers
(`mark_young_to_old_refs`, `fixup_young_old_refs`) to skip free-list holes.
**Invariant:** every linear young-from-space walk must skip the non-moving
sweep's free holes.

### Result
bt18 `inconsistent header` 2 → 0 across all runs; bt16 still golden; no
regression.

---

## Bug 3 — PROMOTION-AT-SCALE

After Bugs 1 & 2, bt18 no longer thrashes-by-cursor or "corrupts", but the
non-moving sweep still cannot drain the depth-18 live set out of young (it never
moves anything). `CRATONVM_DBG_SWEEP_EDGES` (classify every about-to-be-swept
node by inbound referrer: passed-in root, young survivor, old-gen via full
`walk_objects`) reported **edges=0 on all 151 sweeps** — the sweep *never* frees
a referenced node. The first sweep reclaims ~29.3M genuinely-dead nodes cleanly;
then the ~527k-node persistent tree fills young and the sweep reclaims ~0.

So the fix direction is **selective promotion**: evacuate the live set to old
gen. The moving collector already tenures (`PROMOTION_AGE=3` + a 25%-pressure
"always tenure" flag); the non-moving sweep does none of that.

### 3a — The allocator probe fix (LANDED, correct)

First attempt at selective promotion *drained young correctly* (telemetry:
`evac=527001`, young `used−free ≈ 7KB`, old stable at 36MB) but bt18 still
`rc=124`. Per-GC timing (`CRATONVM_SP_TIME`) showed **~840 GCs/sec**, each 0ms,
reclaiming nothing, with `eff_used=0M ≪ thr=1024M` — i.e. **not** `needs_gc`.

Root cause: `try_alloc_young_probe` checked **only the bump cursor** (`used()`
vs `capacity()`), ignoring the free list. With the cursor stuck at capacity, the
probe reported false-OOM with **gigabytes free**, so the JIT allocation slow
path (`jit_new_object`, `vm/src/jit/helpers.rs`) retired its TLAB and forced a
GC on *every* slow-path allocation.

**Fix:** the probe falls back to `Arena::largest_free_block()` when the bump
tail can't satisfy the request. **bintrees18 now COMPLETES in ~33s** (vs
FORCE_MOVING's 27s). Default path verified unaffected (bt16 gate-off + sieve250k
golden). This fix is correct and landed.

### 3b — Selective promotion: correct heap fixup, wrong result

With the probe fix, bt18 *completes* but produces a **deterministic, heap-size-
independent wrong checksum: 68332206 vs golden 67674804** (same value at -Xmx
4g/8g/12g). `check()` is *inflated*, meaning some node's child points to a
larger-than-correct subtree.

The implementation (`sweep_young_non_moving`, gated `CRATONVM_SELECTIVE_PROMOTE`):
- **Pin set** = every young address appearing as a root/finalizer *value*
  (conservative false-positives included) — so an evacuated object's address can
  never equal a conservative slot value, and the VM-level `pointer_map` remap
  can't corrupt a non-pointer slot.
- **Evacuate** non-pinned, aged, marked survivors to old gen with forwarding
  pointers; **fix up** all references (surviving-young (3a), evacuated-old (3b),
  dirty-card old (3c)) by following forwarding pointers; dirty cards for new
  old→young edges; return the young→old map for the VM remap; reclaim evacuated
  slots; coalesce free blocks.

It is **exact on bt10/14/16** (golden checksums) and drains young correctly. The
bug is scale/old-gen-volume dependent.

---

## The elimination chain for Bug 3b (all by gated instrumentation)

| # | Hypothesis | Instrument / test | Result |
|---|---|---|---|
| 1 | Sweep zeroes a live node | `CRATONVM_DBG_SWEEP_EDGES` | edges=0 every sweep → **refuted** |
| 2 | Missing JIT safepoint-spill | real `x64.rs` CalleeSaved-operand flush + 2 prior experiments | corruption unchanged; dominated by full-stack scan → **refuted** (see `jit-safepoint-revert.md`) |
| 3 | Register-only dangling *read* | "don't zero evacuated slots" variant | didn't fix the checksum → **refuted** |
| 4 | Clean-card old→young miss | replace dirty-card (3c) with a full old-gen fixup walk | didn't fix the checksum → **refuted** |
| 5 | `old_gen.alloc` dst collision | `CRATONVM_SP_TRACE`: sort evac destinations, detect overlaps/dups | `dst_overlaps=0 dst_dups=0` (evac=526998) → **refuted** |
| 6 | Missed heap fixup | `CRATONVM_SP_VERIFY`: count surviving refs still pointing at a forwarded-young object | `old=0 young=0` → **refuted** |
| 7 | Heap aliasing (wrong-address rewrite) | `CRATONVM_SP_VERIFY`: incoming-ref count per evacuated object | `max_incoming=1`, `aliasing=0` → **refuted** |
| 8 | **Register-invisibility (non-heap ref to a moved object)** | by elimination + FORCE_MOVING behavior | **confirmed** |

The decisive observation: after the full fixup the heap is a **structurally
correct forest** (no missed fixup, no collision, no aliasing — every evacuated
node has exactly one incoming heap reference), **yet the checksum is wrong**. A
structurally-valid heap with a wrong `check()` result is only possible if the
*wrong tree was built*, i.e. a reference **outside the heap** was stale.

---

## Root cause (confirmed): register-invisibility

Selective promotion — like *any* moving GC — is **fundamentally unsafe under
CratonVM's conservative JIT roots**:

- During `make()`'s bottom-up construction, intermediate Node references live on
  the JIT operand stack / in registers.
- The GC root set comes from a **conservative** scan
  (`conservative_roots::scan_active_jit_frames`) plus precise VM roots. A value
  that lives only in a JIT register not spilled to the stack at the safepoint is
  **invisible** to that scan → it is **not in the pin set**.
- Selective promotion evacuates that object to old gen and **reuses** its young
  slot. The uncaptured register reference now points at a reclaimed/reused slot.
- `make()` continues with the stale reference and links a **wrong child** → a
  structurally-valid-but-**wrong-size** tree (each node still has one parent, so
  it passes every heap check) → inflated `check()`.

**Why FORCE_MOVING is safe and selective promotion is not:** Cheney does *not*
copy an object that isn't reachable from its root set; it leaves it in from-space
**un-zeroed and un-reused** until the semispaces flip. So an uncaptured-register
reference reads the stale-but-intact copy and happens to work. Selective
promotion frees the young slot for reuse, so the same stale read returns a
*different* object's data. (The "don't zero" experiment didn't help precisely
because the slot is still *reused* — leaving the bytes intact isn't enough.)

This is the **same register-invisibility problem** that opened the bintrees18
saga (the original "value held in a register" theory) and that doomed the
safepoint-spill attempts. It is **unfixable without precise JIT stack maps** —
the long-standing unsolved hard problem for this VM.

### Verdict
- The **non-moving sweep is the only safe collector** while JIT frames are live.
- bintrees18's throughput limit is **fundamental**: you can't drain young
  without moving, and you can't move safely without precise maps.
- Any future selective-promotion fix must either (a) add **precise JIT stack
  maps**, or (b) make evacuated young slots **non-reusable until provably dead**
  (Cheney-style, no free-list reuse) — not the current free-list reuse.

---

## What landed on `dev`

| Commit | Change |
|---|---|
| `6a027e0` | `fix(gc): trigger young GC on live occupancy, not raw bump cursor` (Bug 1) |
| `9ff6b09` | `fix(gc): make non-moving-sweep free holes parseable to all from-space walkers` (Bug 2) |
| *(probe fix)* | `try_alloc_young_probe` consults the free list (`Arena::largest_free_block`) — bt18 completes in ~33s |
| `19af3ec` | restore evacuated-slot zeroing + mark `CRATONVM_SELECTIVE_PROMOTE` KNOWN-BUGGY |
| `5484f75` | gated `CRATONVM_SP_TRACE` evac dst-collision detector (refutes collision) |
| `7b87baa` | gated `CRATONVM_SP_VERIFY` missed-fixup + aliasing verify; register-invisibility conclusion |

Selective promotion stays `CRATONVM_SELECTIVE_PROMOTE`-gated, **default-off,
KNOWN-BUGGY**, with the proven reason it can't be correct as-is.

---

## Diagnostic env gates added (all default-off)

- `CRATONVM_DBG_SWEEP_EDGES` — classify each about-to-be-swept node by inbound
  referrer (root / young-survivor / old-gen-via-`walk_objects`).
- `CRATONVM_DBG_HDR_BT` — on an `inconsistent header` detection, dump a wide
  byte window around the misread header with plausible-header annotations.
- `CRATONVM_SELECTIVE_PROMOTE` — enable selective promotion (KNOWN-BUGGY).
- `CRATONVM_SP_TRACE` — per-GC evacuation dst overlap/duplicate detector.
- `CRATONVM_SP_VERIFY` — post-fixup missed-forwarded-ref + incoming-ref aliasing
  detector (`for_each_ref` / `forwarded_ref_count` helpers).

Existing knobs used: `CRATONVM_DBG_FORCE_MOVING` (control), `CRATONVM_DISABLE_JIT`
(interpreter — precise roots; too slow to complete bt18, inconclusive).

---

## Refuted theories worth not re-treading

- **"The non-moving sweep zeroes a live Node."** Refuted (`SWEEP_EDGES`
  edges=0). The "corruption" was Bug 2 (a hole-unaware walker desync), and the
  remaining wrong-result is register-invisibility, not a freed-live-node.
- **"A missing JIT safepoint-spill of live oops."** Implemented as a real
  CalleeSaved-operand flush in `x64.rs`; corruption unchanged. It is provably
  dominated by the full-native-stack conservative scan, which already failed.
  Do **not** re-attempt safepoint-spill (also `jit-safepoint-revert.md`:
  +40% perf, new SEGVs).
- **"old_gen.alloc collision / wrong-address fixup."** Refuted (`SP_TRACE`
  overlaps=0; `SP_VERIFY` aliasing=0, missed=0). The heap fixup is correct.

---

## Process hazards encountered (Windows shared checkout)

- **`.exe`-lock build trap:** a running `cratonvm.exe` (often a timed-out
  bintrees18 outliving its `timeout` wrapper, or rust-analyzer's background
  cargo) locks/deletes `target/release/cratonvm.exe`, so `cargo build` reports
  `Finished` without relinking — serving a stale binary. Verify the binary
  contains a unique string literal from the change, or copy to a stable path
  (`/tmp/cvm_*.exe`) immediately after a confirmed `Finished` and run the copy.
- **Shared working tree:** a parallel session sharing this checkout committed
  in-progress `CRATONVM_SELECTIVE_PROMOTE` work into HEAD under an unrelated
  message — shared-working-tree commits sweep up other sessions' uncommitted
  changes.
