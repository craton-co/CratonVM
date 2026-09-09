# The moving evacuator is handed a live root and refuses to copy it, and a refusal is indistinguishable from success

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-09. Deterministic. Root-caused to a named refusal path; the reason the bitmap is missing the object is not yet settled. |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. Passes at `>= 524288`. |
| **Collector** | the **MOVING** (Cheney) young cycle. `CRATONVM_DBG_FORCE_MOVING=1` does not cure it, and neither does `--nojit`. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64, ~6 s (SIGSEGV) or ~20 s with `CRATONVM_GC_RESERVE=0` |
| **Found while** | fixing [the `URL.openConnection` stale-receiver defect](../../internal/springboot/bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md). That one crashed at moving cycle 1203; with it fixed the workload reaches cycle 7098 and hits this. |

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
(`site=unbumped-middle`) before any guard can report, and the log says nothing.
With it the same read returns zeroes and every instrument below fires.

## The chain, one measured link at a time

Everything here is from one run, in order.

**1. A live frame slot names an object the cycle did not copy.**

```text
POST-GC RECLAIMED-WHILE-HELD LOCAL: frame[34]
  org/junit/jupiter/engine/descriptor/DisplayNameUtils.determineDisplayNameForMethod
  local[0] pc=11 holds 0x200868400d8, inside the semispace this cycle emptied, and the
  pointer map has no entry for it -- the object was NOT copied, so this slot was not in
  the root set. local_kind=0 in_heap=true collection=2398
```

`local_kind=0` rules out the `local_kinds` LONG/DOUBLE gate; `in_heap=true`
rules out the `is_heap_addr` screen. Those are `scan_local_objects`' only two
ways to drop a live object local, and the arm is already live-mask filtered, so
the per-bci liveness filter is ruled out too.

**2. The scan produces it, and the collector was given it.**

```text
POST-GC RECLAIMED-WHILE-HELD LOCAL ^ in_root_set=Some(true)
POST-GC RECLAIMED-WHILE-HELD LOCAL ^ scan_would_root=true (frame's own scan yields 1 roots)
```

`in_root_set` is membership in the address set `collect_roots` actually returned
for this collection. So this is not a scanning fault.

**3. The evacuator refused it.**

```text
[forward-refused] not-an-object-start old_ptr=0x200868400d8 off=0x400d8
  arena=[0x20086800000 cap=0x20000000 used=0x40320] bitmap=[0x20086800000 span=0x40320]
  off_beyond_bitmap=false in_free_block=false
```

`forward_object_impl`'s first gate — `young_object_starts.contains(old_ptr)` —
said no. The address is inside from-space, inside the bitmap's extent, and not
inside a free block, so none of the three reasons that gate exists for applies.

**A refusal returns `old_ptr` unchanged, and `seed_roots` writes that straight
back into the root slot.** On the moving path that is byte-for-byte
indistinguishable from a successful no-op: no pointer-map entry, no diagnostic,
and from-space is reset a moment later. That property is the reason this defect
took a chain of five instruments to reach rather than one.

**4. The object-start walk recorded fifteen objects in 262 944 bytes.**

```text
[forward-refused] ^ objstart_walk: used=0x40320 starts_recorded=15 parallel=false
  chunks=0 complete=true skips=0 skip_bytes=0x0 first_skips=[] reserved_tails=false
[forward-refused] ^ header_at_old_ptr: class_id=0 kind_tag=0 elem_tag=0
  nearest_recorded_start=0x20086800400 delta=261336
```

The walk completed, had no skip list to jump over, and ran sequentially — and it
recorded **fifteen** starts across the whole from-space. The nearest recorded
start below the refused address is 261 336 bytes below it. So the walk strode
from offset `0x400` to the end of the space in one step: it read a size of about
256 KiB for the object at `0x400` and walked over everything between.

The header at the refused address is already all-zero at this point, before
from-space is reset.

## What that leaves, and it is exactly two possibilities

* **The object at offset `0x400` really is ~256 KiB**, the walk is right, and
  `0x200868400d8` is an INTERIOR word of it. Then the reference in `local[0]` is
  wrong and the refusal is correct — and the defect is whatever put an interior
  address into a Java local. A Java local cannot hold an interior pointer under
  bytecode semantics, so this would itself be a serious upstream defect.
* **The walk mis-sized the object at `0x400`** and strode over live objects,
  which loses every one of them.

The next measurement separates them in one line: print the header at
`nearest_recorded_start` — its class, kind and computed
`gen_object_total_size` — beside the refusal. If it is a 256 KiB array the first
branch holds; if it is a small object with a corrupt size the second does.

## Ruled out

* **Not the non-moving sweep.** `CRATONVM_DBG_FORCE_MOVING=1` gives
  `moving=7130 non_moving=0` and fails identically.
* **Not the JIT.** `--nojit` fails too (at a different point,
  `NoSuchMethodError java/lang/String.size()` from `EngineFilterer.isExcluded` —
  the same family with a different victim).
* **Not the parallel object-start walk.** `CRATONVM_GC_PAR_THREADS=1` fails, and
  the census reports `parallel=false` on the failing cycle anyway.
* **Not the per-bci local-liveness filter.** `CRATONVM_NO_LOCAL_LIVENESS=1`
  reproduces, and the verifier arm is live-mask filtered.
* **Not the free list.** `skips=0 skip_bytes=0x0` on the failing cycle, and
  `in_free_block=false` for the address.
* **Not an un-retired TLAB tail.** `reserved_tails=false`.
* **Not the young uncommit.** `CRATONVM_GC_RESERVE=0` changes the symptom from a
  SIGSEGV to a clean report, not the outcome.
* **Not the stale-reference family the sibling page describes.** The exact
  vacated ledger — now correct on this collector — reports zero hits for this
  address: it was never moved, it was never copied.
* **Not the JIT spill-region conservative scan.**
  `CRATONVM_DBG_ROOT_REMAP_AUDIT` does report unremapped roots attributed to
  scan section 14 on other cycles, but with `--nojit` there is no such section
  and the failure persists.

## A second, separate finding from the same run

`CRATONVM_GC_VERIFY_RSET=1` reports a missing old→young card on ~1250 of 3594
moving cycles, and it is the same referrer every time:

```text
[rset-verify] first MISSING old->young edge: referrer=0x200424136a0 class_id=424 slot=0
```

`class_id=424` is `java/lang/Module` (`CRATONVM_DBG_LAYOUT=1`:
`[layout] java/lang/Module cid=424 body=72 refs=8 fields=9`), and the heap-stale
walk names the same pair independently: `[heap-stale] ZEROED(reclaimed) OBJ
java/lang/Module field[0]`, 2272 reports in one run plus 8 `UN-FORWARDED`.

It is **not** this defect: `CRATONVM_GC_VERIFY_RSET=1` (which seeds the missing
edges it finds) and `CRATONVM_GC_FULL_RSET_SCAN=1` both still fail, with the
same `Supplier`. But a persistent old→young edge that the card table never
delivers is a real defect on its own and should be filed and fixed separately.

## Instruments this page depends on

All default-off, all added 2026-09-09, all with the reasoning at the site:

| instrument | flag | what it answers |
|---|---|---|
| `POST-GC RECLAIMED-WHILE-HELD` | *(always on when a moving cycle runs)* | a live frame slot naming the emptied semispace with no pointer-map entry |
| `in_root_set` | `CRATONVM_DBG_ROOT_REMAP_AUDIT` | was the address in the list the collector was handed |
| `scan_would_root` | *(with the above)* | would the frame's own scan produce it |
| `[forward-refused]` | `CRATONVM_DBG_ROOT_REMAP_AUDIT` | which of the evacuator's three refusal paths declined it, with the arena geometry |
| `objstart_walk` summary | *(with the above)* | what the walk that built the bitmap actually did |

Before them, this defect presented as `Stale pointer detected in invokevirtual
receiver … falling back to CP class java/util/function/Supplier` and nothing
else — no statement anywhere of which collection dropped it, or why.
