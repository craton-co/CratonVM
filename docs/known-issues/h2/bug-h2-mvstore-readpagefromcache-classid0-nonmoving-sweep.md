# `MVStore.readPageFromCache` — `java.lang.Object cannot be cast to Page` under sustained non-moving young sweeps

## Status
**OPEN — not fixed.** Pre-existing silent heap corruption. See
*Session 2* below: the top suspicion is refuted and the repro did NOT
reproduce in 26 runs on 2026-08-01, including on the exact commit that
failed 8/20 the day before. Not caused by
— and not fixed by — the old-gen coalescing change committed alongside this
doc (`7303483521`); measured explicitly, see *Not the coalescer* below.

This is the residual recorded as "one unexplained repeat failure" in
[`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md).
It now has a repro that hits in **12–40 minutes at roughly a 40 % rate**,
instead of once in nine hours.

## Severity
**HIGH** — silent. No guard fires: zero `gen_heap::set_field`/`get_field`
out-of-bounds hits, zero `read_slot: corrupt Value cell` reports, no
`SIGSEGV`. The first observable symptom is application-level nonsense.

## Symptom
```
java.lang.ClassCastException: java.lang.Object cannot be cast to org.h2.mvstore.Page
    at org/h2/mvstore/FileStore.readPageFromCache(FileStore.java:2089)
    at org/h2/mvstore/FileStore.readPage(FileStore.java:1987)
    at org/h2/mvstore/MVStore.readPage(MVStore.java:1158)
    at org/h2/mvstore/MVMap.readPage(MVMap.java:632)
    at org/h2/mvstore/Page$NonLeaf.getChildPage(Page.java:1178)
    ...
    at org/h2/mvstore/MVMap.get(MVMap.java:417)
```

A reference that should point at an `org.h2.mvstore.Page` reads back as a
bare `java.lang.Object`. `java.lang.Object` is `ClassId(0)` — the ALL-ZERO
header shape. The non-moving young sweep **zeroes every span it reclaims**
(`young_mark::zero_spans_parallel`) before publishing it to the free list,
so "a live reference now points at a zeroed header" is the signature of an
object that was swept while still reachable: a **premature reclamation**,
i.e. a marking/root-coverage gap in the non-moving young sweep.

## Repro

```bash
cd apps/h2database/h2
CRATONVM_NO_MOVING_YOUNG=1 <cratonvm-bin> \
  --java-home /home/victor/jdk25 --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.store.TestMVStoreCachePerformance
```

`CRATONVM_NO_MOVING_YOUNG=1` **with the JIT on** is the whole trick. It
forces `divert_non_moving` true on every cycle that has conservative JIT
roots, so the workload runs the non-moving young sweep continuously
instead of occasionally. That is the same regime the
`xt-helper-window-conservative-scan` coverage fallback produces naturally
when 10–100 reader threads sit blocked in a native read under a compiled
frame — just held permanently instead of intermittently.

Do **not** add `--nojit`: with no JIT frames there are no conservative
roots, `divert_non_moving` stays false, and the collector takes the moving
Cheney path regardless of the flag. That configuration does not reproduce.

Failures land in rounds 3–4 (`testCache(10, …)`), typically 730–1750 s.

## Observed rate

18 runs, 2026-07-31/08-01, Azure box, `--Xmx 1g`, JIT on,
`CRATONVM_NO_MOVING_YOUNG=1`:

| arm | runs | CCE failures |
| --- | --- | --- |
| old-gen coalescing ON | 8 | 4 |
| old-gen coalescing OFF | 8 | 3 |
| pristine `origin/dev` binary (no coalescing code at all) | 2 | 1 |

**~40 % overall.** Every failure carries exactly 4 `cannot be cast`
lines (one per reader thread that trips over the same corrupted page).

### Not the coalescer
The first two samples happened to be a coalescing-ON failure against a
coalescing-OFF pass, which looks alarming. It was small-sample noise:
4/8 versus 4/10 across the full set (Fisher exact p ≈ 1.0), and the
**pristine `origin/dev` binary, which contains none of the coalescing
code, reproduces the identical signature**. The two are independent.

## Session 2 (2026-08-01): top suspicion REFUTED, repro did not reproduce

**Still OPEN. Not fixed.** What this session establishes is mostly negative,
which is worth more than another guess.

### The dropped-peer suspicion is wrong

Suspicion 1 below (`xt_root_scan` silently dropping a peer that misses its
20 ms deadline, leaving its JIT-frame oops out of the root set) is
**refuted**. The takeover pass does drop such a peer — but it is not the
last word. `interpreter.rs`'s STW entry runs

```rust
loop {
    take_over_pass(...);                 // freeze what it can, excuse those from the barrier
    if gc_barrier.wait_for_all_timeout(WAIT_SLICE) { break; }
    ...
}
```

The barrier still *expects* an unfrozen peer, so the loop iterates and
retries until every counted mutator has either arrived cooperatively or
been frozen. A `STATE_CANCELLED` peer is retried on the next round, not
skipped, and the collection cannot begin while one is outstanding. This
was measured, not just read: the new counter (below) shows unclassified
peers occurring **1–2 per run**, in runs that pass.

Do not spend time here again.

### The repro is not reliable — 0/26 today

| binary | runs | CCE failures |
| --- | --- | --- |
| current `dev` + instrumentation | 18 | **0** |
| `0cc13efa37` — the EXACT commit that failed 8/20 yesterday | 6+ | **0** |

Rebuilt yesterday's commit specifically to rule out "someone fixed it":
only two JIT-perf commits had landed in between, and yesterday's binary
passes today too. The variable is the **host**, not the code. Yesterday's
failures came with 2000–3700 other logged-in users and load average
300–379 from other tenants; today the box was quiet. Synthesising load
with 24 CPU spinners (load average 33) did **not** reproduce it — so
whatever the real trigger is, plain CPU contention is not it.

Treat the "~40 % in 12–40 min" figure below as *observed once, under host
conditions not reproducible on demand*, not as a recipe you can rely on.

### Correction: read the DISTRIBUTION, not the last decision

`[GC] decision #N: ... young=MOVING` reports only the **most recent**
cycle. Reading it as the run's behaviour briefly made this look like a
moving-collector bug. The new histogram shows the truth:

| conditions | moving cycles | non-moving cycles |
| --- | --- | --- |
| recipe, quiet host | ~40 | ~215 (`nonmoving-conservative-jit-roots`) |
| recipe, 24 CPU spinners | ~380 | ~75 |

So the non-moving framing of this doc is right on a quiet host — but the
mix **inverts** under CPU load, which is worth knowing before attributing
anything measured under load.

### Tooling added this session (all permanent, all default-quiet)

* `[GC] decision histogram: moving=N non_moving=M <reason>=k …` — the
  per-reason distribution across the whole run. This is what corrected the
  misreading above.
* `[GC] xt_peer_scan: unclassified_peers=N cycles_with_unclassified=M` —
  peers the cross-thread scan could not classify. This is what refuted
  suspicion 1.
* `CRATONVM_XT_PEER_DEADLINE_MS` (default 20) — the peer deadline is now
  tunable, so "is the 20 ms bet the problem?" is a one-flag experiment
  rather than a rebuild. (Tested at 5000; made no difference.)
* The **sweep-ring reporter is now wired to `checkcast`**. The
  "zeroed-a-live-object" ring (`CRATONVM_DBG_SWEEP_ZERO`) already existed
  and already had a consumer — but only on the stale-RECEIVER (invoke)
  path. A reclaimed object surfaces here as a failed **cast** first, so
  setting the flag and reproducing produced silence. On a ring hit the
  checkcast now reports the victim's ORIGINAL class plus the sweep cycle,
  GC reason, initiating thread and blocked-thread count — i.e. it names
  the root-coverage gap.

  It speaks **only** on a ring hit. `java.lang.Object` is `ClassId(0)`,
  which is also an ordinary `new Object()`, and nothing on that path
  distinguishes the two for free (`zero_forensics` and the sweep ring are
  both gated), so it deliberately does not guess — verified silent on a
  plain `(String) new Object()`.

**So the next occurrence self-diagnoses**, provided it is run with
`CRATONVM_DBG_SWEEP_ZERO=1`. That is the single highest-value thing to
carry into the next attempt.

## Where to look

The non-moving young sweep marks from the full root set plus conservative
JIT roots, and the documented safe direction is over-retention — a
conservative false positive can only keep a dead object alive, never free
a live one. Something in that chain is under-marking under sustained
multi-threaded load. Candidates, in rough order of suspicion:

1. **Cross-thread root coverage.** 10–100 mutators, most blocked in a
   native `read` under a compiled frame. `xt_root_scan`'s helper-window
   pass scans a blocked peer's register file + `[rsp, region_end)`
   conservatively — but only for peers in `blocked_os_tids`, and only
   while `arm_slot`/`send_takeover_signal`/`wait_for_response(20ms)` all
   succeed. Any peer that misses that 20 ms window is scanned by nothing.
   `slot.clear()` on timeout silently drops the peer.
2. **Side-marked survivors.** The sweep's `side_marked_survivor` arm keeps
   an object without touching its header; the interaction between that set
   and the disposition walk is where the identity-map fixes have
   repeatedly landed.
3. **Selective promotion.** Survivors evacuated young→old mid-sweep, with
   the `evac_map` remap arriving after some consumer already read the old
   address.

The first suspicion is directly testable: count peers examined versus
peers that reached `STATE_PARKED` (`[xt-jit-roots] linux helper-window
pass: examined N blocked peer(s), M window(s)`) and see whether the gap
correlates with the failing runs. `CRATONVM_DBG_XT_JIT_ROOTS` prints it.

## Related
- Same family as [`reference_fork6_gcstress_jitfree_corruption`] and the
  OSR non-moving-sweep corruptor — all "the non-moving young sweep freed
  something live".
- The `xt-helper-window` fallback that produces this regime naturally is
  *correct* (a blocked peer's JIT-frame oops genuinely cannot be
  rewritten); it should not be weakened to avoid this bug.
- `7303483521` fixes the old-gen **fragmentation** consequence of the same
  regime. Unrelated cause, unrelated fix.

## Possible cheaper handle: `TestDiskFull` (2026-08-01, unconfirmed)

While closing out the `TestDiskFull` corruption report, one run in **330** stock
`org.h2.test.synth.TestDiskFull` runs on `dev@c8a3ba181d` died with a `SIGSEGV`
whose registers carry this family's shape:

```
#  SIGSEGV at pc=0x7e65e95ad765, addr=0x0
#  maps: fault pc IS MAPPED - perms are on the `here` line   (r-xp)
#  slot[r10]: 0x0 0x0 0x0 0x0 0x0 0x0 0x0 0x0        r10=0x2003a0dd800
```

`addr=0x0` with a mapped executable `pc` is a *data* fault inside compiled code,
and `r10` points at eight zero words — an all-zero object header, i.e. a
reference into a span the sweep zeroed. The run showed **zero**
`gen_heap::set_field`/`get_field` guard hits and zero `corrupt Value cell`
reports, which is this doc's "silent" signature and not the (now fixed)
reference-processing one.

Why this might matter: `TestDiskFull` runs in ~40–110 s, against 12–40 min for
`TestMVStoreCachePerformance`. Against that, the rate here is **1 in 330** and it
did **not** recur: 203 further runs armed with `CRATONVM_DBG_SWEEP_ZERO=1`
produced neither a second crash nor a ring hit. So this is a single sample and a
lead, not a repro. Note also that most `TestDiskFull` runs wedge or time out for
a completely unrelated, non-VM reason — see
`h2-testdiskfull-upstream-transaction-recovery-livelock.md` — so a `TIMEOUT` row
for that class is not evidence of this bug.

Harness: `docs/known-issues/repros/h2-testdiskfull-livelock/`.
