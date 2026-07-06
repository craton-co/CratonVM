# Keycloak test-framework: Phaser/ForkJoinPool hang in SmallRye Sisu bean loading

Status: open — newly exposed after fixing the `LinkedList.addAll(LinkedList)`
bug that previously masked it.

Date observed: 2026-07-06.

## Summary

With [the `LinkedList.addAll` dependency-resolution bug fixed](../../internal/fixed-suite-bugs/keycloak-testframework-linkedlist-addall-deployrequestedinstances-FIXED.md),
`tests/base :: org.keycloak.tests.admin.identityprovider.IdentityProviderMapperTest`
no longer fails instantly in `beforeEach`. Instead, `Registry.deployRequestedInstances()`
correctly proceeds to deploy `DistributionKeycloakServerSupplier`, which calls
`DistributionKeycloakServer.start()` → `ProviderDeployer.updateDependencies()` →
Quarkus's Maven bootstrap (`io.quarkus.bootstrap.resolver.maven.BootstrapMavenContext`)
to resolve the `keycloak-tests-custom-providers` artifact. This successfully
loads the entire Keycloak reactor's Maven workspace (hundreds of
`WorkspaceLoader` "Loaded module from ..." DEBUG lines), then **hangs
indefinitely** — confirmed hung for over 12 hours in one run, no CPU burn
(idle/blocked, not spinning).

## Where it's stuck

A `--stack-dump-on-timeout 90` run captured the (sole dumped) thread blocked
in:

```
tid=0 ... java/util/concurrent/Phaser$QNode.block()
tid=0 ... java/util/concurrent/ForkJoinPool.unmanagedBlock/managedBlock
tid=0 ... java/util/concurrent/Phaser.internalAwaitAdvance/arriveAndAwaitAdvance
tid=0 ... io/smallrye/beanbag/sisu/BeanLoadingTaskRunner.waitForCompletion()
tid=0 ... io/smallrye/beanbag/sisu/Sisu.addClassLoader(...)
tid=0 ... io/smallrye/beanbag/maven/MavenFactory.<init>/create(...)
tid=0 ... io/quarkus/bootstrap/resolver/maven/BootstrapMavenContext.configureMavenFactory/initRepoSystemAndManager/getRepositorySystem
tid=0 ... org/keycloak/it/utils/Maven.getArtifact/resolveArtifact
tid=0 ... org/keycloak/testframework/server/ProviderDeployer.getDependencyPath/updateDependencies
tid=0 ... org/keycloak/testframework/server/DistributionKeycloakServer.start
tid=0 ... org/keycloak/testframework/server/AbstractKeycloakServerSupplier.getValue
tid=0 ... org/keycloak/testframework/injection/Registry.deployRequestedInstances/beforeEach
```

SmallRye's Sisu (`io.smallrye.beanbag.sisu.BeanLoadingTaskRunner`) submits
background bean-loading work and waits for it via a `java.util.concurrent.Phaser`
(`arriveAndAwaitAdvance`), presumably registered so that the worker thread(s)
call `Phaser.arrive()` when done. The waiting thread never wakes — either the
worker task never runs, never completes, or never signals the phaser under
CratonVM.

## Why this wasn't seen before

The prior `LinkedList.addAll` bug (now fixed) made
`Registry.deployRequestedInstances()` throw before `DistributionKeycloakServer.start()`
was ever reached, so this code path was never exercised for this test. Real
HotSpot reaches and completes this exact path successfully in ~30s (see the
HotSpot baseline run for this class: 4/5 tests PASS, 1/5 fails on
`testDeleteProtocolMappersAfterDeleteIdentityProvider` with an unrelated
"Keycloak did not start within timeout" on the *second* server boot — a
possible timing/perf issue, not investigated here).

## Suggested next steps (not yet investigated)

- Check whether CratonVM's `ForkJoinPool.commonPool()` (or whatever pool Sisu
  submits to) actually spawns/runs worker threads in this scenario, vs. e.g. a
  synthetic/stubbed executor that never executes submitted tasks.
- Check `Phaser.register()`/`arrive()` semantics under CratonVM in isolation
  (a minimal repro submitting a task to a pool that arrives on a shared
  Phaser, without the full Keycloak/Quarkus/Maven stack).
- Cross-reference the existing concurrency fixes in
  `reference_executor_falsehang_concurrency` (memory) — that investigation
  covered `Thread.interrupt`, timed `Object.wait`, and `shutdownNow()` gaps,
  but not `Phaser`/`ForkJoinPool.managedBlock` specifically; this may be a
  related but distinct gap in the same "real concurrency primitives" area.

## Evidence

- Stack dump: `cratonvm-fixed-stackdump.log` (scratch — not committed).
- Repro: run `IdentityProviderMapperTest` under CratonVM with the
  `fix/kc-registry-deployrequested-instances-20260706` binary and
  `--stack-dump-on-timeout 90`.
