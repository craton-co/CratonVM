# `DefaultCatalogAndSchemaTest` is intermittently unstable in every collector arm

| | |
|---|---|
| **Status** | 🔴 OPEN — pre-existing, present on `origin/dev` @ `cc8167f94` with no local changes. 2026-08-03: `OldGen::compact` (the leading coalescer-adjacent suspect below) is RULED OUT — see the dated section near the end. The crash signature (`num_slots=33554433`/`0x02000001`) is confirmed identical to the one in [`map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md) (the `--nojit` sibling, now resolved for its own narrower scope by disabling `compact()` — which does **not** fix this doc's crash, proving the corruption is shared and upstream of both old-gen reclamation strategies). |
| **ID** | `HIB-DCAST-LATEPHASE.1` |
| **Found** | 2026-08-01, while re-verifying `HIB-GCOVERHEAD-HALFFULL.1` against the `dev` tip. |
| **Two faults, not one** | (a) *test failures / truncated runs* — pre-existing, reproduces on pure `dev` with no local changes; (b) *VM crashes* — appear only when selective promotion is enabled **on this tip**, and never on base `32f9db9a2`. See the table. |

## The measurement

Same class, same classpath, `--Xmx 1500m`, JIT on, real JDK. Sixteen runs on the
`origin/dev` tip `cc8167f94`, split by whether the non-moving sweep's SELECTIVE
PROMOTION is enabled:

| arm | runs | crashes | other |
|---|---|---|---|
| promotion **ON** (`fix/hib-gcoverhead-halffull-20260731`) | 7 | **5** (4 SIGSEGV, 1 `capacity overflow` panic) | the 2 non-crashing runs were clean `132/132` |
| promotion **ON** + `CRATONVM_NO_MOVING_YOUNG=1` | 1 | **1** SIGSEGV @118 | — |
| promotion **OFF** (`CRATONVM_NO_SELECTIVE_PROMOTE=1`, same binary) | 3 | **0** | 1 clean; 1 × 6 failures @54; 1 × 1 failure @11 |
| promotion **OFF** — pure `origin/dev`, no local changes | 5 | **0** | 4 clean; 1 × 1 failure @50 |
| promotion **ON**, on base `32f9db9a2` instead of the dev tip | 3 | **0** | 3 × clean `132/132` |
| promotion **ON** + `CRATONVM_NO_OLDGEN_COALESCE=1` | 2 | **0** | 1 clean `132/132`; 1 × 8 failures and progress collapsing to ~2 tests/20 min |
| HotSpot control | 2 | 0 | `132/132`, ~120 s |

**6 crashes in 8 promotion-ON runs; 0 in 8 promotion-OFF runs.** And promotion-ON
on the *older base* was 3-for-3 clean. So:

- Selective promotion is **unsafe on the `dev` tip and was not on `32f9db9a2`**.
  Something in dev's 187-file span between those two commits either broke it or
  exposed a latent unsafety in it.
- It is not moving-young engaging: the crash survives `CRATONVM_NO_MOVING_YOUNG=1`.
- It is not the movable-JIT-root exclusion: that bound is already in the
  promotion-ON binary (see
  [`invocation12-...-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md)),
  and the crashes persist through it.
- Separately, and independently of promotion, the class fails *tests* on this tip
  in every arm (11, 50, 54-test truncations with real failures). That older
  instability is what this document was originally filed for.

⚠️ **This blocks merging `fix/hib-gcoverhead-halffull-20260731` into `dev`.** The
GC-overhead fix on that branch is correct and independently proven (see its own
doc, and `probes/GcPromoteProbe.java`, which still wedges on the `dev` tip) — but
it cannot land until this is understood, because landing it turns a heap wedge
into a VM crash.

### All three crashes attribute themselves identically

```
#  gc young-gen actual: 3-5 moving cycle(s), 24-28 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee
#  gc young-gen: the faulting thread had an UNREGISTERED JIT frame on its native
#     stack in the last root-gathering pass (no precise root map for it)
```

## Where to start

Note the moving-cycle count above: on this tip moving-young **does** engage,
which it did not on `32f9db9a2`. That is not the cause on its own — the crash
survives `CRATONVM_NO_MOVING_YOUNG=1` — but it does mean the tip mixes copying
and sweeping young cycles in one run, alongside unguarded direct JIT→JIT callees
(`FOREIGN_INNERMOST_RBP`) and unregistered frames.

Cheap next steps, in order — the top one is a specific, testable hypothesis:

1. **`730348352`'s old-gen coalescer is the leading suspect, and it was tested.**
   That commit ships its own escape hatch, so no rebuild is needed:
   `CRATONVM_NO_OLDGEN_COALESCE=1`. With it set, **2 runs produced 0 crashes**
   where the same binary crashed 6 times in 8 without it. Selective promotion is
   the only thing on either branch that allocates into old gen at volume, and a
   coalescer that widens a free block over a live promoted object corrupts
   exactly like this.

   That is suggestive, **not conclusive** — n=2, and the flag is not a viable
   configuration in its own right: the second run degenerated to ~2 tests per
   20 minutes, which is precisely the un-coalesced free-list collapse
   `730348352` exists to fix. So the answer is not "turn coalescing off"; it is
   to find why coalescing and freshly-promoted old-gen objects disagree. Start
   at `OldGen::coalesce_free_blocks` and ask what can put a block on the free
   list while a promoted object still occupies it — the selective-promotion
   evacuate pass installs `GC_FLAG_OLD_GEN` and bumps `bytes_promoted` at
   `gen_heap.rs` ~6800, and its "(3) fix up references … then dirty cards" pass
   runs later, so there is a window.

   Sibling candidate, untested: `c3dbb011a` ("the old-gen mark must not accept
   unvalidated addresses, and old-gen liveness must be free-list aware").
2. `CRATONVM_DBG_SWEEP_LIVENESS=1` (dev's own `6ce9be3ab`/`e91fd7232`) for a full
   class run — it asserts nothing live still points at a block the sweep frees.
   It does **not** fire on `probes/GcPromoteProbe.java`, which promotes megabytes
   per cycle, so whatever this is needs the class's shape.
3. Bisect `32f9db9a2..cc8167f94` with promotion forced ON, 3 runs per point.

## 2026-08-01 — the corruption is IN OLD GEN, and dev's own validators see it

Running the class on the promotion-ON binary with `CRATONVM_DBG_SWEEP_LIVENESS=1`
+ `CRATONVM_GC_STATS=1` (2 runs: one clean `132/132`, one exiting `rc=3` at 129
tests) produced the first direct evidence rather than a symptom:

```
old-gen mark: rejecting object at 0x207908e5610 with implausible extent 0
    (kind=0, array_len=0, num_slots=33554433) — corrupt header        [x2]

old-gen mark: rejecting external-overlay(BFS owner) candidate 0x2078a1b8b00
    — not a plausible object base (aligned=true, w0=0x00...)          [x8]
```

Both come from `c3dbb011a`'s new `old_gen_mark_candidate_plausible` screen, and
both point the same way:

* `num_slots=33554433` is `0x0200_0001` — not a plausible slot count, and the
  shape of a header word read at the **wrong base** (or of live old-gen bytes
  overwritten by something that thinks it owns them). Selective promotion is the
  only thing writing object headers into old gen at volume on this branch.
* Eight external-overlay entries resolve to old-gen addresses that are not object
  bases. `run_non_moving_young_cycle` calls
  `external_roots::remap_external_roots(&pointer_map)` right after the young
  sweep precisely so promoted objects' overlay owners follow the young→old move,
  so these are entries that either missed that remap or now point at memory that
  has been reused underneath them.

The `CRATONVM_DBG_SWEEP_LIVENESS` assertion itself did **not** fire in either
run — so whatever frees or overwrites this memory is not the old-gen sweep
freeing a block something still points at. Combined with the
`CRATONVM_NO_OLDGEN_COALESCE=1` result above (0 crashes in 2 runs), the surviving
shape of the hypothesis is: **a free block that does not correspond to a real
dead object gets merged and handed back out over live promoted data.** The next
question is what puts such a block on the list — an over-sized `OldGen::free`,
or an alloc that leaves an inconsistent remainder — not whether the coalescer's
own strict-adjacency merge is sound (it is).

Instrument `OldGen::free` and `alloc_from_buckets` to assert the freed extent
matches the object's real extent and that remainders never overlap, and re-run.
That is a cheap, targeted next step and it does not need the 27-minute class:
`probes/GcPromoteProbe.java` promotes megabytes per cycle and can be pushed into
old-gen sweeping with a smaller `--Xmx`.

## 2026-08-03 — `OldGen::compact()` ruled out; the corruption is in the shared mark-phase BFS

Session on `fix/hib-mapresize-chain-cursor-retire-20260803`, working the
`--nojit` sibling doc
([`map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md)),
produced two things directly relevant here.

**The actual corrupted bytes, finally.** A raw header dump added to
`scan_region`'s break-on-implausible-header path (`gc/src/old_gen.rs`)
captured, identical byte-for-byte at the identical old-gen offset across two
independent `--nojit` runs:

```
class_id=16 kind=Object gc_age=2 gc_flags=0x03(OLD_GEN|MARKED)
identity_hash=0x014df581 shape/num_slots=0x02000001 (33554433)
forwarding_ptr=null mark_word=0
```

Every field looks like a genuine, correctly-marked live object — `gc_flags`
especially — **except `shape`**, which is nonsense for any real class.
Deterministic (not a race), and the header's plausible fields rule out
generic memory garbage: this is one specific field being written wrong.

**`OldGen::compact()` was the leading suspect (this doc's own #1 next step,
above) and code review of its Phase 1-3 found nothing.** So it was bisected
out instead: a lever forcing every old-gen major GC through the in-place
(non-compacting) arm, tested on the `--nojit` sibling's own repro. Three
independent runs (two via the lever, one via the resulting production
default) all completed **cleanly at exact HotSpot parity** — where every
prior run, of any kind, on that reproducer had crashed. `OldGen::compact` is
now **disabled by default** on that branch
(`oldgen_compact_enabled()`/`CRATONVM_OLDGEN_COMPACT=1`), and it fixed that
doc's own narrower `--nojit` scope.

**It does not fix this doc.** A run of `DefaultCatalogAndSchemaTest` under
JIT-on, real JDK, on the SAME binary with `compact()` disabled, still
crashed:

```
EXCEPTION_ACCESS_VIOLATION at old_gen_gc+0xFA3 (gen_heap.rs:10060,
  Self::scan_object_for_old_refs — inside the mark-phase BFS)
gc young-gen actual: 2 moving cycle(s), 30 cycle(s) diverted to the NON-MOVING sweep
```

with `old-gen mark: rejecting object ... num_slots=33554433 — corrupt header,
not scanned` logged eleven lines earlier in the same run — the identical
signature. `scan_object_for_old_refs` runs inside `old_gen_gc`'s mark phase,
which is **shared** by both the compacting and non-compacting arms and runs
*before* the branch between them is even reached — so this is direct proof
the corruption is not produced by `compact()`, does not depend on which
old-gen reclamation strategy the caller requests, and predates the point in
the collection cycle where that choice is made.

**Why the `--nojit` sibling doc's fix "worked" anyway.** Disabling
`compact()` did not remove the corruption — it stopped that specific
`--nojit` reproducer from *walking into* an already-corrupted object during
mark or sweep. `gc young-gen actual: ... 30 cycle(s) diverted to the
NON-MOVING sweep` above shows this JIT-on workload spends nearly all its
old-gen time in the in-place sweep regardless of `compact()`'s setting, so
"disable compact()" was never going to touch its crash rate — consistent
with what was actually observed. The `--nojit` and JIT-on lanes evidently
differ in whether/when they reach the corrupted object, not in whether the
corruption exists. That difference (workload shape, timing, promotion
volume — not yet measured) is the next thing to explain, not `compact()`.

**Where this leaves the coalescer hypothesis at the top of this doc.** Still
not directly tested here (only inferred safe by the shared-mark-phase
argument above, and by the `--nojit` sibling's own finding that
`CRATONVM_NO_OLDGEN_COALESCE=1` did not prevent ITS crash either, just
changed SIGSEGV to a different fault). Worth an explicit, paired
`CRATONVM_NO_OLDGEN_COALESCE=1` run on **this** doc's own JIT-on reproducer
before fully retiring that lead, but it is now a secondary lead behind
"what corrupts a header before/during the mark-phase BFS, shared by every
old-gen collection path."

**Next, concretely:** the literal ASCII text bytes this investigation keeps
finding where a pointer or header should be (`kind=0x3a` originally,
`"TestTask"` in one `--nojit` crash's shadow-stack dump this session — Java
9+ compact/Latin-1 strings store ASCII with no interleaved zero bytes, which
is exactly that shape) is the strongest unchased lead in either doc. Grep for
raw byte-level writes into old-gen memory that do not go through
`set_field`/the allocator; this reads like a type-confusion or wrong-stride
copy, not a GC-rooting bug, which is consistent with every GC-side detector
in both docs' histories (mark-phase plausibility screens, the
freed-while-referenced `SWEEP-LIVENESS` assertion, the walk-coverage guard)
coming back clean or only catching the symptom downstream.

## Cost

~27 minutes per run on a quiet box, and no arm fails reliably — the worst arm is
2 in 3, the best 1 in 5. Budget several runs per hypothesis, and prefer
`ListingRunner` over `CratonRunner`: the latter's failure dump is gated on
`failed != 0`, so it prints nothing when tests vanish at the container level.
