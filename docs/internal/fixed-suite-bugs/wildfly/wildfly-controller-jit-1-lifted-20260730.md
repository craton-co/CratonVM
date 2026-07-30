# WILDFLY-CONTROLLER-JIT.1 — fixed and retired 2026-07-30

**Status: FIXED AND RETIRED.** The `org/jboss/as/controller/` blanket JIT
ban has been removed. The current special-call runtime and the current
WildFly moving-root/metadata-lock repairs now execute the real parallel boot
with controller code compiled, rather than routing the entire package to the
interpreter.

## Resolution and acceptance

The branch was rebased onto `origin/dev` at `0c9935ab3`, including
`0fc08097d`'s native-handle peer-root remapping and class-manager recursive
read-lock fix. Those repairs remove the stale-receiver and parallel-boot
liveness residuals that previously obscured a direct re-verification of this
ban. The only branch code change is removal of the controller prefix guard,
protected by a unit gate for `OperationContextImpl.executeOperation` and
`AbstractOperationContext.executeOperation` under both JIT policies.

Final binary: `cvm-wildfly-controller-jit-20260729-019fb03f-final-0c9935ab3`
(SHA-256 `52cbad16e76d9a03f2a95856c67f331577909eed59e56f7e3842ebe95f07ced2`).

- The focused skip-list gate passed: 1 passed, 0 failed.
- Fresh `standalone.sh` boots reached `WFLYSRV0026` twice in `--nojit`
  (15.015 s and 19.719 s) and twice under default JIT (46.322 s and
  30.069 s).
- Every boot started 275/522 services. The same three failed/missing services
  in both modes stem from the separately tracked
  `SecureRandom.getProvider() == null` Elytron follow-on, not controller JIT.
- The traced JIT boot emitted 969 `full-compile org/jboss/as/controller/`
  records, including `OperationContextImpl` and `AbstractOperationContext`.
  None of the four boots logged `controllerOperations` null,
  `java/lang/Object.hasNext/next`, or `WFLYCTL0079`.
- `WildflyControllerIteratorProbe` (100,000 CompositeIterable/String.join
  rounds) passed in both `--nojit` and JIT (`CRATONVM_JIT_THRESHOLD=1`) modes.

This is direct full-fixture evidence that the original invokespecial
constructor failure is gone with the controller package JIT-enabled.



## What it banned

`org/jboss/as/controller/` (blanket package prefix).

## Original symptom (2026-07-13)

The optimized `invokespecial` path skipped
`AbstractOperationContext.<init>` while constructing
`OperationContextImpl`. Its `controllerOperations` list remained null and
parallel EJB boot failed. The same standalone WildFly boot reaches past
that point with `CRATONVM_DISABLE_JIT=1` — a JIT-only miscompile, not a
native gap.
