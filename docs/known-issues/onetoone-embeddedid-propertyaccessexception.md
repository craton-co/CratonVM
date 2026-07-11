# `OneToOneJoinColumnsEmbeddedIdTest` — `PropertyAccessException` on embedded-id setter (real bug, unmasked)

Status: OPEN — not investigated further this session (out of scope; see below)

Found: 2026-07-11, verifying the
[uncaught-exception-misattribution fix](../internal/fixed-suite-bugs/uncaught-exception-misattribution-native-pending-return-FIXED.md).
Before that fix, `org.hibernate.orm.test.onetoone.embeddedid.OneToOneJoinColumnsEmbeddedIdTest`
crashed the whole process (`rc=1`) with a misattributed `java/lang/Thread`/
`org/antlr/v4/runtime/CommonToken` "exception" — the crash masked whatever
Java-level failure was actually happening. With the fix, the class now runs
to completion instead of crashing, revealing a genuine, pre-existing test
failure.

## Symptom

```
found=6 started=6 ok=3 failed=3 aborted=0 skipped=0

@@FAIL … :: org.hibernate.PropertyAccessException: Could not set value of type
  [OneToOneJoinColumnsEmbeddedIdTest$EntityBKey]:
  'OneToOneJoinColumnsEmbeddedIdTest$EntityB.entityBKey' (setter)
@@FAIL … :: org.hibernate.PropertyAccessException: Could not set value of type
  [OneToOneJoinColumnsEmbeddedIdTest$EntityAKey]:
  'OneToOneJoinColumnsEmbeddedIdTest$EntityA.entityAKey' (setter)
@@FAIL … :: org.hibernate.PropertyAccessException: Could not set value of type
  [OneToOneJoinColumnsEmbeddedIdTest$EntityBKey]:
  'OneToOneJoinColumnsEmbeddedIdTest$EntityB.entityBKey' (setter)
```

3 of 6 tests fail this way; 3 pass. Reproducer: run
`org.hibernate.orm.test.onetoone.embeddedid.OneToOneJoinColumnsEmbeddedIdTest`
under CratonVM (real-JDK mode, JIT on) via the standard Hibernate ORM JUnit5
harness (`/data/hibpkg/runner` on the Azure host, or any equivalent
CratonRunner-based setup).

## Root cause

Not investigated this session — the failure smells like a reflective
property-setter/embedded-id-composite-key access path issue (Hibernate's
`PropertyAccessException` wraps a reflective `Method.invoke`/field-set
failure on the entity's embedded-id setter), but no stack trace was captured
(`-Dcraton.trace=1` wasn't set on this run) and no further triage was done —
out of scope for the exception-reporting fix this was found alongside. A
`-Dcraton.trace=1` rerun would surface the wrapped cause and is the natural
next step.

## Severity

Non-crashing, isolated to embedded-id one-to-one join-column mapping tests.
