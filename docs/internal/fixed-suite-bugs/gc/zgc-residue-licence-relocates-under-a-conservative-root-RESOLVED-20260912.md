# The residue relocation licence — RESOLVED 2026-09-12

*Retired from `known-issues/gc/zgc-residue-licence-relocates-under-a-conservative-root-20260908.md`.
The page's fix — `CRATONVM_JIT_UNREG_RESIDUE_LICENCE` default-OFF — is in the
tree and unchanged. What retires the page is that **neither of the two costs it
weighs against each other reproduces any more**, and the mechanism both of them
run through is measurably inert.*

| | |
|---|---|
| **Status** | **RESOLVED.** The default flip shipped (`unreg_residue_licence_enabled`, default OFF). Re-measured 2026-09-12 on `dev@c0bebbde5`, Windows 11 / x86-64 / 8 cores: the licence no longer discriminates in either direction, and the probe it gates fires **zero** times on the page's own reproducer. |
| **What is still true** | The page's REASONING. Granting a relocation licence while a raw band word still names the object is a use-after-free, and the default it chose is the right one. Nothing here argues for flipping it back. |
| **What is no longer true** | The two NUMBERS: `8/10` vs `10/10` at `500k`/`-Xmx2g` (the corruption), and `2/3` vs `0/3` at `200k`/`-Xmx256m` (the OOM). Both are inside noise on this tree. |

---

## 1. The mechanism does not engage

`CRATONVM_JIT_UNREG_RESIDUE_LICENCE` gates exactly one decision: whether an
accepted hit from the **A5 unregistered-JIT-frame probe** raises
`mark_moving_young_coverage_incomplete_because(UNREGISTERED_JIT_FRAME)`. No hit,
no decision, and the flag is a no-op whatever it is set to.

Measured on the page's own workload, `probes/MvsCreate.java` at `-Xmx256m`,
`dev@c0bebbde5`:

```text
[a5-engagement] calls=9777 memo_clean=0 memo_banded=0 full_rescan=9777 words=7777
[a5-fallback]   TOTALS hits=0 residue=0 shapeless=0 filtered=0
                        residue_filter=true shape_filter=false
```

and over the same run, on every collection without exception:

```text
[jitroots] … incomplete=false reason=none …
```

Read the three lines together, because each on its own could be a dead counter:
the probe RAN 9777 times, it SCANNED 7777 band words, and it found **no word in
the band that is a return address into JIT code**. That is engagement with a
real zero, not an unbumped counter. `[bandpath] a5_sweeps=0 a5_roots=0` says the
same thing from the other side — no A5 sweep contributed a single root.

The band is `[max(scanner_sp, cover_hi), current_thread_stack_high())`. It is
empty or near-empty on this host because the JIT entry chain's `cover_hi`
reaches the top of the stack: 7777 words over 9777 calls is an average band of
under one word.

## 2. The corruption arm does not reproduce

`MvsCreate 500000` at `-Xmx2g` on ZGC, one binary, **eight concurrent pairs**
(the page's own protocol — this host's load swings wider than the effect, so
arms run side by side rather than in sequence):

| `dev@c0bebbde5` | `rc=0` | faces |
|---|---:|---|
| default (licence withheld) | **7/8** | `Chunk 11 not found` ×1 |
| `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=1` (granted) | **6/8** | `Chunk 11/12 not found` ×2 |

Fisher's exact p ≈ 1.0. The page measured `8/10` granted against `10/10`
withheld; this is `6/8` against `7/8`, with the *withheld* arm — the safe
default — carrying a failure of its own in the very first pair.

**The unambiguous face never appeared.** The page's argument rests on
`ClassCastException: class [B cannot be cast to class [J` — one address carrying
two array headers, which no H2 race can produce. Over sixteen runs at `500k`,
and nine more at `200k`, it occurred **zero times**.

### What the surviving face is

`MVStoreException: Chunk N not found` is already root-caused, in this tree, as
an H2-level chunk-reclaim race: a chunk's `pinCount` reaching zero makes it
reclaim-eligible with no regard for an in-progress multi-page traversal. See
`known-issues/h2/not-bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md`,
whose HotSpot control reproduced the same exception on stock HotSpot 25 under
load.

The control here is consistent with that and is worth stating precisely, because
it is NOT a clean exoneration: stock HotSpot 25, same classpath, same workload,
same host, ran `MvsCreate 500000 -Xmx2g` **3/3 clean** — but it finishes the
`put` phase in seconds where CratonVM takes ~160 s, so it never opens the
window. "HotSpot is clean at 30x the speed" is evidence about the race's
reachability, not about its cause.

## 3. The OOM arm does not reproduce either

This is the arm that mattered, because it is the COST the page knowingly accepted
when it withheld the licence: *"withholding the licence puts the ZGC OOM back,
exactly as `115f9f00b` says it does"*, scored `2/3` granted against `0/3`
withheld (`OutOfMemoryError` ×3).

`MvsCreate 200000` at `-Xmx256m`, nine concurrent pairs on one binary:

| `dev@c0bebbde5` | `rc=0` |
|---|---:|
| default (licence withheld) | **8/9** — one `OutOfMemoryError` |
| granted | **9/9** |

One OOM in nine, against the page's three in three. With the A5 probe reporting
`hits=0` the flag cannot be what moved it, so the single OOM is the workload's
own headroom at `-Xmx256m` and not a licence effect.

**So the page's own trade — "a loud OOM beats silent corruption" — has nothing
left on either side of it.** That is why this retires rather than being
re-argued.

## 4. What §5's repair was for, and why it is not being built

The page's repair is *"give the shallow band above `cover_hi` precise roots.
Then it needs no conservative scan, so it needs no pin, so the licence has
nothing to grant and the OOM goes with it."*

That repair targets the A5 span sweep. On this tree the sweep contributes
`a5_roots=0` on both of the workloads that motivated it (`MvsCreate`, and H2
`TestValueMemory` — see the companion page), so there is no measurement that
could accept or reject a change to it. Building it would be a change nobody can
score, which is the shape this project has been burned by often enough to have
written it down.

It is left unbuilt **deliberately**, and the instrument that decides when to
build it is in the tree and free when unset:

```bash
CRATONVM_DBG_A5_ENGAGEMENT=1   # [a5-engagement] calls / words the probe scanned
CRATONVM_DBG_A5_FALLBACK=1     # [a5-fallback]  hits, and how each was classified
CRATONVM_DBG_JIT_ROOTSCAN=1    # [bandpath]     a5_sweeps / a5_roots per pass
```

A workload with `a5_roots` in the thousands is the one that re-opens this. Until
then `CRATONVM_JIT_A5_FRAME_SCAN` and `CRATONVM_JIT_A5_MARK_SPAN` stay as the
pricing levers their doc comments say they are.

## 5. Scope of this retirement

Measured on **one host** (Windows 11, x86-64, 8 logical cores) at **one
revision** (`dev@c0bebbde5`) with **H2 2.4.240**. The page's own numbers were
taken on a different host with 2.4.249. A reader who sees `a5_roots` non-zero,
or the `[B`/`[J` ClassCastException, on any host is looking at something this
retirement did not measure and should re-open it — the flag, the levers and the
censuses are all still in the tree exactly as the page left them.

## 6. Repro, unchanged

```bash
H2J=~/.m2/repository/com/h2database/h2/2.4.240/h2-2.4.240.jar
javac -cp "$H2J" -d /tmp/p probes/MvsCreate.java
cd "$(mktemp -d)"
cratonvm --java-home "$JDK" --Xmx 2g -c "/tmp/p:$H2J" MvsCreate 500000 ./data/m
```

Concurrent pairs against `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=1`, separate
working directories per arm, and eight pairs minimum — at the rate this page
originally reported, three pairs is not a result.
