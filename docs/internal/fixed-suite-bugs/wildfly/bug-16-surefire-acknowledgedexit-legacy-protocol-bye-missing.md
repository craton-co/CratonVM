# Bug 16: Surefire fork never sends the legacy "goodbye" handshake for zero-test classes (older Surefire 2.x booter protocol)

## Status

**FIXED** on 2026-07-07 in `native-builtins/src/lib.rs`
(`native_surefire_forkedbooter_acknowledged_exit`).

## Symptom

When Maven Surefire 2.22.2 (JUnit4 provider -- still pinned by WildFly's
testsuite poms) forks CratonVM to run a test class with zero actually-runnable
`@Test` methods (an abstract base class matched by a `*TestCase.java`
discovery glob, or a concrete class whose tests are all filtered), the child
process exits cleanly (exit code 0) with the correct `Tests run: 0, Failures:
0, Errors: 0, Skipped: 0`, but Maven still reports:

```text
[ERROR] The forked VM terminated without properly saying goodbye. VM crash or System.exit called?
[ERROR] Process Exit Code: 0
```

Confirmed CratonVM-specific via HotSpot A/B on the identical class/command
(`org.jboss.as.test.integration.ejb.remote.distinctname.DistinctNameTestCase`):
HotSpot reports `BUILD SUCCESS`; CratonVM reports `BUILD FAILURE` for the
exact same invocation. Reproduces with a bare Maven Surefire invocation, no
WildFly server or deployment needed. In a 50-class sample from a full WildFly
suite bug-bash run, 31/50 (62%) of CRASH/ABEND classifications matched this
exact signature, making it the single highest-value fix for that suite's
signal-to-noise ratio.

## Root Cause

`ForkedBooter.acknowledgedExit()` is natively overridden
(`native_surefire_forkedbooter_acknowledged_exit`) so that CratonVM can drive
Surefire's newer (3.x-line) `eventChannel`-based exit protocol -- calling
`eventChannel.bye()` / `eventChannel.onJvmExit()` / `ForkedBooter.
closeForkChannel()` -- added for a WildFly run against Surefire 3.5.4 (see
bug-09/bug-13, whose logs show `eventChannel_null=false`).

Surefire 2.22.2 (what WildFly's testsuite poms actually pin) predates that
API: its `ForkedBooter` class has **no `eventChannel` field and no
`closeForkChannel()` method at all** (confirmed via `javap` against
`surefire-booter-2.22.2.jar`). Its real `acknowledgedExit()` bytecode instead:

1. Writes the literal line `"Z,0,BYE!\n"` to the captured `originalOut`
   `PrintStream` via the private `encodeAndWriteToOutput(String)` method, then
   flushes.
2. Optionally waits (bounded) for the parent's bye-ack.
3. Calls `System.exit(0)`.

Maven's parent-side `ForkClient` (`maven-surefire-common`) only sets its
`saidGoodBye` flag -- the thing that suppresses the "terminated without
properly saying goodbye" error -- when it parses that literal `"Z,0,BYE!\n"`
line off the forked process's stdout (`ForkClient.consumeLine`, the `"Z"`
event-prefix branch).

Because `eventChannel` does not exist on this class, `get_field_by_name(this,
"eventChannel")` always returned `Object(None)` for Surefire 2.22.2, so the
native override's `if let Some(event_channel) = ev { ... }` branch was always
skipped -- and with nothing in the `else` branch, the override went straight
to `std::process::exit(0)` without ever writing `"Z,0,BYE!\n"`. (It also
attempted `closeForkChannel()` via `invoke_special` regardless, throwing a
harmlessly-ignored `NoSuchMethodError` on this booter version -- a second,
cosmetic symptom of the same version mismatch.) The parent never saw the bye
line, `saidGoodBye` stayed false, and Maven treated the clean exit as a crash.

## Fix

In `native_surefire_forkedbooter_acknowledged_exit`, when `eventChannel` is
absent (the legacy 2.x protocol), invoke the real, private
`ForkedBooter.encodeAndWriteToOutput(String)` method via `invoke_special` with
the literal `"Z,0,BYE!\n"` before falling through to `cancelPingScheduler` /
`commandReader.stop()` / `process::exit(0)` -- matching what the real
bytecode does. `closeForkChannel()` is now only attempted when `eventChannel`
is actually present (Surefire 3.x), removing the spurious `NoSuchMethodError`
for 2.x forks.

The newer-protocol (`eventChannel` present) path is untouched, so the
Surefire 3.5.4 behavior fixed in bug-09/bug-13 is not affected.

## Verification

**Isolated minimal repro** (no WildFly needed -- a throwaway single-module
Maven project, JUnit4 4.13.2, `maven-surefire-plugin` 2.22.2, one abstract
`AbstractFooTestCase` with a single `@Test` method so Surefire's default
runner skips it as non-instantiable, `-Djvm=<cratonvm-binary>`):

- Before fix: `BUILD FAILURE`, "The forked VM terminated without properly
  saying goodbye", `Process Exit Code: 0`, dumpstream shows
  `eventChannel_null=true` and no bye write attempted.
- After fix: `BUILD SUCCESS`, `Tests run: 0, Failures: 0, Errors: 0, Skipped:
  0`. Reproduced clean across repeated runs.
- HotSpot baseline: `BUILD SUCCESS` (unchanged, as expected).

**Real WildFly classes** (`testsuite/integration/basic` module, direct
`mvn -Dtest=<class> test -Djvm=<binary>` invocation matching the suite
runner's command shape), before vs. after the fix:

| Class | Before | After |
|---|---|---|
| `ejb.remote.distinctname.DistinctNameTestCase` | CRASH (goodbye) | OK |
| `batch.common.AbstractBatchTestCase` | CRASH (goodbye) | OK |
| `ejb.transaction.exception.TxExceptionBaseTestCase` | CRASH (goodbye) | OK |
| `ee.injection.support.InjectionSupportTestCase` | CRASH (goodbye) | OK |
| `ejb.descriptor.AbstractCustomDescriptorTests` | CRASH (goodbye) | OK |
| `ejb.timerservice.mgmt.TimerManagementTestCase` | fails pre-test (needs `-Djboss.dist`) | 1 real test, errors identically under HotSpot with the same simplified invocation -- unrelated to this bug, needs full suite-runner flags to reach 0/1 tests cleanly |

All five true zero-test classes tested flipped cleanly from the "goodbye"
crash to `BUILD SUCCESS`.

**Known separate, still-open issue found during this verification (NOT
fixed by this change):** classes where the fork actually executes one or
more real `@Test` methods before exiting (e.g. via
`org.jboss.as.test.integration.ejb.remote.ejbnamespace.
EjbNamespaceInvocationTestCase`, or a trivial single-passing-`@Test`
project) can *still* hit "the forked VM terminated without properly saying
goodbye" even after this fix, independent of WildFly deployment infra. This
looks like a distinct bug in the output/event-stream path exercised only
once real test-run reporting has happened (plausibly adjacent to bug-13's
event-channel buffered-output interleaving), not the `eventChannel`-absent
gap this doc covers. Worth its own follow-up investigation and known-issue
doc if it reproduces cleanly outside the shared/contended Azure host (initial
attempts to isolate it hit contamination from another concurrent session's
WildFly suite run writing to the same shared
`apps/wildfly/testsuite/integration/basic/target/surefire-reports`
directory).
