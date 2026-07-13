# WildFly standalone boot hangs forever in CratonVM's STW cross-thread JIT-takeover during parallel-extension-add

Status: OPEN — new, found 2026-07-13 while root-causing
[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] after the
`ProcessBuilder.environment()` fix (`d22a5e73`) landed and changed but did not resolve that bug.
Severity: **Critical** — this is now the *dominant* cause (~91% of observed failures, see Scale) of
WildFly managed-server boot failure under CratonVM, larger than the companion
[[wildfly-remoting-classcastexception-parallel-extension-add]] race.
First confirmed: 2026-07-13, Azure worktree `test/wildfly-full-suite-20260707`, dev@e8f36c78 (round-6e
binary, `frozen-cratonvm-wildfly-bugbash-v6-20260713`, built after `d22a5e73`)

## Symptom

WildFly's standalone boot process ("`org.jboss.as.standalone`" launched exactly as Arquillian's
`CommonManagedDeployableContainer` launches it) reaches the `parallel-extension-add` boot step — the
point where every WildFly extension (remoting, undertow, elytron, jaxrs, ejb3, weld, connector,
clustering.*, microprofile.*, ...) is initialized **concurrently**, spinning up ~30-40 threads at once —
and then simply stops producing any further output, permanently, for as long as the process is left
running (tested up to 120s in isolation; Arquillian's own harness gives up well before that and reports
`LifecycleException: Could not start container`).

The last line ever printed before the hang:

```text
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for cooperative
mutators rounds=64 pending=6 taken=0
```

This is a CratonVM-internal diagnostic, not WildFly's own logging — it comes from CratonVM's JIT-tiering
safepoint/stop-the-world machinery. `pending=6` and `taken=0` mean six mutator threads were asked to
cooperate with a JIT-takeover safepoint and, after 64 polling rounds, **not one of them ever did** — the
classic signature of a stuck/deadlocked safepoint rather than an ordinary slow boot.

## Confirmed as a genuine permanent hang, not slowness

Ran the exact captured Arquillian launch command in isolation (no Surefire, no Maven, no other concurrent
shards) five times:

- 1/5 attempts crashed quickly via a different, independent bug
  ([[wildfly-remoting-classcastexception-parallel-extension-add]])
- 4/5 attempts hit this hang: `timeout 20` killed all four after 20s with zero further output past the
  STW warning; a fifth run given a full 120s budget also produced nothing more after the same STW warning
  line — 72 total log lines, then silence for the remaining ~100+ seconds. `server.log` (WildFly's own
  post-bootstrap log file) never gets created in any of these runs, because the logging extension is
  itself one of the ~30 extensions stuck in the same parallel-add operation that never completes.

## Scale (round 6e, this session, post `d22a5e73`)

Of 478 classes completed before the run was stopped for this investigation: 463 `FAIL`. Of those,
401 (87%) show `LifecycleException: Could not start container` — Arquillian's own wait-timeout path,
consistent with the managed server silently hanging rather than exiting — versus only 39 (8%) showing
`LifecycleException: ... exited unexpectedly with code [1]` (the crash path, matching
[[wildfly-remoting-classcastexception-parallel-extension-add]]'s rarer race). This is a **reversal** from
before the `ProcessBuilder.environment()` fix landed, where the two signatures were roughly balanced
(round 6b/6c/6d: ~50/50) — that fix changed the dominant failure mode from "fast crash" to "silent hang,"
without net progress in OK count (still 0 across every round observed so far).

## Why this matters beyond WildFly — related to, but apparently not fixed by, `bee86ff0`

A symptom with the identical diagnostic ("`STW cross-thread JIT takeover is still waiting for cooperative
mutators ... rounds=64 ... taken=0`") was previously root-caused and marked FIXED in
`docs/internal/fixed-suite-bugs/wildfly-gc-barrier-boot-hang-and-harness-fixes.md` (dev `bee86ff0`,
2026-07-07): `main-vm`'s post-`main()` cleanup blocked in a raw `pthread_join` loop
(`ThreadRegistry::wait_for_non_daemon_threads`) without registering itself as "blocked" with the GC
barrier, so it stayed counted as an "alive, uncooperative" safepoint participant forever once any
worker thread requested a GC/STW pause. That fix long predates this session's binary
(`dev@e8f36c78`, 2026-07-13, includes `bee86ff0`) — yet the hang still reproduces. Two details argue this
is a related-but-distinct trigger, not a straightforward regression of that exact fix:

- The earlier bug reproduced with `pending=1` (only `main-vm` itself uncooperative, stuck post-`main()`).
  This one reproduces with `pending=6` — multiple worker threads, hit *during* active, mid-boot,
  concurrent extension loading (`parallel-extension-add`'s ~30-40 threads), not after `main()` has
  returned and handed off to a background thread pool.
- The fixed scenario was specifically about a *single, identifiable* uncooperative participant
  (`main-vm` parked in `pthread_join`). This one has *several* pending participants simultaneously,
  which is more consistent with a genuinely different code path failing to reach (or poll for) a
  safepoint under heavy concurrent thread creation/classloading, rather than the same one-thread-stuck
  mechanism recurring.

Flagging both possibilities for whoever picks this up: either the `bee86ff0` fix has a gap that a
different uncooperative-thread scenario still falls through, or this is a second, independent way to
starve the same cross-thread JIT-takeover safepoint (also plausibly connected to the separately-noted
`wip/gc-stw-quota-race-20260710` branch, parked because an attempted fix there itself "HANGS — do not
merge"). Either way, WildFly's `parallel-extension-add` step is a clean, concrete, reliably-reproducible
trigger (~30-40 threads all doing real work — classloading, static init, service registration —
concurrently) that's considerably easier to reproduce in isolation than chasing this inside the full
suite, and should be a useful addition to whoever's test matrix for this general class of STW work.

## Repro

```bash
cd apps/wildfly/testsuite/integration/basic
rm -f target/wildfly/standalone/log/server.log
timeout 60 <cratonvm-javahome>/bin/java \
  -Xmx512m -XX:MetaspaceSize=128m \
  -Djboss.home.dir=target/wildfly -Djboss.server.base.dir=target/wildfly/standalone \
  -Djboss.server.log.dir=target/wildfly/standalone/log -Djboss.server.config.dir=target/wildfly/standalone/configuration \
  -Dorg.jboss.boot.log.file=target/wildfly/standalone/log/server.log \
  -Dlogging.configuration=file:target/wildfly/standalone/configuration/logging.properties \
  -jar target/wildfly/jboss-modules.jar \
  -mp <shared-dist>/modules:testsuite/integration/basic/target/modules \
  org.jboss.as.standalone -Dts.wildfly.version=32.0.1.Final -c=standalone.xml
# -> boots normally through ~30-40 thread startups, then (most runs) prints exactly one
#    "STW cross-thread JIT takeover is still waiting for cooperative mutators ... taken=0"
#    line and produces no further output for the rest of the timeout window.
```

Reproduces in ~4/5 attempts on an otherwise-idle host (no other concurrent shards); likelihood may be
load-dependent given it's a scheduling-sensitive race, so worth retrying a few times if it doesn't
reproduce immediately.

## Evidence

```text
/tmp/repro_iter1.log .. /tmp/repro_iter4.log, /tmp/repro_long.log on the Azure host — 5 isolated repro
  attempts, 4/5 hung silently after the STW warning line (72 lines total, then nothing)
/data/data/wt-wildfly-bugbash-20260707-runner/out/round6e-s*of6-jit-real-others-20260713-024309/
  logs/*.log — 401/463 FAIL classes this round show "Could not start container" (consistent with this
  hang; Arquillian's own timeout path never observes the CratonVM-side diagnostic directly since child
  stdout isn't forwarded into these logs)
```

## Related

[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] — the original, less-precise
documentation of this whole failure family, written before this specific root cause was isolated.
[[wildfly-remoting-classcastexception-parallel-extension-add]] — the rarer, independent crash-flavored
sibling hit during the same boot step; possibly related at a deeper level (both fire specifically during
concurrent, multi-threaded extension/module loading) but not confirmed to share a root cause.
`docs/internal/fixed-suite-bugs/wildfly-gc-barrier-boot-hang-and-harness-fixes.md` — the earlier,
already-FIXED (`bee86ff0`, 2026-07-07) bug with the *identical* diagnostic message but a different
specific trigger (`main-vm` parked in a post-`main()` `pthread_join`, `pending=1`); see "Why this matters"
above for why this looks related but not simply a reopened regression of that exact fix.
