# The non-moving sweep's missed root is gone — the page's configuration passes, and so does every lever in its table

| | |
|---|---|
| **Status** | **RETIRED**, 2026-09-09. Does not reproduce on `dev` @ `0af72de1b`, at this page's own configuration or at any lever it lists. |
| **Was** | `docs/known-issues/springboot/bindabletests-bytebuddy-receiver-reclaimed-under-gc-stress-20260908.md` |
| **Fixed by** | Work that landed in `dev` between this page's tree and `0af72de1b` — **not** by anything in the branch that retired it, and **not** by either repair this page itself made. Evidence below. |
| **Kept** | Both root-coverage repairs, both instrument corrections and the three fixed instruments this page produced. They are in `dev` and are unaffected. |

## What the page reported

`org.springframework.boot.context.properties.bind.BindableTests`, 26/27, ~110 s,
under `--XX:UseGc Generational`, JIT on, `CRATONVM_DBG_GC_STRESS=4194304`. The
**non-moving** young sweep reclaimed a live
`net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType`,
surfacing as `Object.asGenericType()` — the all-zero header of a reclaimed span
resolving to `ClassId(0)`. `moving=2 non_moving=6274`, reason
`nonmoving-conservative-jit-roots`.

## What the same commands do now

Linux x86-64, `dev` @ `0af72de1b`, one class per process, `-Parallel 1`,
`--XX:UseGc Generational`, JIT on.

| lever | this page's result | now |
|---|---|---|
| *(none)*, `GC_STRESS=4194304` | **FAIL 26/27**, `moving=2 non_moving=6274` | **PASS 27/27**, three reps, `moving=4 non_moving=0` |
| `CRATONVM_DBG_FORCE_MOVING=1` | PASS | PASS 27/27 |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | PASS | PASS 27/27 |
| `CRATONVM_NO_PRECISE_JIT_MAPS=1` | FAIL 26/27 | **PASS 27/27** |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` | FAIL 26/27, `non_moving=5184` | **PASS 27/27**, `non_moving=3484–6286` |
| `CRATONVM_JIT_ABOVE_CHAIN_SCAN=1` | FAIL 26/27 | **PASS 27/27** |
| `…ABOVE_CHAIN_SCAN=1` + `…ALL_PATHS=1` | *(the page's next experiment)* | **PASS 27/27** |

Two things in that table matter more than the pass marks.

**The sweep no longer engages at all on this workload.** The page's own
configuration now reports `moving=4 non_moving=0
moving-no-jit-frames-live=4` where it reported `moving=2 non_moving=6274`. No
JIT frame is live at any collection, so the collector never selects the
non-moving path, and the 110-second run time the page recorded — which was the
sweep thrashing, not the workload — is now 2.4 s.

**Forced onto the sweep, the root is no longer missed.**
`CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` is the page's own honest control: it holds
the collector on the sweep without touching whether JIT frames exist. It
produces 3 484–6 286 non-moving cycles and passes 27/27.

## Neither of this page's own repairs is what carries it

This is the check that keeps the retirement honest, because "it passes now" and
"our fix works" are different claims.

`conservative_locals_compiled_in()` gates BOTH halves of the page's first repair
— step 1's conservative locals probe and the step-14a5 pass — and
`CRATONVM_NO_CONSERVATIVE_LOCALS=1` turns it off outright. On the forced-sweep
configuration:

| | cycles | `a5_frame_pass` | result |
|---|---:|---|---|
| conservative pass ON | `non_moving=3484` | `cycles=3328 roots=2025142` | PASS 27/27 |
| `CRATONVM_NO_CONSERVATIVE_LOCALS=1` | `non_moving=3632` | `cycles=0 roots=0` | **PASS 27/27** |

With the repair engaged on 3 328 of 3 484 sweeps it passes; with the repair
compiled out entirely and no conservative probing anywhere it passes just the
same. So the root the page was chasing is not being recovered by the widened
probe — it is not missing any more.

That also settles the page's open question ("why does a scan that runs at every
native call find a root that the same scan, run at collection time, does not?").
It no longer has a subject: the collection-time pass finds everything the
fullstack diagnostic did.

## What did fix it

Not this branch: the table above was measured on a `dev` binary built from
`0af72de1b` with no change from the retiring branch in it.

The candidates all landed in `dev` after this page's tree, and all three are
about compiled frames publishing roots the collector can see:

* `c798aae82` — *the optimizing tier spliced callee bodies and gave their frames to nobody*
* `2e11a27a5` — *array loads and primitive array stores published no exceptional frame*
* `020bf001f` — *the vacated-frame auditor could not see a compiled frame*

with `5dd6bfee7` (*the discharge relocates on a pin its predicate could not
complete*) and `6c9933883` (*the cross-thread peer stack walk dereferenced a
`/proc/self/maps` snapshot*) alongside them. The first of those is the one whose
shape matches the symptom exactly — an inlined callee frame that no root map
described is precisely a compiled frame holding a reference the sweep cannot
see. This page does not bisect to a single commit: doing so needs a tree on
which the failure still reproduces, and there is no longer one.

## What this page produced, all of it still in `dev`

The repairs and instrument corrections stand on their own and are unaffected by
the retirement:

1. **The A5 cycle ran the sweep without the pass that makes it safe** — step
   14a5, placed after the JIT scan where the A5 flag is authoritative, with two
   unit tests in `vm/src/memory/roots.rs` pinning both halves of the truth
   table.
2. **The third non-moving predicate was dead** — `conditional_loader_metadata`'s
   `unregistered_jit_frame_on_stack()` term could never be true at its call
   site; removed, and `young_marker_follows_side_tables`' doc is true again.
3. **The off-grid sweep-anchor counter reported the walk's own start** —
   `gen_heap::AnchorGridProbe`, four unit tests.
4. **The local-liveness ledger had no age** — entries carry the collection they
   were made on, and the report prints `collections_since`.
5. **An orphaned `#[cfg]`** — `dbg_fullstack_scan`'s attribute had attached
   itself to `UNREG_MEMO_SUPPRESSED`, cfg-gating a counter `vm-cli` reads
   unconditionally.

## The sibling page, and the residual

The page filed alongside this one —
[`bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md`](bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md)
— was the MOVING collector, and it is fixed rather than retired: `URL.openConnection`
stored its receiver's pre-GC address in the carrier's `url` field.

A third defect, reachable only once that one was fixed, keeps
`CRATONVM_DBG_GC_STRESS <= 262144` failing and is filed as
[`bindabletests-moving-evacuator-refuses-a-root-it-was-given-20260909.md`](../../known-issues/springboot/bindabletests-moving-evacuator-refuses-a-root-it-was-given-20260909.md).
It is the moving collector, not the sweep, and it is not what this page
described.

## Re-check command

```bash
CRATONVM_DBG_GC_STRESS=4194304 \
CRATONVM_GC_STATS=1 \
CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

The lever is the point: without it the collector takes the moving path on this
workload and the sweep is never exercised, so a bare pass proves nothing about
the defect this page described. `CRATONVM_GC_VERIFY_RSET` is still not needed
and still costs a full old-generation walk per collection, and `--nojit` is
still the dishonest control for the reason the page gave.
