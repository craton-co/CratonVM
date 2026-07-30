# Hibernate five-class results after the HIB-TEMPORAL.1 ban retirement

Fresh runs on `codex/fix-hibernate-five-takeover-20260730` merged with
`origin/dev` `c8da3d918` (which contains the `org/hibernate/` package-ban
retirement, `3d040a400`). One dedicated release binary, `-Xmx2g`, Eclipse
Adoptium JDK 25.0.3.9 fixture, whole classes via `CratonRunner`, one process per
class, 900 s cap.

Ban lift verified in-binary: `CRATONVM_DBG_JIT_METHOD_STATS=1` on `LockTest`
now reports `c2=17 … compiles: c1=40 c2=19` with only
`ineligible-by-policy=8`, where the whole `org/hibernate/` package was refused
before.

## All lanes

`ban ON` = pre-merge binary (HIB-TEMPORAL.1 active). `nomoving` =
`CRATONVM_NO_MOVING_YOUNG=1`, which also re-enables the C2/IR optimizing tier
(see below).

| Class | HotSpot | ban ON, default | ban ON + nomoving | ban OFF, default | **ban OFF + nomoving** |
|---|---:|---:|---:|---:|---:|
| `LockTest` | FAIL 10.3 s | FAIL 26.5 s | FAIL 34.4 s | FAIL 34.5 s | **FAIL 34.4 s** |
| `OffsetDateTimeTest` | 51.1 s | TIMEOUT >900 s | PASS 757 s | TIMEOUT >900 s | **PASS 772 s** |
| `ZonedDateTimeTest` | 64.7 s | TIMEOUT >900 s | 980 s | (one-off early exit 663 s) | **PASS 798 s** |
| `OracleInlineMutationStrategyIdTest` | 41.4 s | PASS 442 s | PASS 429 s | PASS 322 s | **PASS 325 s** |
| `ASTParserLoadingTest` | 21.0 s | PASS 408 s | — | PASS 376 s | **PASS 380 s** |

`LockTest`'s failure is not a VM defect — HotSpot fails the same method with
identical counts. See
`docs/internal/fixed-suite-bugs/hibernate/locktest-pessimistic-write-timeout-is-not-a-vm-bug-20260730.md`.

Every passing row above has `failed=0` with `ok`/`aborted` counts **identical to
HotSpot** (`OffsetDateTimeTest` 324/164 of 488; `ZonedDateTimeTest` 404/204 of
608; `ASTParserLoadingTest` 106/106; Oracle 6/6). Correctness is at parity
throughout; only wall-clock differs.

## What the ban retirement bought

* **`OracleInlineMutationStrategyIdTest`: 442 s -> 322 s (-27%)** — the clear
  win, and it holds in both moving-young lanes (429 -> 325 s).
* **`ASTParserLoadingTest`: 408 s -> 376 s (-8%)** — modest but real.
* **The temporal cluster: nothing.** `OffsetDateTimeTest` is 757 -> 772 s
  (neutral/noise) and both temporal classes **still time out on the default
  config** with the ban lifted. This matches the prior measurement that
  narrowing this ban makes the temporal cluster *worse*, not better.

So the ban was a genuine throughput brake on the bulk-id and HQL workloads and
its retirement is worth keeping — but it is **not** what was hanging the
temporal classes.

## What actually unhangs the temporal classes

`CRATONVM_NO_MOVING_YOUNG=1`, in every lane, with or without the ban:

| | default | nomoving |
|---|---:|---:|
| `OffsetDateTimeTest` (ban OFF) | TIMEOUT >900 s | **772 s PASS** |
| `ZonedDateTimeTest` (ban OFF) | did not finish | **798 s PASS** |

**Correction to the earlier write-up on this branch.** I first attributed that
gap to the rewritable-root-map emission `moving_young` forces on. That is real
but secondary. The larger mechanism was found independently and landed on dev as
`docs/known-issues/jit-optimizing-tier-disabled-by-moving-young-default.md`:
`try_compile_inner` admits the optimizing IR pipeline only when
`!x64::moving_young_enabled()`, so with `DEFAULT_MOVING_YOUNG = true`
**every compile falls through to the single-pass C1 backend and the C2/IR tier
never runs at all.** `CRATONVM_NO_MOVING_YOUNG=1` is therefore not merely
"skip some GC bookkeeping" — it turns the optimizing tier back on.

That also explains why the two levers do not compose the way one would guess:
lifting the package ban makes Hibernate methods *eligible*, but under the
default they are only ever compiled by C1.

Note that dev's doc explicitly rejects flipping `DEFAULT_MOVING_YOUNG` back as
the fix ("trades a throughput ceiling for a GC-correctness hazard"). The
supported fixes are to give IR the exact-RBP + safepoint-map contract the gate
demands, or to scope the gate. Both are open.

## Where this leaves the five classes

On the best available configuration (ban lifted, `CRATONVM_NO_MOVING_YOUNG=1`):

* **4 of 5 pass with zero failures and HotSpot-identical counts.**
* The 5th (`LockTest`) fails identically on HotSpot and is not a VM issue.
* **None of this is under the suite's 300 s per-class cap** except `LockTest`,
  and Oracle at 325 s is now close to it.

Remaining gap to HotSpot on that configuration: 15.1x (Offset), 12.3x (Zoned),
7.8x (Oracle), 18.1x (ASTParser). Closing it is the JIT-tiering work tracked in
`jit-optimizing-tier-disabled-by-moving-young-default.md`, tomcat doc 30, and
`project_hib_actionqueue_graph_default_jit_tiering_blocker` — not a
Hibernate-specific defect.

## One unreproduced observation

In the `ban OFF, default` sweep `ZonedDateTimeTest` exited silently at 663 s
after 430 KB of normal output — no exception, no crash file, no `@@RESULT`.
An isolated re-run of the same class on the same binary ran the full 900 s and
timed out normally, so this did not reproduce and is recorded, not diagnosed.
