# Moving young generation default-on delivery (2026-07-30)

## Outcome

The generational collector's moving/compacting young path is the supported
default. `CRATONVM_NO_MOVING_YOUNG=1` is the compatibility opt-out, and an
incomplete per-cycle JIT root-coverage proof still diverts that collection to
the non-moving sweep.

The compiled constant was already `true` on `origin/dev`: commit
`67de5400ac60d7d4097b0657bbfac9965cbd744a` changed it while fixing Liquibase
GC corruption. The rest of the repository had not completed that transition.
This delivery pins the empty-environment result directly, keeps the opt-out
contract covered, corrects the stale default-off documentation/comments, and
repairs a synthetic shadow-stack test fixture that no longer represented the
fixed-capacity production layout.

## Build and unit evidence

- `cargo check --workspace`: PASS.
- `cratonvm-types` moving-young filter: 1 passed.
- `cratonvm-types` library: 417 passed. Its separate `flag_surface` integration
  fixture rejected a checked-in UTF-8 BOM before any moving-young assertion;
  this is fixture invalidation, not VM evidence.
- `cratonvm-gc --lib`: 872 passed, zero failed.
- `cratonvm-jit --lib moving_young`: 2 passed, zero failed. The final complete
  JIT library run reached 819 explicit passes and 12 unrelated failures before
  the Windows test process hit `STATUS_ACCESS_VIOLATION` in
  `cooperative_poll_runs_in_a_pure_compiled_method`; it produced no valid final
  suite summary.
- `cratonvm-vm --lib moving_young`: 9 passed, zero failed.
- `shadow_window_is_recovered_from_a_live_compiled_frame`: 1 passed. The fixture
  now allocates `DEFAULT_SHADOW_SLOTS` and publishes the real fixed-capacity
  `end`, matching `ShadowStack::ensure_allocated`.
- Complete `cratonvm-vm --lib`: 2,448 passed, six failed, 111 ignored. The six
  failures are unrelated skip-list classification, panic-census, and
  serviceability socket baselines; every moving-young and shadow-window test
  passed.

## Runtime contract and pressure evidence

Release binary: `cratonvm-moving-young-default-019fb305.exe`, SHA-256
`66D0368246687D5FE120AF3ACCF8C1A39644D44CB0DC9869ABA54EAFBFE3ACB4`.

The pressure lanes returned their HotSpot checksums:

| Lane | Heap | Result | Moving diagnostic |
|---|---:|---:|---|
| default JIT | 512m | 68332206 | requested=true; cycles=0, coverage_fallbacks=64 |
| default `--nojit` | 128m, depth 16 | 14985902 | 31 non-diverted moving collections |
| `CRATONVM_NO_MOVING_YOUNG=1`, JIT | 8g | 68332206 | absent, as required |

The `--nojit` lane logged
`moving_young_requested=true divert_non_moving=false` 31 times and therefore
exercised 31 real Cheney cycles. The counter printed at shutdown is deliberately
limited to moves with live JIT roots, so it remains zero in that lane. The JIT
pressure lane proves every unsafe cycle is visibly fail-closed when the merged
JIT cannot recover an exact frame base; the correct checksum across both moving
and diverted cycles proves the safety split.

The HotSpot-differential regression suite passed all 18 classes in each of:

- default JIT;
- default `--nojit`;
- `CRATONVM_NO_MOVING_YOUNG=1`, JIT.

That is 54/54 class runs with every deterministic output matching HotSpot.

## Real-JDK application gauntlet

Every valid fixture was run as a whole class in both JIT and `--nojit`:

- Spring Boot: four classes, 17 tests per mode, zero failed, aborted, skipped,
  or failed containers. Classes covered security auto-configuration, Web MVC
  management context configuration, embedded Tomcat lifecycle, and thread-dump
  actuator behavior.
- Hibernate ORM: ten classes, 41 found/started and 41 successful per mode, zero
  failed/aborted/skipped, with all ten `@@RESULT` rows recorded.
- Tomcat: `org.apache.catalina.startup.TestTomcat`, `OK (26 tests)` per mode.

Two additional Spring Boot candidates were excluded after `SBRUNNER_LOAD_FAIL`
showed their compiled test classes were absent from the available fixture.
Likewise, no compiled standalone H2 suite fixture was present. Neither
pre-launch condition was counted as VM evidence.

## Safety contract

Default-on does not authorize relocation by itself. A live compiled frame must
still publish complete rewritable oop homes for the active safepoint. Missing
exact frame identity, an unguarded callee, a wide-locals gap, or another
incomplete proof records a reason and runs the non-moving sweep for that cycle.
The explicit opt-out overrides the compatibility opt-in for the compiled-frame
codegen/root/collector contract. Interpreter-only collections remain safely
relocatable because all of their roots are rewritable.
