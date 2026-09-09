# A Java local reaches `invokevirtual` holding an interior word of a retired TLAB's tail filler

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-09. Deterministic. Narrowed to one sentence; the producer is not named. |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. Passes at `>= 524288`. |
| **Collector** | the **MOVING** (Cheney) young cycle. `CRATONVM_DBG_FORCE_MOVING=1` does not cure it; neither does `--nojit`. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64, ~6 s (SIGSEGV) or ~20 s with `CRATONVM_GC_RESERVE=0` |
| **Family** | The same one as [the `URL.openConnection` defect](../../internal/springboot/bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md): a stale reference reaching bytecode. That one crashed at moving cycle 1203; with it fixed the workload reaches cycle 7098 and hits this. |

## The finding, in one line

`org/junit/jupiter/engine/descriptor/DisplayNameUtils.determineDisplayNameForMethod`
runs with `local[0]` holding `0x200868400d8`, which is **216 bytes into a
retired TLAB's tail filler** — a span that is dead by construction and never
held an object base. Every collector-side component that touches it behaves
correctly; the reference itself is wrong.

## Repro

```bash
CRATONVM_DBG_GC_STRESS=262144 \
CRATONVM_GC_STATS=1 \
CRATONVM_GC_RESERVE=0 \
CRATONVM_DBG_ROOT_REMAP_AUDIT=1 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

**`CRATONVM_GC_RESERVE=0` is load-bearing for diagnosis, not for the defect.**
Without it the process takes a SIGSEGV reading a decommitted granule
(`site=unbumped-middle`) before any guard can report, and the log says nothing
at all.

## The chain, one measured link at a time

All of it is from one run, in order. Each line is a different instrument; the
last one is what settles it.

**1. A live frame slot names an object the cycle did not copy.**

```text
POST-GC RECLAIMED-WHILE-HELD LOCAL: frame[34]
  org/junit/jupiter/engine/descriptor/DisplayNameUtils.determineDisplayNameForMethod
  local[0] pc=11 holds 0x200868400d8, inside the semispace this cycle emptied, and the
  pointer map has no entry for it. local_kind=0 in_heap=true collection=2398
```

`local_kind=0` rules out the `local_kinds` LONG/DOUBLE gate, `in_heap=true`
rules out the `is_heap_addr` screen, and the arm is live-mask filtered so the
per-bci liveness filter is out. Those are `scan_local_objects`' only three ways
to drop a live object local.

**2. The scan produces it, and the collector was given it.**

```text
^ in_root_set=Some(true)
^ scan_would_root=true (frame's own scan yields 1 roots)
```

**3. The evacuator refused it.**

```text
[forward-refused] not-an-object-start old_ptr=0x200868400d8 off=0x400d8
  arena=[0x20086800000 cap=0x20000000 used=0x40320] bitmap=[0x20086800000 span=0x40320]
  off_beyond_bitmap=false in_free_block=false
```

`forward_object_impl`'s first gate — `young_object_starts.contains` — said no,
and the address is inside from-space, inside the bitmap's extent, and not inside
a free block.

**4. The object-start walk recorded fifteen starts in 262 944 bytes, and it was right to.**

```text
^ objstart_walk: used=0x40320 starts_recorded=15 parallel=false chunks=0
  complete=true skips=0 skip_bytes=0x0 reserved_tails=false
^ header_at_old_ptr: class_id=0 kind_tag=0 elem_tag=0
  nearest_recorded_start=0x20086800400 delta=261336
^ object_at_nearest_start: addr=0x20086800400 class_id=4044482304 kind=Array
  num_slots=65444 array_length=65444 stride=0x3fea0 covers_refused=true
```

`4044482304` is `0xF111_E700` — `tlab::TLAB_FILLER_CLASS_ID`, the synthetic
class id stamped into the unused tail of a retired TLAB. Its own doc says the
layout is "a perfectly valid `int[]` so even walkers that do not recognise the
sentinel will skip past it correctly using the normal array-size formula", and
that is exactly what the walk did: one stride of `0x3fea0` over 256 KiB of
**dead** bytes.

So the walk is right, the bitmap is right, and the evacuator's refusal is right.
`0x200868400d8` is an interior word of a dead TLAB tail. It is not an object
base, and it was never one in this from-space.

## What that leaves

**A Java local held an address that no live object occupies.** Under bytecode
semantics a local cannot hold an interior pointer, so the value was already
wrong when it was written — the same shape as the `URL.openConnection` defect,
which stored a receiver's pre-GC address into a live object's field and had it
handed back later.

The two candidate producers, in the order they are cheap to test:

1. **A native holding a raw `ObjectRef` across an allocation**, like
   `URL.openConnection` and `ClassFileDumper.getInstance` did. The per-call-site
   census on the stale-reference barrier
   (`load_and_forward`, under `CRATONVM_DBG_VACATED_FRAMES`) reports **zero**
   sites on this run — but that barrier only sees a reference the ledger still
   holds, and the ledger drops an address the moment the allocator re-issues it.
   A reference that goes stale and is not USED until after re-issue is invisible
   to it. Closing that window needs the barrier consulted at the WRITE rather
   than at the next boundary crossing.
2. **A TLAB whose chunk was handed out twice**, so objects really were allocated
   inside what a later retire filled as an unused tail. The T-3 tripwire
   (`moving_with_reserved_tails`) covers the case where a live mutator still
   owns a TLAB in the arena being reset, and it reports `reserved_tails=false`
   here — so if this is the mechanism it is a different one.

The measurement that separates them: record, per TLAB refill, the `[start, end)`
chunk and the retire that filled it, and check whether the refused address falls
inside a chunk that was live at the moment its filler was stamped.

## Ruled out

* **Not the non-moving sweep.** `CRATONVM_DBG_FORCE_MOVING=1` gives
  `moving=7130 non_moving=0` and fails identically.
* **Not the JIT.** `--nojit` fails too, at a different point
  (`NoSuchMethodError java/lang/String.size()` from
  `EngineFilterer.isExcluded`) — the same family with a different victim.
* **Not the parallel object-start walk.** `CRATONVM_GC_PAR_THREADS=1` fails, and
  the census reports `parallel=false` on the failing cycle anyway.
* **Not the per-bci local-liveness filter.** `CRATONVM_NO_LOCAL_LIVENESS=1`
  reproduces, and the verifier arm is live-mask filtered.
* **Not the free list, and not an un-retired TLAB tail at collection time.**
  `skips=0 skip_bytes=0x0 reserved_tails=false` on the failing cycle,
  `in_free_block=false` for the address.
* **Not the young uncommit.** `CRATONVM_GC_RESERVE=0` changes the symptom from a
  SIGSEGV to a clean report, not the outcome.
* **Not a missed old→young card**, though there is one — see below.
* **Not the object-start walk or the evacuator**, per §4 above. Both are correct
  on this input.

## A second, separate defect from the same run

`CRATONVM_GC_VERIFY_RSET=1` reports a missing old→young card on ~1250 of 3594
moving cycles, always the same referrer:

```text
[rset-verify] first MISSING old->young edge: referrer=0x200424136a0 class_id=424 slot=0
```

`class_id=424` is `java/lang/Module` (`CRATONVM_DBG_LAYOUT=1`:
`[layout] java/lang/Module cid=424 body=72 refs=8 fields=9`), and the heap-stale
walk names the same pair independently — `[heap-stale] ZEROED(reclaimed) OBJ
java/lang/Module field[0]`, 2272 reports in one run plus 8 `UN-FORWARDED`.

It is **not** this defect: `CRATONVM_GC_VERIFY_RSET=1` seeds the missing edges it
finds and `CRATONVM_GC_FULL_RSET_SCAN=1` scans without the card table, and both
still fail with the same `Supplier`. But a persistent old→young edge the card
table never delivers is a real defect and wants its own page.

## Instruments this page depends on

All default-off, all added 2026-09-09, all with the reasoning at the site:

| instrument | flag | what it answers |
|---|---|---|
| `POST-GC RECLAIMED-WHILE-HELD` | *(on for every moving cycle)* | a live frame slot naming the emptied semispace with no pointer-map entry |
| `in_root_set` | `CRATONVM_DBG_ROOT_REMAP_AUDIT` | was the address in the list the collector was handed |
| `scan_would_root` | *(with the above)* | would the frame's own scan produce it |
| `[forward-refused]` | `CRATONVM_DBG_ROOT_REMAP_AUDIT` | which of the evacuator's three refusal paths declined it, with the arena geometry |
| `objstart_walk` summary | *(with the above)* | what the walk that built the bitmap actually did |
| `object_at_nearest_start` | *(with the above)* | the object the walk strode over the address with — the line that settled this page |

Before them this defect presented as `Stale pointer detected in invokevirtual
receiver … falling back to CP class java/util/function/Supplier`, and nothing
else: no statement of which collection dropped it, or whether anything had.

**One gap worth closing:** `POST-GC RECLAIMED-WHILE-HELD` cannot yet tell
"reclaimed while a frame held it" from "the frame held an address that was never
an object". Both print identically, and only the `[forward-refused]` chain
separates them. The verifier should consult the same object-start bitmap and say
which it is.
