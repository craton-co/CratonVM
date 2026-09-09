# The residue relocation licence relocates out from under a conservative root — 2026-09-08

| | |
|---|---|
| **Status** | **OPEN.** `CRATONVM_JIT_UNREG_RESIDUE_LICENCE` shipped **default-ON** in `115f9f00b` and corrupts the heap: `8/10` against a `10/10` control on one binary. This page flips it to **opt-in** and keeps the ZGC OOM it was built to fix, because a loud OOM beats silent corruption. The licence's *mechanism* is right and its OOM fix is real — only the default is wrong. |
| **Scope** | ZGC (the default collector), any workload that keeps live references in the shallow band above `cover_hi`. Reproduced on `probes/MvsCreate.java` at `-Xmx2g`, and still present after `12b8a05aa`. |
| **Control** | the same binary with `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0`: 43 of 43 clean. |
| **Supersedes the "FIXED" claim in** | `../../internal/fixed-suite-bugs/gc/zgc-oom-on-mvstore-was-returned-frame-residue-FIXED-20260908.md` — its cause analysis stands, its §5 verification has one gap, named in §3 below. |

---

## 1. What it looks like

Two faces, both on the shipped default, both absent from the control:

```text
org/h2/mvstore/MVStoreException: Chunk 13 not found [2.4.249/9]
  MVMap.remove -> MVMap.operate -> CursorPos.traverseDown
  -> Page$NonLeaf.getChildPage -> MVMap.readPage -> MVStore.readPage
  -> FileStore.readPage -> FileStore.getChunk
```

```text
java/lang/ClassCastException: class [B cannot be cast to class [J
  MVStore.commit -> MVStore.store -> FileStore.dropUnusedChunks
  -> FileStore.cleanToCCache
```

The second one is the whole argument. `[B` and `[J` are `byte[]` and `long[]`:
one address carrying two different array headers is not an H2 defect, not an I/O
defect and not a flake. It is a reference that was followed after the object it
named had moved.

## 2. The measurement

`MvsCreate 500000` at `-Xmx2g` on ZGC — a heap where **both arms complete**, so
the control is a control and not an OOM. One binary, dev@`d7768380b`, arms run
as CONCURRENT PAIRS because this host's load swings far wider than the effect.

| dev@d7768380b | `rc=0` |
|---|---:|
| default (licence granted) | **8/10** |
| `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0` | **10/10** |

An independent implementation of the same idea — decline the refusal when the
hit is explained by residue — was measured the same way before this page's
author found `115f9f00b` on `dev`, and agrees:

| variant, `500k` @ `2g` | granted | withheld |
|---|---:|---:|
| dev@d7768380b, 10 pairs | 8/10 | 10/10 |
| this branch pre-merge, 8 pairs | 7/8 | 8/8 |
| **this branch merged onto dev, 25 pairs** | **20/25** | **25/25** |
| mark **and** pin both dropped, 33 runs/arm | 27/33 | 33/33 |
| **pooled** | **62/76** | **76/76** |

Seventy-six clean control runs against fourteen failures is what turns "a known
MVStore flake" into a signal.

**The merged-tree row nearly went the other way, and how it did is the point.**
`12b8a05aa gc,vm: the blocked-region wake never remapped compiled state` landed
on `dev` between the first measurement and this one — a stale-compiled-state fix,
exactly the family this failure belongs to — so the merged tree was re-measured
rather than assumed. Its **first ten pairs were 10/10 clean**, which at the
observed ~20% rate happens about one time in nine and reads exactly like "the
sibling fixed it". The next fifteen pairs were **10/15**, with `Chunk 6`,
`Chunk 6`, `Chunk 15`, `Chunk 6`, `Chunk 15`. Ten clean runs is not an absence
proof for a one-in-five defect; budget the sample from the rate before reading
a clean batch as a fix.

### The ratchet, both directions, on the shipped binary

A default flip has to be shown to move both ways, so both are scored — one
binary, concurrent pairs:

| | granted | withheld (the new default) |
|---|---:|---:|
| **A.** `200000` @ `-Xmx256m` — the OOM the licence fixes | 2/3 | **0/3**, `OutOfMemoryError` ×3 |
| **B.** `500000` @ `-Xmx2g` — the corruption | 7/8 | **8/8** |

Arm A is the honest cost of this page: withholding the licence puts the ZGC OOM
back, exactly as `115f9f00b` says it does. Note also that arm A's granted column
is 2/3 — the third run died `Chunk 9 not found` — so the corruption is not
confined to large heaps; it is simply easier to see where the control survives.

## 3. Why the fix's own safety argument does not reach it

`unreg_residue_licence_enabled`'s doc comment reasons, correctly, that:

> Marking is unaffected either way: the full band is conservatively scanned on
> every accepted hit under both settings, so this switch can only change how
> often the collector is ALLOWED TO COMPACT, never what it RETAINS.

Retention really is unaffected. **Retention is not the failure.** The accepted
hit used to do two things:

```rust
scan_one_frame(search_lo, high, heap, out);   // MARK the band
mark_moving_young_coverage_incomplete_because(UNREGISTERED_JIT_FRAME);  // PIN it
```

and the second one is there because a conservative root cannot be rewritten — a
stack word that merely *looks* like an address must not be updated. Granting the
licence keeps the mark and drops the pin, so the object survives the cycle and
**moves**, while the raw word in the band still holds where it used to be. The
object is retained; the pointer to it is stale. That is `[B` read as `[J`.

The verification in the retiring page is 32 green arms, and the gap is narrow
and specific: **every `MvsCreate` arm it runs is at `-Xmx256m`, where the
control OOMs.** A ratchet measured only at the size the bug lives at never meets
its own control, so a regression that exists only where the control survives
cannot appear. The `-Xmx2g` arm is the one that was missing.

## 4. What is NOT wrong with the fix

* **The cause analysis is right.** The hits really are the leftover return
  address of `MVMap.isPersistent()Z`, a one-line getter that had already
  returned; there was no entry frame to register. The retired page's §5 guess
  was wrong and `115f9f00b` corrected it.
* **The `live_hit` re-probe is right.** Deciding from the first hit while acting
  on the whole 56 KB band would drop live shallow frames; re-probing
  `[max(residue_hi, search_lo), scan_hi)` is the correct question. Measured
  independently here, it is also *inert* on this workload — zero hits above the
  residue mark over a full run — which is why the licence is granted on every
  cycle and the exposure is continuous rather than occasional.
* **The OOM is real and comes back.** Withholding the licence restores it:
  `MvsCreate 200000 -Xmx256m` is `0/3` (`OutOfMemoryError` ×3) withheld against
  `3/3 rc=0` granted. This page is not claiming the OOM is acceptable — only
  that it is the better of the two defaults while both are on the table.

## 5. The repair

Give the shallow band above `cover_hi` precise roots. Then it needs no
conservative scan, so it needs no pin, so the licence has nothing to grant and
the OOM goes with it. Until then the pin stays and `=1` is the escape hatch for
anyone who would rather have the compaction than the safety.

Do **not** reach for `UNPINNABLE_COVERAGE_REASONS` — `115f9f00b` §6 is right
that making `UNREGISTERED_JIT_FRAME` page-pinnable is a larger claim, and it is
the shape that corrupted the heap when `XT_HELPER_WINDOW` was lifted on
2026-09-02.

## 6. Repro

```bash
javac -cp "$H2CP" probes/MvsCreate.java -d /tmp/p
cd $(mktemp -d)
cratonvm --java-home "$JDK" --Xmx 2g -c "/tmp/p:$H2CP" MvsCreate 500000 ./data/m
```

About one run in five fails on a granted licence; run the arms as concurrent
pairs against `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=1` rather than sequentially,
and give it ten pairs — at one-in-five, three pairs is not a result.
