# WildFly `WFLYCTL0079`: duplicate transaction attribute registration

Status: **OPEN, separated from the stale-reference family on 2026-07-30.**

## Symptom

Very rarely, WildFly 32 parallel extension boot aborts while installing the
transactions subsystem:

```text
WFLYCTL0079 / WFLYCTL0043:
An attribute named 'hornetq-store-enable-async-io' is already registered at
location '/subsystem=transactions'
```

The historical campaigns observed two matching failures across roughly 4,800
boots. Neither carried the stale-receiver, pointer-map, zeroed-from-space, or
checkcast evidence of the GC family in the retired
`docs/internal/fixed-suite-bugs/wildfly/wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md`
report.

## What has been excluded

Two CratonVM double-execution hypotheses were instrumented:

- `ParallelExtensionAddHandler$ExtensionInitializeTask.call()` was traced by
  receiver and thread. The expected compiler bridge plus covariant method
  produces two entries; no receiver produced a third entry in 2,400 traced
  boots.
- `TransactionSubsystemRootResourceDefinition.registerAttributes()` was
  traced by receiver and registry identity. No identity pair was invoked
  twice in an 800-boot focused campaign.

Those negative results exclude duplicate executor dispatch and duplicate entry
to the subsystem registration method in the sampled runs. They do not prove
that the rare failure is a CratonVM defect, and they do not justify marking it
fixed.

## Next useful discriminator

Instrument the construction and registration performed by the transactions
subsystem's `AliasedHandler` one level below `registerAttributes`, or audit
that code for a non-atomic check/register path. A new reproduction should
record:

- receiver and registry identities at that exact call,
- class-initialization entry/exit count for the declaring class,
- `DUPCALL3X` and `DUPREG2X` output,
- the full WildFly management-operation stack.

Until that evidence exists, this remains a separate low-rate known issue and
must not be used as evidence that the retired GC/native-handle family is still
open.
