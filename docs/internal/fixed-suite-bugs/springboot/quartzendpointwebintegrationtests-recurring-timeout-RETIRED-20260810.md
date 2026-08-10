# `QuartzEndpointWebIntegrationTests` — recurring full-timeout HANG

**Status: RETIRED — 2026-08-10** (branch `fix/flyway-integration-timeout-margin-20260810`).

Supersedes `docs/known-issues/springboot/quartzendpointwebintegrationtests-recurring-timeout-hang-20260807.md`.
The live issue is now
[`moving-young-fallback-turns-three-classes-red-20260810.md`](../../../known-issues/springboot/moving-young-fallback-turns-three-classes-red-20260810.md).

## It asked for a specific measurement; here it is

The retired doc closes with:

> Whoever picks this up next should get a real HotSpot baseline timing and a
> longer-budget CratonVM timing to compute a throughput ratio the way
> `jooqautoconfigurationtests-timeout-regression-20260805.md` did, **before
> assuming this is a hang at all**.

Done, standalone and one class at a time, `--Xmx 2g`, same host and classpath
for both VMs:

| Arm | Result | `[moving-young]` fallback peak |
|---|---|---:|
| HotSpot 25.0.3 | **12.6s**, 45/45 pass | — |
| CratonVM, JIT on | **no completion in 2400s** (killed) | **#4096** `innermost-rbp-belongs-to-unguarded-callee` |
| CratonVM, `--nojit` | **262.9s**, run completes | none |

That answers the doc's own open question, and not in the direction it expected.
Its leading hypothesis was *"Throughput, not a hang — a per-test-body cost that
is already close to or over 300s on a clean run."* But the class completes in
262.9s with the JIT off, comfortably inside the 300s budget it is alleged to be
intrinsically over. With the JIT on it did not finish in **8x** that budget.

So the recurring HANG is not the class being heavy. It is the JIT-triggered
`[moving-young]` fallback degenerating the young generation — the same
mechanism, measured the same way, that kills
`IntegrationAutoConfigurationTests` outright (OOM after 3.8 hours, fallback peak
#16384).

Caveat on the `--nojit` arm: it reports 29/45 failed, but every failure is
`ApplicationContextException: Failed to start bean 'webServerStartStop'` from
`Connector["http-nio-8080"]` failing to bind — port 8080 was held by an
unrelated process on this shared host. Environmental, and it does not affect the
timing. Tests that bind port 0 pass. This is also worth keeping in mind when
reading the doc's historical `FAIL 3/45` and `4/45` rows.

## What it got right

- Separating this from the CLOSED 2026-08-03 NPE/Jersey doc, explicitly and on
  signature rather than on class name.
- Its historical table — PASSes spanning 121–645s, HANGs at 300 — which is what
  makes the "no real margin at 300s" reading reasonable from the data it had.
- Its refusal to call the root cause: *"Root cause: not confirmed. Two candidate
  mechanisms, neither confirmed or eliminated this session."* That was the right
  posture, and it is why this retirement is an answer rather than a correction.

## What remains open

Its candidate mechanism 2 — a genuine intermittent stall in the repeated
Tomcat-stop/Netty-start lifecycle churn — is **not excluded**. A single
non-completing JIT run and a single completing `--nojit` run show the fallback
is sufficient to explain the observed rows; they do not prove nothing else is
also wrong. If Quartz still misbehaves after the fallback is addressed, that
hypothesis should be picked up again rather than treated as closed here.

The suite runner now records `moving-young-fallback peak=#N <reason>` in the
`note` column, so the next occurrence carries its own evidence.
