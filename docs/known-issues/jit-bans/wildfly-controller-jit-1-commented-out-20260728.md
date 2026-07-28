# WILDFLY-CONTROLLER-JIT.1 — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision — only `tomcat`/`hibernate`/`spring`/`spring-boot`/`h2` need
to work right now; WildFly is outside that scope.

## What it banned

`org/jboss/as/controller/` (blanket package prefix).

## Original symptom (2026-07-13)

The optimized `invokespecial` path skipped
`AbstractOperationContext.<init>` while constructing
`OperationContextImpl`. Its `controllerOperations` list remained null and
parallel EJB boot failed. The same standalone WildFly boot reaches past
that point with `CRATONVM_DISABLE_JIT=1` — a JIT-only miscompile, not a
native gap.

## Why it was never re-verified before being commented out

WildFly is not one of the 5 apps this session narrowed scope to. No
fixture was re-run against this ban before commenting it out — the
constructor-skip defect it guards against may still be live.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the `org/jboss/as/controller/`
guard block (search for `WILDFLY-CONTROLLER-JIT.1`) inside
`should_skip_jit_internal`.

## Repro (for whoever re-verifies)

A real WildFly `standalone.sh` boot under default JIT settings, watching
for `AbstractOperationContext`/`OperationContextImpl` construction
failures during parallel EJB deployment.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section) for the full list of sibling
removals from the same pass.
