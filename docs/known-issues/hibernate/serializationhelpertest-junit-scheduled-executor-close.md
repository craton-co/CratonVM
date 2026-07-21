# Hibernate SerializationHelperTest executor-close diagnostic

**Status:** OPEN, separate from the fixed explicit-null Class.forName loader-scope bug.

## Symptom

When the isolated Hibernate runner executes org.hibernate.orm.test.util.SerializationHelperTest on CratonVM, both test methods pass (found=2, started=2, ok=2, failed=0) but the Jupiter engine then emits:

    org.junit.platform.commons.JUnitException: Failed to close extension context
    Caused by: org.junit.platform.commons.JUnitException:
    Scheduled executor could not be stopped in an orderly manner

The same command on HotSpot completes with no container-close exception. It reproduces in normal JIT and --nojit modes, so it is not caused by the JIT nor by the repaired Class.forName routing.

## Scope

The exception originates in JUnit 6 TimeoutInvocationFactory ExecutorResource.close after the test bodies already passed. The executor lifecycle is in the general real ScheduledThreadPoolExecutor / ExecutorService shutdown and awaitTermination path, not SerializationHelper or class-loader scope. This document records it separately so the fixed loader bug is not conflated with an independent executor shutdown residual.

## Reproduction

Use /data/data/apps/hibernate-orm-harness/hib-suite-runner, common.args, and the one-line list file containing org.hibernate.orm.test.util.SerializationHelperTest. Add -Dcraton.trace=1 to obtain the closing stack trace. Compare with /home/victor/jdk25/bin/java using the same arguments.
