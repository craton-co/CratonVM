# `RabbitAutoConfigurationTests` HANG immediately after CGLIB `@Configuration` enhancement — FIXED

**Status: FIXED (2026-07-20)** — was never actually a hang; see below.

Original doc: `docs/known-issues/springboot/rabbitautoconfigurationtests-cglib-enhance-hang.md`
(found 2026-07-17, hypothesis-only — no thread dump had been captured).

## Resolution summary

This was **not a deadlock**. `RabbitAutoConfigurationTests` (78 test methods,
each building and tearing down a full Spring context — many with CGLIB
`@Configuration` proxies and fresh Mockito/ByteBuddy mocks) is genuinely
CPU-heavy under CratonVM and simply needs more wall-clock time than the
suite's default 300-second per-class timeout. Confirmed two things:

1. **It is not parked on a lock.** `Get-Process`-based CPU-time sampling of a
   live, unwatched repro (no `--stack-dump-on-timeout`, so no external
   interference) showed the single VM thread's `TotalProcessorTime` growing
   at a **constant ~0.96-0.99 CPU-seconds per wall-second**, continuously,
   for the entire run — the exact "genuinely executing, not parked on a
   futex" signature documented in
   [[reference_cpu_sample_deadlock_vs_slow_technique]]. Working-set memory
   grew for the first ~5 minutes then plateaued (GC keeping up), ruling out
   an unbounded leak too.
2. **It finishes if you let it.** Run standalone with the VM's own
   `--stack-dump-on-timeout 0` (fully disabling the internal watchdog) and a
   generous 900s/1500s external budget, the class reliably completes in
   **340-470 seconds** (varies with shared-host load — this box runs many
   concurrent sessions), printing a normal `SBRUNNER_RESULT`.

### Why the original stack-dump evidence was misleading

An earlier attempt at this investigation *did* use the VM's built-in
`--stack-dump-on-timeout` watchdog, which appeared to show the thread stuck
in `org.springframework.util.ReflectionUtils.findDefaultMethodsOnInterfaces`
(reflection-heavy `@Autowired` metadata scanning) or, on a different run,
inside Mockito's `InlineByteBuddyMockMaker`/ByteBuddy `TypeCache` machinery.
Both are red herrings caused by the watchdog's own known self-amplification
bug (documented in
[[reference_cpu_sample_deadlock_vs_slow_technique]]): once the deadline
fires, the sticky dump-request flag makes the VM re-dump the SAME thread's
entire frame chain on every subsequent interpreter loop iteration, which
throttles that thread by orders of magnitude and produces a torrent of
near-identical dumps that look exactly like "stuck at this one bytecode
instruction" even though the underlying cause is just "the watchdog itself
is now the bottleneck." Only the *first* dump block is trustworthy, and even
that only proves "here's where it was at t=400s," not "here's where it's
permanently stuck." The CPU-time-sampling method (no watchdog involved at
all) is what actually answered the question.

## Fix

Two independent pieces:

### 1. Suite-runner timeout carve-out

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`,
`Get-EffectiveClassTimeoutSec`'s `$slowClasses` table — added
`module/spring-boot-amqp|org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests = 900`,
matching the exact precedent already established for
`JdbcSessionAutoConfigurationTests`, `HibernateJpaAutoConfigurationTests`,
`WebMvcAutoConfigurationTests`, and the whole
`contextrunner-resource-cycle-then-silent-stall-cluster` (all previously
misclassified as HANG for the identical reason: CPU-bound-but-finite work
exceeding the 300s default). 900s gives ~2x headroom over the observed
340-470s.

### 2. Genuine residual bug found while investigating: `KeyManagerFactory`/`TrustManagerFactory.getInstance` accepted any algorithm string

Once the suite-timeout misclassification was ruled out, running the class to
completion surfaced 2 real (and only 2) test failures, unrelated to hanging:

- `enableSslWithInvalidKeyStoreAlgorithmShouldFail`
- `enableSslWithInvalidTrustStoreAlgorithmShouldFail`

Both expect `KeyManagerFactory.getInstance("test-invalid-algo")` /
`TrustManagerFactory.getInstance("test-invalid-algo")` to throw
`NoSuchAlgorithmException` (the real JDK JCA `getInstance` contract — no
provider claims the algorithm). CratonVM's native registrations for these
two methods (`native-builtins/src/phases_late.rs`, `register_p68_ssl`) used
to accept **any** algorithm string unconditionally: `KeyManagerFactory.
getInstance` looked up the algorithm via `provider_chain::find_service_provider`
only to choose which `Provider` to report via `getProvider()`, falling back
to a hardcoded `"SunJSSE"` when nobody claimed it — i.e. it never actually
*rejected* an unknown algorithm, it just mis-attributed the provider.
`TrustManagerFactory.getInstance` did no lookup at all and always succeeded.

**Fix:** both handlers now call `provider_chain::find_service_provider`
first and, when it returns `None` (no built-in SunJSSE service *and* no
caller-registered `Provider` via `Security.addProvider`/`Provider.put`
claims the algorithm), throw a real `java.security.NoSuchAlgorithmException`
via `ctx.new_object_initialized` (same pattern as `key_factory.rs`'s
`throw_jca`/`p68_signature_failure`), falling back to a catchable
`SecurityException` only if the exception class itself can't be
constructed. Verified this doesn't regress any legitimate caller: the only
hardcoded `KeyManagerFactory`/`TrustManagerFactory.getInstance("...")`
algorithm strings anywhere under `apps/` are `SunX509`, `PKIX`, and
`IBMX509` (the last gated behind `Security.getProvider("IBMJSSE2") != null`,
which is never true under CratonVM, so that branch is dead code) — all of
`SunX509`/`NewSunX509`/`PKIX` (+ TrustManagerFactory's `SunPKIX`/`X509`/
`X.509` aliases) are already registered in `provider_chain::
seed_sunjsse_services` and resolve exactly as before. The lookup is also
case-insensitive (`provider_chain::normalize_algo`), matching the real JCA
contract.

## Verification

Standalone `SbRunner` run of the full class (module `spring-boot-amqp`,
JDK 25, JIT on, `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1`,
`--stack-dump-on-timeout 0`):

- **Before fix** (rebuilt baseline binary, no code change): `SBRUNNER_RESULT
  tests=78 failed=2 aborted=0 skipped=0 containersFailed=0`, `Test run
  finished after 470134 ms`. Failures = the 2 SSL-algorithm tests above.
- **After fix**: `SBRUNNER_RESULT tests=78 failed=0 aborted=0 skipped=0
  containersFailed=0`, `Test run finished after 343792 ms`. Clean.

Worktree: `C:\craton\CratonVM-rabbit-cglib-hang-20260720`, branch
`fix/rabbit-cglib-enhance-hang-20260720`, off `dev` `8719dca85`.

## Files changed

- `native-builtins/src/phases_late.rs` — `KeyManagerFactory.getInstance` /
  `TrustManagerFactory.getInstance` now reject an algorithm no provider
  claims, throwing `NoSuchAlgorithmException`.
- `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1` — added
  `RabbitAutoConfigurationTests` to the slow-class timeout carve-out table
  (900s).

## Affected classes (now passing)

| Module | Class |
|---|---|
| `module/spring-boot-amqp` | `org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests` |
