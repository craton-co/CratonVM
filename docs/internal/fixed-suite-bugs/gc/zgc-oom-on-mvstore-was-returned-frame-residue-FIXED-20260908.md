# ZGC OOMs with 86% of the heap free: it was returned-frame RESIDUE, not an unregistered entry frame — FIXED 2026-09-08

**RETIRES `docs/known-issues/h2/zgc-oom-on-mvstore-is-the-unregistered-entry-frame-blocking-compaction-20260907.md`.**
That page characterised the failure correctly down to the last counter and then
named the wrong frame. Its §5 concluded *"the fix is to make the entry frame
provable — register it, or give it precise oop maps"* and explicitly warned the
next reader **not** to weaken the refusal. There was no entry frame to register:
every hit the probe raised was the leftover return address of a compiled frame
that had already returned, and the refusal was being raised against a stack word
that belonged to nothing.

| | |
|---|---|
| **Status** | **FIXED.** One-term change in `vm/src/jit/conservative_roots.rs`; kill switch `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0` restores the old behaviour exactly. |
| **Symptom** | `probes/MvsCreate.java` at `-Xmx256m` on ZGC: `OutOfMemoryError` in 9-19 s with 88% of the heap free and the largest free block at 0.6% of capacity. |
| **Before** | `compaction_cycles=1 objects_relocated=4421 relocation_skipped_jit=3`, `coverage-proof-incomplete=3`, `unregistered-jit-frame-on-stack=3`, **`rc=1` in 9 s**. |
| **After** | `compaction_cycles=6 objects_relocated=1031610 relocation_skipped_jit=0 relocation_on_proven_jit=6`, **`rc=0` in 36 s**. |
| **Where** | Azure host 2 (`20.80.105.49`), Linux, branch `claude/h2-known-issues-retire-20260908` off `origin/dev` `2b0913374`, H2 2.4.249 at `apps/h2database/h2`, JDK 25.0.4, release binaries. |

---

## 1. What the retired page got right

Everything except the attribution, and it is worth keeping because the new
reading has to explain all of it:

* **Two ingredients.** `--nojit` removes the refusal (`relocation_skipped_jit`
  10 → 0, compaction 1 of 11 → 11 of 14) and passes; `CRATONVM_ZGC_TLAB=0`
  passes *while still being refused*, because it never shreds the free list.
  One ingredient creates the fragmentation, the other forbids repairing it.
* **It is not the collector's throughput.** G1 and Generational run the same
  workload to `rc=0` in minutes; ZGC died in under ten seconds.
* **The A5 frame-shape filter converts nothing here.** `shaped=2` of `hits=2` on
  every refusing cycle, and turning `CRATONVM_JIT_A5_SHAPE_FILTER=1` on still
  OOMs. That measurement stands and this page does not re-open it.

The one thing it did not do is ask the *other* screen the same question.

## 2. The actual cause: a residue short-circuit in the accept rule

`scan_active_jit_frames` probes the native stack above the registered JIT entry
chain for a word that lands inside a JIT code range, and treats a hit as a
compiled frame that is live without a `JitEntryGuard`. The hit is then
classified:

```rust
let accept = match probe {
    None => false,
    Some((hit_slot, _)) => {
        let residue_hi = jit_residue_hi();
        chain_len > 0                    // <-- this term
            || residue_hi == 0
            || hit_slot >= residue_hi
            || unreg_jit_accept_residue()
    }
};
```

`JIT_RESIDUE_HI` is the highest `entry_sp` among the JIT entries that have
**returned** on this thread. A compiled frame writes only below its own
`entry_sp`, so once it returns every return address it left into JIT code lies
below that mark — indistinguishable, by inspection, from a live guardless frame.
That is exactly what the mark is for, and the sibling probe in
`refresh_moving_young_coverage_for_current_thread` has consulted it by default
since the H2 `FileNioMapped.unMap` investigation.

The `chain_len > 0` disjunct disables it, on this argument (quoted from the call
site): *"with entries on the chain, `search_lo` is already `cover_hi`, so
anything found above it is a frame the chain does not cover and must be
marked."*

**That is right about marking and wrong about liveness.** `JIT_RESIDUE_HI` is
MONOTONIC over the thread's whole life — its own doc says why, and says it
deliberately is not reset on a push — so a shallower JIT call that returned long
ago leaves residue at addresses **above** the current chain's `cover_hi`. The
band `[cover_hi, residue_hi)` is then scanned, its leftovers read as a live
guardless frame, and the cycle is refused. "Not covered by the chain" and "live"
are not the same statement, and the disjunct conflates them.

### The evidence, from the failing run

`CRATONVM_DBG_A5_FALLBACK=1`, all three refusing cycles of `MvsCreate 200000`:

```text
[a5-fallback] #1 slot=0x714fd47b25f8 word=0x714fb7b22335 residue_hi=0x714fd47b309f
              is_residue=true shapeless=false filtered=true
              band=[0x714fd47b2486,0x714fd47c0000) chain_len=1
[a5-fallback] #2 ... same slot, same word, chain_len=1
[a5-fallback] #3 ... same slot, same word, chain_len=3
[a5-fallback] TOTALS hits=3 residue=3 shapeless=0 filtered=3
```

Read it as a picture of the stack: the hit sits `0x172` bytes above `search_lo`
and `0xaa7` bytes **below** the residue mark. The band above the mark — the
52 KB between `residue_hi` and the stack top, where the process entry point's
frame would be — contains no hit at all. The retired page's §4 census
(`hits=2 shaped=2`) was reporting the *shape* of that same residue word; shape
cannot tell a returned frame's return address from a live one, because they are
the same instruction.

The coverage probe classified it correctly (`is_residue=true filtered=true`) and
declined. The marking probe accepted it, and only the marking probe raises
`UNREGISTERED_JIT_FRAME`. **The two screens disagreed, and the wrong one owned
the refusal.**

## 3. The fix

`vm/src/jit/conservative_roots.rs` — separate the two questions the one `accept`
was answering:

* **Marking is unchanged.** Any accepted hit still conservatively scans the FULL
  `[search_lo, high)` band. A conservative mark is over-retention, never a
  correctness risk, and `memory/roots.rs` republishes those addresses through
  `publish_pinned_jit_roots`, whose page ZGC withholds from every relocation.
  Nothing this change does can make an object less reachable.
* **The relocation refusal is raised only for a hit the residue mark cannot
  explain.** When the hit is residue, the band `[residue_hi, scan_hi)` — one no
  returned frame on this thread can have written — is re-probed, and only a hit
  *there* sets `set_unregistered_jit_frame_on_stack()` /
  `mark_moving_young_coverage_incomplete_because(UNREGISTERED_JIT_FRAME)`.

The re-probe is load-bearing and is **not** "believe the first hit was residue
and stop": `native_stack_has_jit_frame` returns the LOWEST hit in the band, so a
residue hit can hide a genuine one above it. Re-probing from the mark upwards is
what keeps the one live guardless frame the whole probe exists for — the process
entry point, which sits above every JIT entry the run ever makes — detectable.

`gc/src/gc_quiescence.rs` gains the census the refusal never had, and
`gc/src/vm_heap.rs` prints it under `CRATONVM_GC_STATS=1`:

```text
[GC] zgc-unregistered-jit-frame: hits_above_residue_mark=0 hits_explained_by_residue=10
```

`unregistered-jit-frame-on-stack=N` alone reads identically for a run held back
by a live entry-point frame and for one held back by the leftovers of a frame
that returned minutes ago, and those want opposite repairs. The census is
computed the same way under either setting of the kill switch, so it can be used
to judge the switch.

## 4. The A/B

`MvsCreate 200000`, `-Xmx256m`, default (ZGC), same binary, one arm each:

| arm | result | compaction cycles | objects relocated | `skipped_jit` |
|---|---|---:|---:|---:|
| **default (fixed)** | **`rc=0`, 36 s** | **6** | **1 031 610** | **0** |
| `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0` | `rc=1`, OOM | 1 | 4 421 | 3 |

The kill-switch arm reproduces the pre-fix counters exactly, down to
`objects_relocated=4421` and `coverage-proof-incomplete=3`. The census reads
`hits_explained_by_residue=10 hits_above_residue_mark=0` on the fixed arm and
`hits_above_residue_mark=6` on the switched-off arm — the same hits, classified
identically, refused in one arm and not the other.

`MvsCreate 500000`, `-Xmx256m` — the retired page's §3 row, where ZGC was
`rc=1` × 3 in 9-10 s against G1's and Generational's three-minute passes — is
now `rc=0` (101 s, and `rc=0` again on a repeat).

## 5. Verification

32 arms, all `rc=0`, on the fixed binary:

* Every oop-map / relocation correctness probe in `probes/`, under the default
  collector, `-XX:+UseG1GC` and `-XX:+UseSerialGC`: `OopMapPeerCoverage`,
  `OopMapHelperWindow`, `OopMapSelfCall`, `OopMapWideLocals`, `BoxUnboxReloc`,
  `BoxUnboxRelocMT`, `MtChurnProbe`, `HumongousChurn`.
* `BinTreesClassic 16` on all three collectors — the checksum oracle for the
  `main`-compiled corruption this probe family was built for.
* `MvsCreate 200000 -Xmx256m` × 3 on the default collector, plus one arm each on
  G1 and Serial.

The three-collector sweep matters because
`gc_quiescence::unregistered_jit_frame_on_stack()` is read by the generational
and G1 paths too, not only by ZGC's relocation gate.

## 6. What is deliberately NOT changed

* **The refusal for a genuinely live guardless frame stands.** A hit at or above
  the residue mark refuses exactly as before. The retired page's quotation —
  *"a slide under a compiled frame whose oops nothing can rewrite corrupts the
  heap, and fragmentation only wastes it"* — is still the rule; this change only
  stops applying it to frames that are not there.
* **`UNREGISTERED_JIT_FRAME` stays in `ZgcRealHeap::UNPINNABLE_COVERAGE_REASONS`.**
  Making it page-pinnable is a second, larger claim — it is the shape that
  corrupted the heap when `XT_HELPER_WINDOW` was lifted on 2026-09-02 — and it
  is not needed: with the residue hits reclassified, the reason no longer fires
  on this workload at all.
* **The A5 shape filter stays off.** The retired page measured it at zero
  conversions here and that measurement is unaffected — the hits it would screen
  are the same residue words, and the residue mark screens them for free.

## 7. Repro

```bash
javac -cp "$H2CP" probes/MvsCreate.java -d /tmp/p
cd $(mktemp -d)
CRATONVM_GC_STATS=1 cratonvm --java-home "$JDK" --Xmx 256m \
    -c "/tmp/p:$H2CP" MvsCreate 200000 ./data/m
```

`rc=0` in about 36 s. `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0` on the same command
is the pre-fix `rc=1` in about 9 s. `CRATONVM_DBG_A5_FALLBACK=1` prints the
per-hit classification the fix turns on.
