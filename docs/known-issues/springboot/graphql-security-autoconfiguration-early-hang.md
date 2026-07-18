# `GraphQlWeb{Flux,Mvc}SecurityAutoConfigurationTests` HANG shortly after Spring Security CGLIB proxy setup

**Status: OPEN — found 2026-07-17**

## Symptom

Both Spring Security variants of the GraphQL security auto-configuration
tests HANG at the full 300s suite timeout:

| Class | Wall time (`results.tsv`) |
|---|---:|
| `org.springframework.boot.graphql.autoconfigure.security.GraphQlWebFluxSecurityAutoConfigurationTests` | `TIMEOUT`/`HANG`, 300.087s |
| `org.springframework.boot.graphql.autoconfigure.security.GraphQlWebMvcSecurityAutoConfigurationTests` | `TIMEOUT`/`HANG`, 300.064s |

Both `.out.log`s are completely empty (0 lines — no Spring Boot banner, no
JUnit output at all). Both `.err.log`s show the standard VM-boot
`Post-clinit fixup` lines, then (WebMvc variant only) a CGLIB proxy
definition for the test's own `@Configuration` fixture, then nothing further
except the generic `InterceptingExecutableInvoker`/`$Proxy*` OOB-field-read
guard warning this whole rerun's logs are full of:

```
[CCE] enhance: defined org/springframework/security/config/annotation/method/configuration/AuthorizationProxyWebConfiguration$$EnhancerByCGLIB$$0 (super=org/springframework/security/config/annotation/method/configuration/AuthorizationProxyWebConfiguration, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
2026-07-17T20:10:07.172945Z  WARN cratonvm_vm::vm::vm_util: Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION policy=ALLOW repaired=true
```

...followed by ~500 lines (both classes) of the repeating
`gen_heap::get_field` OOB guard warning against `org/springframework/core/$Proxy42`
(WebFlux) / `$Proxy45` (WebMvc) — a JDK/Spring dynamic proxy object — at a
tight, sub-millisecond-to-low-millisecond cadence, for the entire process
lifetime.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.security.GraphQlWebF-87da44ca989c.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.security.GraphQlWebM-bb81fe27ada1.err.log`

## Root cause (not confirmed — two hypotheses considered, neither proven)

Neither class uses `@ClassPathExclusions`/`@ClassPathOverrides` (confirmed
by reading both test source files in full — no such import), so this is
**not** an instance of the
[`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md)
fork-hang cluster despite the superficially similar "empty `.out.log`,
`InterceptingExecutableInvoker` OOB-warning churn" log shape — that
resemblance is misleading. Per the code comment at
`gc/src/gen_heap.rs:2018-2027` (`get_field`'s OOB-diagnostic rate limiter),
this specific warning is deliberately capped at 512 occurrences globally
per-process (`OOB_DIAG_CAP`) and is independently known to appear in 466/510
(91%) of this rerun's logs overall, **including passing classes** — so its
mere presence/volume is not on its own diagnostic of a hang here; it is
noise from ordinary JUnit/Spring reflection-heavy dispatch that happens to
keep recurring for as long as the process runs, whatever else it is doing.

**Hypothesis 1 — genuine early deadlock/livelock in Spring Security's
`AuthorizationProxyFactory`/CGLIB setup path.** The last unique, meaningful
log line before the warning churn takes over is the CGLIB enhancement of
`AuthorizationProxyWebConfiguration` (Spring Security's method-authorization
proxy-factory configuration, new in recent Spring Security releases). If the
process were merely slow rather than stuck, later test-run-visible output
(a Spring Boot banner, a Tomcat/Netty startup line, or eventual `SBRUNNER_RESULT`)
would be expected somewhere in 300s for what should be a small,
few-test-method class — none appears. This class of "CGLIB/dynamic-class
generation deadlocking against concurrent virtual dispatch" bug has a
confirmed, closed precedent in this codebase
(`docs/internal/fixed-suite-bugs/http-server-zerocopy-bytebuddy-vtable-classmanager-deadlock-FIXED.md`,
an ABBA lock-order deadlock between `class_manager` and `vtable_manager`).
**That specific fix is already present in this worktree** — confirmed via
`grep -n vtable_manager.read gc/src/gen_heap.rs`/`vm/src/runtime/interpreter.rs`:
`execute_invokevirtual_vtable_fast` (`vm/src/runtime/interpreter.rs:34047-34089`)
already drops the `vtable_manager` read guard before acquiring
`class_manager.read()`, matching that fix's description exactly — so this
is **not** a recurrence of that specific closed bug, but the general
"dynamic class generation + concurrent dispatch" trigger shape is similar
enough that a different, still-open lock-ordering or retry-loop hazard in
the same neighborhood is plausible.

**Hypothesis 2 — the churn genuinely reflects continuous (if very slow)
forward progress, and 300s is simply not enough time.** Not distinguishable
from Hypothesis 1 using only these logs: both classes' last timestamped log
line lands within a few hundred milliseconds of `300.06-300.09s` after
process start (computed from `results.tsv`'s wall time minus the last
timestamp in `.err.log`), which is consistent with either "still actively
logging right up until the watchdog kill" (progress) or "coincidentally the
watchdog fires right as a periodic/retried operation happens to log" (stuck
in a bounded retry loop) — the log data alone cannot distinguish these.

**What would confirm either hypothesis:** a live thread/stack dump
(`cdb`/`gdb -p <pid>` mid-hang, per this repo's
`reference_crash_debug_tooling`/`reference_linux_gdb_signal_passthrough`
convention) showing either a blocked lock-acquisition backtrace (Hypothesis
1) or genuinely varying, CPU-busy activity across repeated samples
(Hypothesis 2, per the same technique used to rule in/out a "real hang" in
`docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md`'s
residual-throughput section). Not attempted this session — flagged as the
concrete next step.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.security.GraphQlWebFluxSecurityAutoConfigurationTests` |
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.security.GraphQlWebMvcSecurityAutoConfigurationTests` |
