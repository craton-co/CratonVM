# OAuth2 issuer-URI test: CratonVM blows a 500 ms localhost HTTP budget

**Status:** 🔴 **OPEN**, CratonVM defect. The timeout is REAL, not spurious —
the read waited out its full budget and no response had arrived.

**2026-08-06:** two of the three open questions are now closed — the 500 ms
budget is library behaviour that **HotSpot applies identically**, and the client
is identified. A cheap local repro with both controls now exists. What is still
open is the stall itself.

Three earlier versions of this doc were wrong and are superseded:

1. "host-load artifact of the harness" — decided by CratonVM-vs-CratonVM, which
   cannot answer *is this ours*. HotSpot on the same host is **11/11 clean**,
   including at load average 47 while CratonVM failed at 16.
2. "spurious `SocketTimeoutException`, probably the poll layer" — measured and
   false. See below.
3. "the 500 ms client has not been identified, so whether HotSpot budgets the
   same is unproven" — both are now measured, and HotSpot budgets the same.

## The measurement that settled it

Instrumenting every read-timeout site in
`native-builtins/src/http_url_connection.rs` with elapsed-vs-configured, run
under 10 added CPU burners, it fired on the **first** run:

```
[HUC-DIAG] site=read_io_err/response read waited=507ms configured=500ms pooled=false
                url=http://localhost:44017/test/.well-known/openid-configuration
[HUC-DIAG] site=raise3               waited=1948ms configured=500ms pooled=false
```

Two facts, both surprising:

* **The configured read timeout is 500 ms, not the 30 s I assumed.** Every OIDC
  discovery request in the class runs with `read_timeout_ms=Some(500)`.
* **The wait is real.** 507 ms against a 500 ms budget. Nothing is short-cutting
  the timeout; CratonVM genuinely did not have a response after 500 ms for a
  round trip to a `MockWebServer` **inside the same VM**.

So the defect is **latency**, not timeout handling: a localhost HTTP exchange
that must complete within 500 ms sometimes doesn't.

Corroborating: `probes/MockWebHangProbe.java` (same request shape, 150
iterations, suite env) shows CratonVM worst-case **179 ms** vs HotSpot **99 ms**
with nothing else running — already ~2x, and the suite adds JIT compilation and
GC on top.

## Established

| | |
| --- | --- |
| HotSpot, same host, interleaved | 11/11 PASS (incl. load 47) |
| CratonVM, JIT | ~12 failures in ~25 runs |
| CratonVM, `--nojit` | **0** failures in 8 |
| the test alone (`OneMethodRunner`) | 6/6 PASS, ~9s |
| effective read timeout | 500 ms, on JIT **and** `--nojit` |

`--nojit` being clean while the 500 ms budget is identical points at
**compilation-time stalls** (background compile, deopt, or the GC they drive)
on the thread serving or consuming the response — not at wrong configuration.

## A cheap local reproduction, with both controls (2026-08-06)

The Azure recipe (whole class + 10 CPU burners) is no longer needed. **Six
concurrent lanes of the class on one Windows box reproduces it**, and the same
harness reproduces this page's own `--nojit` control:

| arm | runs | runs with >=1 failed test |
|---|---:|---:|
| CratonVM, JIT | 12 | **7** |
| CratonVM, `--nojit` | 12 | **0** |

Failures are `SocketTimeoutException: Read timed out` on
`autoConfigurationShouldConfigureResourceServerUsingOidcIssuerUri` and
`autoConfigurationShouldConfigureCustomValidators` — the same symptom this page
was opened for. The lanes supply their own load; no burners.

```powershell
# 6 lanes x 2 rounds, ~6 min per arm
run-single-class.ps1 -Module module/spring-boot-security-oauth2-resource-server `
  -ClassName org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests `
  -SpringBootRoot <built fixture> -Exe <binary>     # add -NoJit for the control arm
```

## What it is NOT: a baseline latency deficit

`probes/IssuerBudgetProbe.java`'s `headroom` mode sweeps the server delay: at
delay D the exchange has 500-D ms of budget left, so the largest D a VM still
passes measures what that VM consumes. On a quiet box:

| VM | headroom limit | consumed |
|---|---:|---|
| HotSpot | 480 ms | < 20 ms |
| CratonVM, JIT, **buffered** server read | **480 ms** | **< 20 ms** |
| CratonVM, JIT, byte-at-a-time server read | 400 ms | ~100 ms |

**CratonVM matches HotSpot when the server reads the way a real server does.**
So there is no constant per-exchange tax to find: the flake is a *transient*
that only appears under concurrent load with the JIT on, exactly as the
`--nojit` control implies. That eliminates a whole family of explanations
(connect cost, poll granularity, per-request HTTP overhead), all of which would
have shown up as a steady-state gap here and do not.

**The third row is a warning, not a result.** It was the first number this probe
produced, and it is an artifact of the probe: `readLine` issued one `read()` per
byte, and CratonVM's per-call socket-read cost (see below) turned ~150 header
bytes into ~10 ms of server-side latency. Read one way it "confirmed" a 100 ms
CratonVM latency deficit; the deficit was the instrument. A probe that reads
differently from the thing it models measures itself.

### The probe alone does not reproduce it — bounded negative

Shrinking the repro to the probe was tried and **failed**, which is worth
knowing before anyone tries again. `IssuerBudgetProbe soak 40` run 6-wide (240
exchanges, zero server delay, JIT on) produced **0 failures**, though it does get
close: 58 of 240 exchanges exceeded 400 ms and the worst was 1651 ms. The
per-request 500 ms read budget was never blown, because a slow *exchange* is not
a slow *read* — the budget is per read, and the probe's two reads stayed inside
it even when the surrounding work did not.

So the trigger needs what the probe lacks: 52 test methods each building a Spring
context, and MockWebServer, i.e. far more class loading and compilation churn
than two HTTP requests generate. Keep the 6-lane class repro; do not spend more
time trying to shrink it to a probe without a new idea about the mechanism.

## ROOT CAUSE (2026-08-06): a young GC pause longer than the read budget

`CRATONVM_DBG=gcpause` on the 6-lane repro, failing runs:

```
[gcpause] collection took 1538ms  scan_dirty_cards=22ms full_old_rset_scan=0ms
          root_forward=17ms overlay_forward=125ms card_root_forward=0ms cheney_drain=872ms
[gcpause] collection took  993ms  ... overlay_forward=94ms  cheney_drain=517ms
[gcpause] collection took  989ms  ... overlay_forward=88ms  cheney_drain=525ms
[gcpause] collection took  973ms  ... overlay_forward=101ms cheney_drain=505ms
```

**A stop-the-world young collection of 500–1538 ms, against a 500 ms per-read
budget.** MockWebServer runs *inside the same VM*, so the pause freezes the
thread that must write the response; the client's read then expires having
genuinely received nothing. That is the whole mechanism, and it explains every
established fact: the wait is real (this page's own finding), `--nojit` is clean
(far less allocation and compilation, so far fewer and shorter collections),
load matters (more lanes, more heap pressure), and HotSpot never does it —
its young pauses on this workload are single-digit ms.

Note the phases do **not** sum to the total (1036 of 1538 ms). The remainder is
outside the instrumented span — lock acquisition, the young object-start bitmap
build, and the post-drain finalizer/sweep/swap tail. Worth instrumenting next.

### Two defects behind the pause

1. **`cheney_drain` dominates — 283–872 ms**, i.e. the survivor copy/scan
   itself. This is the one that has to come down for the flake to go away;
   fixing anything else still leaves a pause over the budget.

   `CRATONVM_DBG=gcpause` now prints the counts that say what "slow" means:

   ```
   [gcpause] collection took 563ms  … overlay_forward=51ms cheney_drain=306ms
             objects_copied=994447 young_bytes_before=291741144 pointer_map_len=994447
   ```

   So **~994 000 objects copied out of a ~291 MB young gen, at ~300 ns each**.
   The count is astonishingly stable across cycles (994 444 / 994 445 /
   994 447 / 994 448 / 994 450).

   **That stability is NOT "survivors are never tenured" — I checked, and
   age-based promotion works.** `PROMOTION_AGE = 3`, and `forward_object_impl`
   increments `gc_age` in the `else` branch of the promotion test, i.e. on every
   young→young copy, so a survivor is tenured on its third cycle. The stable
   count is a genuine steady state: a workload building 52 Spring contexts
   allocates and retains a near-constant population. Worth stating because "the
   tenuring counter only advances on the path that has already tenured" is
   exactly what the shape of the number suggests, and it is wrong.

   What is left is arithmetic: a young generation of that size, at that survival
   rate and that per-object cost, cannot be collected inside a 500 ms budget.
   Two independent levers, and they are not alternatives — both are real:

   * **Young-gen sizing.** ~291 MB of young in a 2 GB heap is very large, and
     nothing ties it to a pause goal. `VmConfig` already carries
     `g1_max_gc_pause_ms` with no equivalent for this collector. Sizing young to
     a pause target is the standard fix and the one that would actually clear
     the 500 ms budget.
   * **Redundant per-object work.** `pointer_map_len == objects_copied`
     **exactly**, every cycle: one `FxHashMap` insert per copied object, on top
     of the forwarding pointer that `forward_object_impl` already installs in
     the source header two lines earlier. HotSpot pays only the forwarding
     pointer. The map exists to serve `remap_external_roots` afterwards, so
     removing it means teaching that pass to chase forwarding pointers instead
     — a contained change, ~1M hash inserts and one large map per cycle saved.

1a. **TRIED AND REVERTED — filtering the overlay walk is NOT the remaining cost.**

   The obvious next move after the lock batching was to stop materialising
   roots the collector throws away: push the young-span test *into* the
   provider's walk so only young-pointing refs are collected. Implemented
   (provider contract grew a `RefPredicate`, all three call sites updated,
   977/977 gc lib tests and every native-collections target green) and then
   **measured, interleaved A,B,B,A** against the batched binary:

   | | pooled median | pooled mean |
   |---|---|---|
   | A (batched only) | 43 ms | 43.3 |
   | B (batched + filtered) | 43 ms | 43.9 |

   No difference. The block means run 38.1 → 41.8 → 45.8 → 47.9 in *time*
   order regardless of arm, i.e. the host drifted ~3–4 ms per block over the
   run; fitting that drift out gives A ≈ 38.1 and B ≈ 37.8–38.5. Failures also
   went 12/24 to 20/24 versus the earlier session on the same box, so this run
   was measured under worse conditions than the one before it — the
   interleaving is the only reason that is visible rather than being read as a
   regression.

   **The premise was wrong, and it was inherited from this page rather than
   checked.** The claim that the phase materialises "millions of `ObjectRef`s
   per pause" was never measured. If building the `Vec` were the cost, removing
   nearly all of it would have moved the number. It did not, so the remaining
   ~43 ms is the **walk itself** — visiting every element of every overlay
   collection and doing the per-key table lookups.

   Reverted rather than carried: it is a cross-crate provider API change for an
   unmeasured benefit. **Do not re-attempt this.** The only lever left on this
   phase is the dirty flag below, which shortens the walk instead of shrinking
   its output; anything that keeps visiting every element will land in the same
   place.

1b. **CORRECTION — `pointer_map` is NOT simply redundant. Do not delete it.**

   An earlier note on this page framed `pointer_map` as ~1M `FxHashMap` inserts
   per cycle duplicating forwarding pointers the headers already hold, removable
   by teaching the remap to chase them. **Scoped 2026-08-07; that framing is
   wrong**, and acting on it would have broken two unrelated things:

   * It is a **public `GcResult` field consumed by a different collector.**
     `g1.rs` composes forward maps across evacuation rounds
     (`compose_forward_maps`, `identities(&acc.pointer_map)`) and asserts on its
     contents. Header forwarding pointers cannot serve that: composition needs
     the accumulated old→new mapping across rounds, not a single hop.
   * It is a **survivor oracle whose consumers outlive forwarding pointers.**
     Post-GC reference processing tests
     `pointer_map.contains_key(addr) || is_addr_live(addr)`, and the non-moving
     sweep keeps survivors in place with no entry at all — the gap behind the
     2026-07-07 RRWL / `ThreadLocalMap$Entry` IMSE/hang family. Chasing a
     forwarding pointer answers "where did it move", never "did it survive".

   The map is load-bearing for consumers that have nothing to do with
   forwarding. **The 82 references in `gen_heap.rs` are not 82 copies of one
   idea**, which is what made this look like a contained cleanup from the
   outside.

   What does survive scoping: the `FxHashMap`→`HashMap` rebuild at the end of
   the moving path (`pointer_map.into_iter().collect()`) pays ~1M SipHash
   inserts purely to satisfy the public field's type. The existing comment
   already argues this is the cheap end of a deliberate trade — the Cheney scan
   was moved to `FxHashMap` precisely to avoid SipHash per insert — so the
   remaining win is changing the *public field* to `FxHashMap`, which touches
   `g1.rs` and the VM consumers. Real, but an API change, not a local cleanup.

2. **`overlay_forward` is O(every overlay in the process), per minor GC** —
   68–125 ms and growing with heap population. `gen_heap.rs` seeds it with
   `external_roots_for_matching_owners(&|_| true)`: an always-true predicate
   that materialises **every** overlay-backed root (LinkedList /
   LinkedHashMap / TreeMap / TreeSet side tables) and only then filters with
   `young_from.contains(...)`. A minor collection should be proportional to the
   young set, not to the whole heap's overlay population.

   Traced to the source, it is worse than "one big Vec". There is exactly one
   provider (`native-collections`), and its
   `gc_overlay_roots_for_matching_owners` does, per minor GC, for **N** overlay
   owners (a Spring context has thousands):

   ```rust
   let owners = overlay_owner_keys().lock()… .keys().filter(|o| true).collect();  // 1 lock, 1 Vec
   for owner in owners {
       roots.extend(gc_overlay_roots_for_collection(owner, None));               // per owner:
   }                                                                             //   re-lock + clone
   ```

   and `gc_overlay_roots_for_collection` re-takes that **same global mutex** and
   does `index.get(&owner).cloned()` — so the cycle pays **1 + N lock/unlock
   cycles on one global mutex, N key-set clones, N result `Vec` allocations**,
   and then walks every element of every overlay collection. All of it to
   discover which handful of refs happen to be in young.

   Three fixes, cheapest first:
   * ~~hold the mutex **once** and gather owner→keys in a single pass~~ —
     **LANDED** `828f8a68b`. 102 → 59 ms per collection (~42%).
   * ~~batch the per-key table locks~~ — **LANDED** `8e8446104`. The owner-index
     fix above left the *dominant* half untouched: the per-key loop still took
     **7 more global mutexes per key**, and with the key count being the whole
     index that is thousands of acquisitions inside the pause. Each table is now
     locked once for a whole key list (`hm_int_fast` is sharded *by key*, so its
     keys are grouped by shard instead — bounded by 64, never locking an empty
     shard). Critically, this only pays off together with **flattening**
     `gc_overlay_roots_for_matching_owners`: batching the per-owner helper alone
     would still run one full set of acquisitions per owner (`6 * N`), which
     would have read as a fix and moved almost nothing.

     Measured interleaved **A,B,B,A**, 6 lanes, `CRATONVM_DBG=gcpause`:

     | arm | n | median | mean | p90 | max |
     |---|---|---|---|---|---|
     | A (before) | 33 | 57 ms | 59.9 | 76 | 107 |
     | B (after)  | 34 | **40 ms** | 41.7 | 61 | **62** |

     −30% median, −42% max; every B sample lands ≤62 ms while A reaches 107 ms,
     and *both* A rounds are worse than *both* B rounds, so the separation
     survives the ordering. **The `[gcpause]` line only prints when the TOTAL
     pause is ≥100 ms**, so as collections shrink they drop out of the sample and
     the survivors skew slow — this understates the fix rather than flattering
     it. No pass/fail rate is claimed: the arms alternated 6/12 and 4/12
     failures with the same config landing on both sides, the same pattern that
     already refuted one rate claim on this page.

   * **still open, and now the whole remaining cost** — the phase still walks
     every element of every overlay collection and materialises them all into a
     `Vec` the collector immediately discards. Two levels:
     - *cheap:* push a young-span **predicate** down into `native-collections`
       so only young-pointing refs are collected. It must be a predicate, **not
       a callback** — a callback would run with a table guard held and calls
       `forward_object`, which mutates the heap and could re-enter
       `native-collections`; a pure span read cannot. This shrinks the `Vec`
       from millions to a handful but keeps the walk.
     - *the real one:* a **dirty/card flag on overlay side-table writes**, so a
       minor GC visits only owners mutated since the last cycle. That is what
       makes the phase proportional to the young set instead of to the heap, and
       it mirrors what the card table already does for ordinary object fields.

### Fix 1 landed (default OFF): pause-goal feedback on the young trigger

`CRATONVM_GC_YOUNG_PAUSE_MS=N` feedback-sizes the young collection trigger
against a pause goal — halve on overshoot, give back additively when
comfortably inside, floored at capacity/16. Default `0` = off, because the right
default is a policy question one workload does not settle and this collector has
a documented history of young-sizing changes that helped one workload and cost
another (`with_capacity`'s note: capping the initial semi was a **net
regression** for large-`-Xmx` workloads, which fall back to the non-moving sweep
and just fire it more often). Reacting to a measured pause cannot repeat that:
a workload already inside its goal is never touched.

**What is established, and what is not.** The GC-level effects are direct
per-collection measurements over many samples and they hold:

* the trigger adapts as designed, `262144KB → 131072KB → 65536KB → 32768KB`;
* `cheney_drain` **266–341 ms → 33–76 ms**;
* `objects_copied` **857k–994k → 116k–236k**;
* worst observed pause **1016 ms → 550 ms**.

**The end-to-end failure-rate improvement is NOT established, and an earlier
revision of this page overstated it.** That revision reported goal-off 12/12,
goal-on 4/12, goal-off 12/12 and called the middle arm trustworthy because it
sat between two controls. A later properly-alternated run refutes it — four
6-lane blocks, flag flipped every block:

| block | card-table-only | failed |
|---|---|---:|
| A goal on | 0 | **0 / 6** |
| B goal on + ct-only | 1 | 6 / 6 |
| C goal on | 0 | **6 / 6** |
| D goal on + ct-only | 1 | 6 / 6 |

**A and C are the same configuration and gave opposite results.** Block-to-block
variance on this host is as large as the effect that was claimed, so the 12/12 →
4/12 reading cannot be attributed to the pause goal. It may still be real; it is
not shown.

The lesson is specific: a pass/fail *rate* over 6–12 runs on a box under hours
of sustained load is not enough resolution for an effect this size, while
per-collection pause and copy counts — hundreds of samples per run, measured
inside the process — are. Prefer the latter as the fix's evidence, and re-test
the rate on a quiet host before believing it.

**It is an improvement, not a cure — 4/12 still fail, and the reason is worth
more than the fix.** Shrinking young cut the drain exactly as intended but did
not cut the pause proportionally, because three phases do *not* scale with young
occupancy and now dominate:

| phase | goal off | goal on |
|---|---:|---:|
| `cheney_drain` | 266–341 ms | **33–76 ms** |
| `objects_copied` | 857k–994k | **116k–236k** |
| `scan_dirty_cards` | 12–13 ms | 32–37 ms |
| `full_old_rset_scan` | **0 ms** | **39–48 ms** |
| `overlay_forward` | 57–66 ms | 36–46 ms |

So the drain is down 5–8x and the fixed-cost phases — now ~105–130 ms per cycle,
paid on *many more* cycles — are what is left. Two new leads, neither
investigated:

* **`full_old_rset_scan` runs on EVERY young GC by default** —
  `full_old_rset_scan_enabled()` is just `!gc_flags().card_table_only`, and
  `card_table_only` is off by default. So `scan_all_old_to_young`, a full
  O(old-gen) walk of every object and every reference slot, runs on every minor
  collection, defeating the point of the card table. It is not a bug so much as
  a standing insurance premium: the comment above it is explicit that card
  marking is "a fast path only" and a missed barrier must never reclaim a
  reachable child.

  It measured 0 ms in the goal-off arm only because old gen was still small
  there; with the pause goal on, more frequent cycles promote sooner, old gen
  grows, and the same unconditional walk costs 39–48 ms. **Young GC cost
  therefore grows with old-gen size, permanently** — that is the scalability
  finding, independent of this flake.

  **Turning it off does NOT fix the flake — measured, and rejected.**
  `CRATONVM_CARD_TABLE_ONLY=1` alongside the 250 ms goal: `full_old_rset_scan`
  duly drops to 0 ms and typical pauses improve (184/120 ms), but the **tail
  gets worse** (worst 679/677/648 ms vs 550/546/543 with the goal alone) and the
  failure rate goes back to **12/12**. Same symptom throughout (`Read timed
  out`), so nothing here says the card table is *incorrect* — only that removing
  the full scan does not pay on this workload, and that whatever the card table
  misses shows up as occasional much larger cycles. Do not re-run this
  combination expecting a win.

  **Re-tested properly alternated, and the rate comparison collapses.** Four
  6-lane blocks with the flag flipped every block gave 0/6, 6/6, 6/6, 6/6 — and
  the 0/6 and the third 6/6 are the *same* configuration. So the earlier "12/12
  vs 4/12" reading, which is what made card-table-only look like a regression,
  does not survive alternation. What stands is only the per-collection fact:
  `full_old_rset_scan` goes to 0 ms and typical pauses improve (184/120 ms),
  while the tail was worse in the one arm where it was sampled (679 vs 550 ms).
  Whether card-table-only helps, hurts, or is neutral end-to-end is **open**,
  and needs a quiet host — not another run on this one.
* `overlay_forward` barely moves under the pause goal (57→40 ms) because it is
  proportional to the whole heap's overlay population, not to young. Its *share*
  of the pause is therefore much larger once the copying phases come down.

  **Do not read those two numbers as the lock fix.** `8e8446104` independently
  moved `overlay_forward` 57→40 ms median — the same figures, from a different
  comparison (batched vs per-key locking, pause goal off in both arms). The
  coincidence is unfortunate; the two results are unrelated and do not compound
  into 57→40→23. The mutex defect this bullet used to point at is now fixed
  (`828f8a68b` + `8e8446104`); what survives is the *walk*, which is what keeps
  the phase proportional to the heap rather than to young.

**Neither of the two defects below is fixed.** The moving-Cheney phase breakdown is new
(`CRATONVM_DBG=gcpause` now reports it) — before this, a slow collection on this
arm printed a bare total, because `gcphase` instruments only the non-moving
sweep this workload never takes. `cheney_drain` is the one that decides whether
the flake survives; `overlay_forward` alone cannot bring a 1538 ms pause under
500 ms.

## The pause is now FULLY accounted for (2026-08-07)

This page's own next step was *"the phases do not sum to the total (1036 of
1538 ms). The remainder is outside the instrumented span — lock acquisition,
the young object-start bitmap build, and the post-drain finalizer/sweep/swap
tail. Worth instrumenting next."* Done. Two blind spots are now instrumented:

* **inside the collector** — `mv_phase!` was declared after the young
  object-start walk and stopped at `cheney_drain`, so everything before
  evacuation and the whole post-drain tail (finalizer resurrection, promotion
  statistics, card clear + young reset, the `FxHashMap`→`HashMap` pointer_map
  rebuild, the semispace swap, the major-GC check, the monitor remap, adaptive
  expansion) was invisible. The stopwatch now starts at the top of
  `collect_garbage_inner` and marks every one of them.
* **outside the collector** — `CRATONVM_DBG_ROOTPROF=1` times each entry of
  `native_roots::VM_ROOT_SOURCES` in `scan_all_roots` / `remap_all_roots`, plus
  `collect_roots` and `update_all_roots` as wholes. That half contains a full
  walk of every overlay collection in the process and had never been measured.

6 lanes × 2 rounds on the Azure host, `--Xmx 2g`, default (goal-off) config, 26
collections over the `[gcpause]` 100 ms print threshold:

| phase | median | p90 | max | share |
|---|---:|---:|---:|---:|
| **TOTAL PAUSE** | **424 ms** | 506 | **628** | — |
| `cheney_drain` | 253 | 303 | 408 | 60% |
| `pointer_map_rebuild` *(new)* | 43 | 65 | 98 | 10% |
| `overlay_forward` | 39 | 51 | 55 | 9% |
| `promotion_stats` *(new)* | 29 | 35 | 43 | 7% |
| `pre_evacuate` *(new)* | 27 | 42 | 65 | 6% |
| `scan_dirty_cards` | 11 | 11 | 11 | 3% |
| `root_forward` | 3 | 4 | 5 | <1% |
| `cardclear+young_reset` | 3 | 3 | 4 | <1% |
| — outside the collector — | | | | |
| `update_all_roots` | 38 | 48 | 99 | — |
| ↳ `remap_all_roots:collection-overlays` | 34 | 43 | 91 | — |
| `collect_roots` | — | — | 26 | (over the 20 ms floor once in 12 runs) |

Counters: `objects_copied` median **755 944**, `young_bytes_before` median
**274 MB**. Phases sum to ~408 of the 424 ms median — the 500 ms gap this page
opened with is closed.

**The composition is stable, which is the durable result.** Re-measured with
`CRATONVM_GC_YOUNG_PAUSE_MS=250`, every absolute number roughly halves but the
shares barely move: `cheney_drain` 60%, `overlay_forward` 9.5%,
`pointer_map_rebuild` 8%, `promotion_stats` 9%, `pre_evacuate` 8.6%. So the
target list does not depend on the sizing policy: it is one 60% phase and four
8–10% phases. (The two arms were consecutive blocks, not interleaved, so **no
absolute comparison between them is claimed** — this page has already had one
rate claim collapse under alternation. Only the within-run shares are used.)

### FIXED: `promotion_stats` — 29 ms per pause to recompute a size the copy already had

Phase H derived `bytes_promoted` / `objects_promoted` / `bytes_copied_young` /
`objects_copied_young` by iterating all ~756 000 `pointer_map` entries after
the copy phase and **dereferencing each destination header** to recompute
`gen_object_total_size` — ~756 000 random header reads inside stop-the-world.
The comment justified it as avoiding "an expensive counter plumbed through
`forward_object`'s 12 call sites".

The counter does not have to be plumbed. The copy phase is single-threaded *by
construction* — every `forward_object_impl` destination parameter is
`&mut Arena`, so the borrow checker enforces it; the parallel part of a young
cycle is the mark closure, which is read-only and never forwards. A
thread-local tally is therefore sound, and `forward_object_impl` already knows
both the size and the destination arena at the point of the memcpy.

The BUG-Z forward validation the walk also performed is kept behind
`CRATONVM_DBG_FWDWALK=1` — it is a diagnostic and does not need to run on every
collection. `gc` suite 979/979 green, including
`phase_h_integration::rh1_promotion_stats_bump_across_cycles`, which is the
test that covers exactly these counters.

#### Measured A,B,B,A

Four 6-lane blocks, binary flipped every block, `CRATONVM_DBG=gcpause`:

| block | arm | n | median pause | p90 | `promotion_stats` |
|---|---|---:|---:|---:|---:|
| 1 | A before | 14 | 361 ms | 430 | 24 ms |
| 2 | B after | 17 | **333 ms** | 373 | **0** |
| 3 | B after | 15 | **300 ms** | 423 | **0** |
| 4 | A before | 14 | 345 ms | 423 | 27 ms |

The phase is gone, and both B blocks land below both A blocks, so the ~10%
median improvement survives the ordering rather than reading as host drift.
`cheney_drain` is unchanged (211/210 vs 210/184), which is the expected
negative control — nothing here touched the copy phase. **No claim is made
about the max**: it went 463/527 (A) vs 432/612 (B), i.e. the tail is dominated
by something this fix does not address, which is the honest state of this page.

### SCOPED, NOT LANDED: `pointer_map_rebuild` — 43 ms per pause, pure container churn

```rust
let mut pointer_map: HashMap<usize, usize> = pointer_map.into_iter().collect();
```

~756 000 SipHash inserts to convert the Cheney scan's `FxHashMap` into the
`HashMap` that `GcResult.pointer_map` is declared as. Nothing about the data
changes. Pre-sizing does not help — std's `FromIterator` already reserves the
full size, so the cost is the hashing itself.

The only fix is the declared type, and the blast radius is now measured rather
than guessed: **94 `&HashMap<usize, usize>` parameter positions** across `gc`,
`vm`, `jit`, `native-builtins`, `native-collections`, `native-io` and
`classloading`, and not all of them are pointer maps — a blind regex over that
set would silently retype unrelated `usize→usize` maps. It needs a deliberate
pass with a `PointerMap` alias, not a sweep. Left as a sized hand-off: 10% of
every young pause, no behaviour change, one afternoon of mechanical work.

### Still the 60%: `cheney_drain`

253 ms for 755 944 objects is ~335 ns per object — copy, forwarding install,
`pointer_map` insert and reference-slot scan. There is no single removable
item in it; the levers remain the two this page already names (fewer surviving
objects via young sizing, or a cheaper per-object copy). `perf` cannot help
narrow it on the Azure host: `/proc/sys/kernel/perf_event_paranoid` is 4, so
even `-e cpu-clock` user-only recording is refused.

## A separate, real defect found on the way: single-byte socket reads are ~35x

Not the cause of this flake — MockWebServer reads through buffered Okio segments
— but genuine, and worth its own fix. With the payload already in the receive
buffer (so no blocking, no wakeup, pure per-call cost),
`probes/IssuerBudgetProbe.java readcost` measures 8192 single-byte
`InputStream.read()` calls on a connected socket:

| VM | single-byte read | bulk read |
|---|---:|---:|
| HotSpot | ~1.5 µs/call | ~0.11 µs/call |
| CratonVM | **~55 µs/call** | ~0.3 µs/call |

Bulk reads are fine on both; it is fixed overhead per `read()` call. Any Java
code that parses a protocol byte-at-a-time off a raw socket — a hand-rolled
header parser, `DataInputStream.readLine`, an unbuffered `InputStreamReader` —
pays ~35x on CratonVM.

**Cause, and FIXED 2026-08-06.** `CRATONVM_DBG_READ0LAT=1` splits `net_read0`'s
per-call cost (means over 24 576 calls):

| stage | ns/call |
|---|---:|
| FileDescriptor field read | 91 |
| socket registry lookup | 59 |
| **`begin_blocking_region`** | **9 006** |
| the `recv` itself | 819 |
| **`end_blocking_region`** | **6 892** |
| total | 16 867 |

The GC blocking-region brackets are **~94% of the call and ~20x the `recv` they
guard**. `begin_blocking_region` retires the TLAB and calls
`deposit_root_snapshot()` — publishing the thread's entire Java root set — and
`end_blocking_region` re-syncs refs against any GC that ran meanwhile.

They exist so a read that parks in the OS cannot deadlock a stop-the-world, and
that is right — for a read that *can* park. It was being paid unconditionally,
including on the JDK's own timed-read path: `NioSocketImpl.timedRead` flips the
fd **non-blocking** and polls separately (in `Net.poll`, which keeps its own
region), so every `Socket.setSoTimeout(...)` reader paid a full root snapshot
per read for a call that returns `WouldBlock` instead of waiting. `net_read0`
now skips the brackets when `net_fd_is_nonblocking(fd)`. Safety rests on the
fd's mode, not on timing — there is no race in which a non-blocking fd starts
parking — and an unknown fd answers "blocking", keeping the region.

## Where the 500 ms comes from — RESOLVED 2026-08-06, and HotSpot budgets it too

**The fork is closed on the "pure CratonVM latency defect" side.** There is no
configuration path we get wrong: HotSpot applies the *same* 500 ms budget to the
*same* request.

`probes/IssuerBudgetProbe.java` settles it without instrumenting either VM, so
one measurement runs on both. It stands up a plain-`ServerSocket` OIDC endpoint
that stalls every response by a fixed delay (applied *after* the request is fully
read, so it is pure response latency), calls `JwtDecoders.fromIssuerLocation` —
the entry point the failing test reaches through `SupplierJwtDecoder`'s delegate
— and reports the outcome per delay. **HotSpot, JDK 25.0.3:**

| delay | outcome | elapsed | last request served |
|---:|---|---:|---|
| 0 ms | OK | 652 ms | jwks.json |
| 200 ms | OK | 408 ms | jwks.json |
| 400 ms | OK | 817 ms | jwks.json |
| 600 ms | **FAIL** | 503 ms | openid-configuration |
| 900 ms | **FAIL** | 507 ms | openid-configuration |
| 1500 ms | **FAIL** | 513 ms | openid-configuration |
| 3000 ms | **FAIL** | 509 ms | openid-configuration |

`SocketTimeoutException: Read timed out` at ~503-513 ms against a 500 ms budget,
on the discovery request — the same URL, the same margin and the same message
CratonVM's `[HUC-DIAG]` line recorded. The 500 ms is library behaviour, not ours.

**The client, from HotSpot's stack (which can be trusted):**

```
JwtDecoderProviderConfigurationUtils.getConfiguration(...:165)   RestTemplate.exchange
NimbusJwtDecoder.lambda$withIssuerLocation$0(NimbusJwtDecoder.java:233)
NimbusJwtDecoder$JwkSetUriJwtDecoderBuilder.jwkSource/processor/build
JwtDecoders.fromIssuerLocation(JwtDecoders.java:92)
```

and the budget is hard-coded in a **private inner class**,
`NimbusJwtDecoder$RestTemplateWithNimbusDefaultTimeouts`:

```
 4: new           SimpleClientHttpRequestFactory
13: sipush        500
16: invokevirtual SimpleClientHttpRequestFactory.setConnectTimeout:(I)V
20: sipush        500
23: invokevirtual SimpleClientHttpRequestFactory.setReadTimeout:(I)V
```

500 is a literal, matching Nimbus's `RemoteJWKSet.DEFAULT_HTTP_CONNECT_TIMEOUT` /
`DEFAULT_HTTP_READ_TIMEOUT` (both `= 500`, confirmed with `javap -constants` on
nimbus-jose-jwt 10.6).

That reconciles every earlier observation rather than contradicting them. The old
attribution was right about the *kind* of client and wrong about the *instance*:

* it really is a `SimpleClientHttpRequestFactory`, which is why the stack said so;
* but it is a **per-`withIssuerLocation` instance owned by that inner-class
  `RestTemplate`**, not the static `JwtDecoderProviderConfigurationUtils.rest` —
  so a subclass installed into the static one is never consulted, exactly as
  observed, on either VM;
* the static one really does hold 30000, and is genuinely not the client here;
* `-Dsun.net.client.defaultReadTimeout` cannot move a `sipush 500`.

**The warning still stands, with a sharper edge.** The stack was not *wrong*, it
was under-specified — and "right class, wrong instance" reads exactly like a
correct attribution. When a stack names a type you can reach two ways, identify
the *object*, not the class. The cheap way here turned out not to be
instrumentation at all: rebuild the shape in a standalone probe and measure both
VMs with it.

## Next step

Two things are done and should not be redone: the client is identified, and the
"steady-state latency" hypothesis is dead. What remains is a **transient stall
under concurrent load with the JIT on**.

Use the 6-lane repro above — it gives a 7/12-vs-0/12 signal in ~12 minutes, with
a positive and a negative control, on one box. Then:

1. Timestamp request-write and response-first-byte inside `perform`, and dump any
   exchange over ~100 ms with the wall-clock window it covers.
2. Correlate those windows against `[cratonvm-jitc]` compile activity and GC.
   The question is narrow now: *what does the JIT do that parks the
   MockWebServer thread (or the reading thread) for >500 ms?*
3. `--stack-sample-ms N` is the time-weighted profiler for this;
   `--stack-dump-on-timeout` is a CALL-COUNT trace and cannot see compiled
   frames, so it will not answer it.

Worth checking early, because it is cheap and would reframe the search: whether
the stall is one long pause or many small ones. The headroom result says
CratonVM has no steady-state deficit, so a >500 ms miss is very unlikely to be
an accumulation of small costs.

## Reproducing

The flake needs the whole class *and* load. Under 10 CPU burners it fired on the
first run; in a quiet window it can pass 6 times running.

```bash
# instrumented hunt loop used above
/tmp/hunt.sh          # burners + suite until a [HUC-DIAG] line appears
```

Arms must be interleaved, never run as consecutive blocks — see
[[feedback_interleave_ab_arms_never_run_them_in_separate_blocks]].
