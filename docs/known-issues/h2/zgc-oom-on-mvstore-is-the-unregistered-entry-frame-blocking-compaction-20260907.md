# ZGC OOMs with 86% of the heap free: the compactor is refused by an unregistered entry frame — 2026-09-07

| | |
|---|---|
| **Status** | **OPEN, characterised.** Two ingredients, each with its own kill switch (§2). The obvious follow-up — the A5 frame-shape filter — is **measured and refuted here** (§4), so nobody should spend a day on it again. The fix target is named in §5 and is not attempted on this page. |
| **Scope** | `probes/MvsCreate.java` at `-Xmx256m` on **ZGC, the default collector**. G1 and Generational pass the same workload. Reproduces in **9-19 seconds**. |
| **Oracle** | G1 and Generational, same binary, same classpath: 3/3 `rc=0` each. HotSpot: `rc=0`, 0.5 s. |
| **Retires** | the ZGC row of `testmvstoretool-never-finishes-its-create-phase-on-cratonvm-20260907.md` (`OutOfMemoryError ... native reference array of length 14053`, 632 s), and answers the `TestMVStoreTool` line that `docs/internal/fixed-suite-bugs/gc/zgc-low-end-fragmentation-was-the-starved-tlab-rung-FIXED-20260830.md` recorded as "FAIL 31 s ... pre-existing, neither this". |

---

## 1. What it looks like

```text
zgc frag gauge: the arena is broken up — 88.5% of the heap is free but the
                largest single block is only 0.6% of capacity

zgc: arena allocation failed — `largest_free_block < request` with a large
     `free_list_bytes` means fragmentation, not exhaustion

zgc frag: the CHEAPEST window that could serve this request — 72 live bytes in
          3 run(s) are all that stand between 78712 free bytes spread over
          78784 bytes of contiguous [span]
zgc frag: wall occupant class=java/lang/Integer count=3 bytes=72

[GC] zgc-real: collections=11 occupancy=38429120/268435456 bytes
[GC] zgc-features: compaction_cycles=1 objects_relocated=4421
                   relocation_skipped_jit=10 relocation_on_proven_jit=1
[GC] zgc-relocation-skip-reason:     coverage-proof-incomplete=10
[GC] zgc-relocation-coverage-reason: unregistered-jit-frame-on-stack=10
[GC] zgc-frag: worst_largest_free_permille=0 free_permille_at_worst=883

Exception in thread "main" ... OutOfMemoryError: Java heap space
                                (anewarray component 516 length 8192)
```

**Three `java.lang.Integer` objects — 72 bytes — wall off a 78 KB contiguous
window, and a 32 KB array allocation fails against a heap that is 88% free.**

The failing request is always an `ArrayList.grow` doubling, so the length
doubles with the heap and the failure does not: `length 4096` at `-Xmx256m`,
`8192`, `16384` at `-Xmx512m`. It fails at 100 000, 200 000 and 500 000 entries
at all three of those heap sizes.

**It is heap-size dependent, though, and the threshold is above 512 MB.** At
`-Xmx2g` ZGC runs the whole of `org.h2.test.store.TestMVStoreTool` to `rc=0` in
848 s — the only arm of any collector that finishes that class at all (G1 times
out at 5 400 s on the same heap). So this is a defect of the low end, not a
wholesale failure of the collector: given enough headroom the fragmentation
never reaches the wall, and ZGC is then the BEST collector for this workload.
That is also why it must not be dismissed as "use a bigger heap" — 256 MB is
what the suite runs, and it is where the default collector dies in nine seconds.

## 2. The cause matrix — two ingredients

One binary, `MvsCreate 200000`, `-Xmx256m`, one arm each:

| arm | result | compaction cycles | `relocation_skipped_jit` | worst largest-free |
|---|---|---:|---:|---:|
| default | **OOM 19 s** | 1 of 11 | **10** | **0‰** |
| **`--nojit`** | **PASS 157 s** | **11 of 14** | **0** | 249‰ |
| **`CRATONVM_ZGC_TLAB=0`** | **PASS 60 s** | 1 of 8 | 7 | 246‰ |
| `CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` | OOM 14 s | 1 of 7 | 6 | 0‰ |
| `CRATONVM_ZGC_TARGETED_COMPACTION=1` | OOM 17 s | 1 of 8 | 7 | 0‰ |
| `CRATONVM_ZGC_RELOCATE=0` | OOM 10 s | 0 of 4 | 0 | 0‰ |

Two switches each remove the OOM, so there are **two ingredients**, and the rows
say which is which:

1. **The compactor is refused.** `--nojit` takes `relocation_skipped_jit` from 10
   to **0** and compaction cycles from 1 to **11 of 14** — and it passes. With
   the JIT on, relocation is declined on almost every cycle, so fragmentation is
   never repaired.
2. **The TLAB is what creates the fragmentation.** `CRATONVM_ZGC_TLAB=0` passes
   **while still being refused relocation** (`skipped_jit=7`, one compaction
   cycle) — it simply never shreds the free list: worst largest-free block 246‰
   against the default's 0‰.

Neither alone is fatal. Together, one shreds the low end and the other forbids
repairing it.

**`CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` does NOT fix this**, which is what
distinguishes it from `zgc-low-end-fragmentation-was-the-starved-tlab-rung`.
That page's defect was one rung of the refill ladder; this is the TLAB as a
whole, and that page's own table already recorded `TestMVStoreTool` as failing
on every one of its arms.

## 3. It is not the collector's own throughput

The same workload at the same heap on the other two collectors, 3 runs each:

| collector | `MvsCreate 500000`, `-Xmx256m` |
|---|---|
| G1 | `rc=0` × 3 (466 s / 237 s / 173 s) |
| Generational | `rc=0` × 3 (220 s / 203 s / 168 s) |
| **ZGC** | **`rc=1` × 3, 10 s / 9 s / 9 s** |

ZGC dies in under ten seconds where the others take three minutes. This is not
a slow collector, it is a collector that cannot allocate.

## 4. The A5 frame-shape filter would convert NOTHING here — measured

`zgc-low-end-fragmentation-...-FIXED-20260830` priced this follow-up on its own
class and found `shaped >= 1` on 12 of 13 refusing cycles, i.e. the filter would
convert exactly one, and declined to build on it. That was a measurement about
that workload, and this workload's refusal is far more expensive, so it was
re-priced here rather than assumed.

`CRATONVM_DBG_A5_CENSUS=1`, every refusing cycle of the run:

```text
[a5-census] hits=2 shaped=2 band=[0x7b0ca77b2456,0x7b0ca77c0000) band_bytes=56234
```

**Identical every time: the same 56 KB band, the same two hits, both genuinely
shaped.** The filter converts zero cycles here, and turning it on confirms it —
`CRATONVM_JIT_A5_SHAPE_FILTER=1` still OOMs at 15 s with
`unregistered-jit-frame-on-stack=7`. Same verdict as the hibernate class,
reached independently on a workload where it mattered much more.

Its COST is not measured here and the two G1 arms in the same battery must not
be read as one: 63 244 ms with the filter against 50 561 ms without, run
sequentially on a box whose load average moved between 5 and 20 during the
battery. On this host a sequential pair is not a measurement — see the
concurrent-arm method the `is_addr_in_live_region` A/B uses in
`docs/internal/performance/h2-mvstoretool-create-phase-is-mutator-side-address-validation-20260907.md`.
Since the filter converts nothing here, its cost was not worth a proper A/B.

## 5. What the band actually is, and the fix target

One fixed band, two real frames, present on **every** cycle for the life of the
process. That is the case `scan_active_jit_frames`' own comment calls canonical:

> *"A JIT method can be live WITHOUT having pushed a `JitEntryGuard`: the
> process entry point (`Vm::invoke` → compiled app `main`) is the canonical
> case — it can sit on the stack while a clinit / interpreted callee runs and
> triggers a GC."*

So the fix is **not** to weaken the refusal. `zgc-low-end-fragmentation`'s
reasoning stands and should be quoted at anyone who proposes it: *"a slide under
a compiled frame whose oops nothing can rewrite corrupts the heap, and
fragmentation only wastes it."* The fix is to make the entry frame provable —
register it, or give it precise oop maps — so the coverage proof succeeds and
the compactor is allowed to run. `relocation_on_proven_jit=1` shows the proving
path exists and fires; it just almost never can.

**Not attempted on this page.** It is a GC/JIT-integration change on exactly the
family `docs/internal/fixed-bugs/g1-eight-byte-write-at-a-live-objects-base-FIXED-20260908.md`
is about, and it needs its own lane and its own gating.

## 6. Repro

```bash
javac -cp "$H2CP" probes/MvsCreate.java -d /tmp/p
cd $(mktemp -d)
CRATONVM_GC_STATS=1 cratonvm --java-home "$JDK" --Xmx 256m \
    -c "/tmp/p:$H2CP" MvsCreate 200000 ./data/m
```

`rc=1` in about 19 s. Add `--nojit` or `CRATONVM_ZGC_TLAB=0` for the passing
arms. Measured on Azure host 2 (`20.80.105.49`), dev tip `4d7108203`, H2 2.4.249
at `apps/h2database/h2`.
