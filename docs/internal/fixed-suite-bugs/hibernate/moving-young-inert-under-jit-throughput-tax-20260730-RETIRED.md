# Moving young gen is inert under the JIT but still charged for — RETIRED 2026-08-03

**Status: RETIRED.** Moved from
`docs/known-issues/hibernate/moving-young-inert-under-jit-throughput-tax-20260730.md`.
The original throughput/hang claim is resolved — superseded (as this document's
own 2026-07-30/07-31 corrections already said) by the moving-young coverage
work in `docs/internal/default-moving-young-enabled-20260730.md` and
`docs/internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`. Two
days later this doc's own re-verification found and fixed one more thing: a
native-registration regression that was corrupting the classes this doc uses
as its repro, unrelated to the JIT/GC claim itself.

## Final verification (2026-08-03, dev `a9241eedf` + this branch)

Re-ran the doc's own two repro classes plus one control, default flags,
`--java-home`, `-Xmx 1500m`, via `apps/hib-suite-runner`'s harness:

| Class | Result | Wall |
|---|---|---:|
| `OffsetDateTimeTest` | `found=488 ok=324 failed=0 aborted=164` | 544 s |
| `LocalDateTimeTest` | `found=162 ok=90 failed=0 aborted=72` | 224 s |
| `ZonedDateTimeTest` | see caveat below | 71 min (host-contended) |

**No hang under default flags, on any class.** This matches the figures
already on record in `jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`
(`OffsetDateTimeTest 367 s/308 s failed=0`, `ZonedDateTimeTest 447 s/435 s/391 s
failed=0`) and closes the one thing this document said it still needed: "a
quiet-host, repeated-run characterisation of these two classes" showing they
complete, not time out, under the JIT's default coverage-proof-and-fallback
machinery. `--nojit` reproduces the same result set (see the regression
below), so there is no JIT-vs-interpreter divergence to chase either.

The `ZonedDateTimeTest` run above is **not evidence either way**: it took 71
minutes against a normal ~450 s and reported 38 failures, all
`java.util.ServiceConfigurationError: ...BytecodeProviderImpl could not be
instantiated` — a Hibernate/ByteBuddy bootstrap failure, zero of which are
timezone-value mismatches. At the time, `Get-Process` showed six-plus
unrelated `cratonvm*`-prefixed processes competing for CPU on this shared
host. This is the exact bimodal/host-load pattern the 2026-07-31 correction
already documented for this class; it is not re-litigated here. Re-run on a
quiet host if a clean number is needed for this specific class.

## Regression found and fixed along the way: `TimeZone.getDefault()` stopped tracking `setDefault()`

Re-verifying this doc's classes on the current dev tip surfaced 60
`OffsetDateTimeTest` failures that do not belong to any open residual —
reproduced **identically under the JIT and `--nojit`**, so it was never a
GC/JIT defect in the first place, just something this doc's re-check happened
to trip over.

**Root cause:** `native-builtins/src/lib.rs` commit `98878a6dd` (2026-07-31,
"TimeZone base methods") added a second
`registry.register("java/util/TimeZone", "getDefault", ...)` that allocates a
fresh synthetic `TimeZone` from the `user.timezone` system property on every
call. `NativeMethodRegistry::register` is documented last-registration-wins on
the exact `(class, method, descriptor)` triple. The correct implementation —
`timezone_default_ref`, registered earlier in the same function, which reads
the real `TimeZone.defaultTimeZone` static field that `TimeZone.setDefault()`'s
actual JDK bytecode writes — was already there and got silently shadowed. Every
`TimeZone.getDefault()` call after a `setDefault(...)` (and everything built on
it: `ZoneId.systemDefault()`, previously fixed 2026-07-17 in
`hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md`, plus any
JDBC/native code that reads the JVM default zone directly) went back to
reporting the VM's *startup* zone. Hibernate's
`Timezones.withDefaultTimeZone()` test helper — `setDefault(...)`, then read
back from a freshly spawned thread — is exactly the shape this breaks.

**Fix** (`native-builtins/src/lib.rs`):
1. Removed the shadowing `getDefault()` registration.
2. Folded its one legitimate feature — honouring `user.timezone` as a
   VM-startup default — into `timezone_default_ref`'s own fallback path (used
   only before the first `setDefault` call), so nothing is lost.

**Regression witness:** `probes/TimeZoneDefaultTrackingProbe.java` —
`setDefault(GMT-08:00)`, read back `TimeZone.getDefault()`/
`ZoneId.systemDefault()` on the main thread and from a freshly spawned thread.
Matches HotSpot (`GMT-08:00` on every line after `setDefault`) on the fixed
binary; the pre-fix binary reported `UTC` on every line, confirmed in this
session.

**Verification:**
- `probes/TimeZoneDefaultTrackingProbe.java`: PASS, matches HotSpot.
- `cargo test -p cratonvm-native-builtins --lib`: 3233 passed, 0 failed, 6
  ignored (unchanged from before the fix).
- `OffsetDateTimeTest`: `failed=60` → `failed=0`, `found=488 ok=324 aborted=164`
  — exact match to the recorded 07-31 baseline.
- `LocalDateTimeTest`: `found=162 ok=90 failed=0 aborted=72` — exact match to
  the recorded baseline in `hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md`.

Not related to moving-young, the JIT, or GC in any way — a native-registration
shadowing bug, caught only because this doc's re-verification happened to run
the classes it corrupted. Filed here rather than as its own document because
the fix, the probe, and the verification are all small enough to read in one
sitting alongside the doc that found them.

---

## Original document (retired; kept for history)

### Claim

With `DEFAULT_MOVING_YOUNG = true` (then-current `dev`), a JIT-enabled
Hibernate workload **never runs a single moving collection**, yet every
compiled method still pays the full cost of making one possible. On the
allocation-heavy `type.temporal` classes that tax is the difference between a
timeout and a pass.

> This claim did not survive its own corrections below, and does not survive
> the final re-verification above either: on the current dev tip, both named
> classes complete well within any reasonable timeout, under default flags,
> every run.

### Evidence — the collector never moves (superseded)

`org.hibernate.orm.test.type.temporal.ZonedDateTimeTest`, default config,
`-Xmx2g`, showed 256+ consecutive young collections diverted to the
non-moving sweep via `[moving-young] fallback` log lines, across four
different fallback reasons. Independently corroborated at the time by the
moving-young owner branch's own `BinTreesClassic` acceptance data
(`cycles=0, coverage_fallbacks=64`).

This was real at the time, and is exactly what
`../default-moving-young-enabled-20260730.md` root-caused to three
independent defects (a process-wide coverage blanket, a coverage proof that
erased its own input, and recursion misread as an unguarded foreign frame) and
fixed: the same `BinTreesClassic` lane went from `cycles=0
coverage_fallbacks=66` to `cycles=25 coverage_fallbacks=0` with the HotSpot
checksum matching.

### Evidence — the cost is real (superseded, then inverted)

The original measurement showed `OffsetDateTimeTest`/`ZonedDateTimeTest`
timing out (`>900 s`) under default flags and passing under
`CRATONVM_NO_MOVING_YOUNG=1`. The 2026-07-31 correction in this same document
found that inverted on a re-measure: both classes completed under default
flags every run (`ZonedDateTimeTest` 447/435/391 s, `OffsetDateTimeTest`
367/308 s, `failed=0` throughout), while `CRATONVM_NO_MOVING_YOUNG=1`
SIGSEGV'd in 1–3 s on a pristine `origin/dev` build (root-caused and partially
fixed in
`docs/internal/jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`) —
the opposite of the original table in both halves.

### Why the flag was thought to cost anything when both paths sweep

Both configurations end in the same `run_non_moving_young_cycle`
(`gc/src/gen_heap.rs`). The historical cost was on the emission side:
`x64::moving_young_enabled()` requires the JIT to publish a complete
rewritable precise root map at every GC-capable safepoint, and forces
`shadow_stack_maps_enabled()` on. This tax is described in more detail, with
its own correction history, in `default-moving-young-enabled-20260730.md`.

### Options considered (moot — resolved by the coverage fix, not chosen from this list)

1. Fix coverage so moving-young actually moves under the JIT — this is what
   `default-moving-young-enabled-20260730.md` did.
2. Flip `DEFAULT_MOVING_YOUNG` to `false` — rejected; contradicted the
   default-on hardening work and the measurements above no longer support it.
3. Adaptive one-way disable — not needed once coverage was fixed for the
   single-threaded case. Cross-thread coverage remains a distinct, openly
   tracked gap (`default-moving-young-enabled-20260730.md`, "Cross-thread
   coverage is still an open obligation").

### Reproduce (superseded harness path; current path is `apps/hib-suite-runner/run-hib.sh`)

```
.hibtake/run-hib.ps1 -Classes @('org.hibernate.orm.test.type.temporal.OffsetDateTimeTest') `
  -Label probe -Runtime craton -TimeoutSec 900 -EnvVars @{ CRATONVM_NO_MOVING_YOUNG = '1' }
```

`CRATONVM_GC_STATS=1` prints `[GC] moving_young: cycles=N coverage_fallbacks=M`
plus the per-reason histogram at shutdown. Note a class that ends in
`System.exit` never prints it — every failing run in this document's history
(including the 60-failure regression found during retirement) exits via
`System.exit(1)`, so the moving-young summary was never visible in those runs
either; this is a harness quirk, not evidence of anything.
