# testsuite/model: Liquibase "Cannot end scope ... when currently at scope root" during JPA migration validation

Status: open

Date observed: 2026-07-06 (1200s-timeout rerun, branch fix/keycloak-nonpassed-rerun-1200s-20260706)

## Summary

15-16 of the 18 `testsuite/model` classes checked so far fail identically,
each taking ~200-280 seconds (previously these classes crashed instantly with
a separate, already-documented Infinispan `isClustered()` NoSuchMethodError —
that issue appears to have been fixed elsewhere on `dev` since the original
2026-07-04 sweep, since these classes now progress much further before
hitting this new blocker):

```
java.lang.ExceptionInInitializerError
    org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.validateSynch(LiquibaseJpaUpdaterProvider.java:263)
    org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.validate(LiquibaseJpaUpdaterProvider.java:231)
    org.keycloak.connections.jpa.DefaultJpaConnectionProviderFactory.migration(DefaultJpaConnectionProviderFactory.java:303)
    org.keycloak.connections.jpa.DefaultJpaConnectionProviderFactory.lambda$lazyInit$0(DefaultJpaConnectionProviderFactory.java:220)
    org.keycloak.models.utils.KeycloakModelUtils.suspendJtaTransaction(KeycloakModelUtils.java:1284)
    org.keycloak.connections.jpa.DefaultJpaConnectionProviderFactory.lazyInit(DefaultJpaConnectionProviderFactory.java:167)
    org.keycloak.connections.jpa.DefaultJpaConnectionProviderFactory.create(DefaultJpaConnectionProviderFactory.java:92)
    ...
    org.keycloak.models.sessions.infinispan.transaction.DefaultInfinispanTransactionProviderFactory.lambda$postInit$0(...)
    org.keycloak.models.utils.KeycloakModelUtils.runJobInTransactionWithResult(...)
Caused by: liquibase.exception.LiquibaseException: java.lang.RuntimeException: Cannot end scope <random-id> when currently at scope root
    liquibase.exception.LiquibaseException.<init>(LiquibaseException.java:32)
Caused by: java.lang.RuntimeException: Cannot end scope <random-id> when currently at scope root
```

The random scope ID differs per run (e.g. `rrdgtboxwd`), consistent with
Liquibase's `Scope` class generating a fresh random identifier per
`Scope.enter()` call — the error itself means something tried to
`Scope.exit()`/pop a scope that isn't the one currently on top of the stack
(or the stack is already back at its root), a mismatched enter/exit pair.

## Scale

Confirmed on 15-16 of 18 `testsuite/model` classes sampled so far (module has
~38 classes total; this run was still in progress on the remaining classes
when this doc was written — see the module's still-pending classes for
further confirmation). Two classes in the same module fail differently and
much faster (~3s): `CacheExpirationTest` and `MultiSiteProfileTest`, both
via a *different* `ExceptionInInitializerError` at
`DefaultInfinispanConnectionProviderFactory.createEmbeddedCacheManager` —
root cause not captured in this pass (the JUnit summary printer didn't emit
a "Caused by" chain for these two); flagged separately, not necessarily the
same bug.

## Notes / next steps

- Liquibase's `Scope` mechanism is typically implemented via a `ThreadLocal`
  stack of scope contexts (`Scope.enter()`/`Scope.exit()` push/pop pairs,
  normally used in a try/finally so every enter has a matching exit on the
  *same* thread). A "cannot end scope X when at scope root" error means a
  `Scope.exit()` call observed the stack already empty (at root) when it
  expected to pop a live scope — i.e. an extra/duplicate exit, a missing
  enter, or (most interesting for a VM-level bug) the `ThreadLocal` value
  itself being wrong/reset — e.g. if CratonVM's `ThreadLocal` implementation
  doesn't correctly preserve per-thread isolation across some boundary
  (thread pool reuse, a GC-triggered thread-state reset, or a JIT/interpreter
  transition), the scope stack a later `exit()` sees could belong to a
  different logical call than the `enter()` that pushed it.
- Given the multi-minute (200-280s) runtime before this fails, there's likely
  a real migration/DB-connection attempt happening first (Liquibase actually
  tries to validate/run schema changes against an H2 or similar embedded DB)
  — worth checking whether the scope corruption is triggered by a specific
  interleaving during that slow phase (e.g. a retry loop, a timeout-driven
  cleanup path that double-exits a scope) rather than being present from the
  very first call.
- Next session: get a `CRATONVM_DBG_ATHROW=1` trace (see
  `docs/known-issues/keycloak-07-04/smallrye-config-missing-charset-memorysize-converters.md`
  for the pattern) to see the exact sequence of Liquibase `Scope`
  enter/exit calls (or add a temporary print) leading up to the mismatch, and
  check whether CratonVM's `ThreadLocal` handling has any known gaps (search
  `native-builtins/src/` for the ThreadLocal native implementation) that
  could explain scope-stack values leaking or resetting across an unexpected
  boundary.
- Confirm whether this reproduces under real HotSpot in the same harness (not
  yet checked in this pass) — if it does NOT reproduce there, this is a
  genuine CratonVM-vs-HotSpot divergence; if it DOES, this may be an
  environment/timing issue in the harness itself (e.g. this host was under
  severe memory/CPU pressure from other concurrent sessions while this data
  was collected, which could itself cause timing-sensitive thread-local
  corruption independent of any CratonVM bug — worth re-running on a quieter
  host before concluding this is 100% a VM defect).

## Repro

```
ssh -i "C:\Users\Victor\.ssh\azure.pem" -o IdentitiesOnly=yes victor@20.83.144.174
cd /data/data/wt-keycloak-nonpassed-1200-20260706
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\ntestsuite/model\torg.keycloak.testsuite.model.clientscope.ClientScopeModelTest\n') \
  -TimeoutSec 300 -RunName repro-liquibase-scope \
  -KeycloakRoot apps/keycloak-fresh \
  -Exe target/release/cratonvm-nonpassed1200-20260706 -JdkHome /home/victor/jdk25
```

## Evidence

`/data/data/wt-keycloak-nonpassed-1200-20260706/apps/keycloak-suite-runner/.suite/results/nonpassed1200-20260706-shard4/others-jit/logs/testsuite_model.*.out.log` (2026-07-06 4-shard rerun with 1200s timeout).
