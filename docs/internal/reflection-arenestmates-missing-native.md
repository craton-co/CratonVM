# Missing native: `jdk.internal.reflect.Reflection.areNestMates`

| | |
|---|---|
| **Status** | OPEN, found 2026-07-07 during a Spring suite non-passed rerun on Azure (dev, real-JDK, JIT on). |
| **Area** | Missing JDK-internal native registration. |

## Symptom

```
java.lang.UnsatisfiedLinkError: jdk/internal/reflect/Reflection.areNestMates(Ljava/lang/Class;Ljava/lang/Class;)Z
```

Found in `org.springframework.beans.factory.DefaultListableBeanFactoryTests.beanProviderSerialization()`.
HotSpot passes. This is a plain missing-native gap, not a dispatch/correctness
bug: `Reflection.areNestMates` has no CratonVM implementation registered at
all, so any code path (reflection/serialization/access-check machinery
checking whether two classes share a nest, for private-member access) that
calls it fails outright.

## Initial read

Register a native for `jdk/internal/reflect/Reflection.areNestMates`
alongside CratonVM's other `jdk.internal.reflect.Reflection.*` natives (grep
`native-builtins` for existing `Reflection.` registrations to match the
pattern/module). Real JVMS semantics: two classes are nestmates iff they
declare the same `NestHost` (a class is its own nest's host if it has no
`NestHost` attribute but does have `NestMembers`, or is neither). CratonVM's
class-loading model should already track `NestHost`/`NestMembers` class-file
attributes for other purposes (e.g. private-member access checks) — this
native likely just needs to expose that existing data through the reflective
API rather than needing new nest-tracking infrastructure.

## Reproduction

Azure host suite runner (path depths on this host shift between `/data/data/`
and `/data/data/data/` — verify with `ls -d` at both before trusting either):

```bash
WT=/data/data/wt-osr-nonpassed-20260706-1945   # prebuilt Spring suite + frozen binary
cd $WT/apps/spring-suite-runner
echo org.springframework.beans.factory.DefaultListableBeanFactoryTests > /tmp/list.txt
SF=$WT/apps/spring-framework RUNNER=$WT/apps/spring-suite-runner \
  CRATONVM_BIN=$WT/cratonvm-osr-nonpassed-20260706.bin JH=/data/data/jdk25-real \
  BATCH=1 BATCH_TO=120 ONE_TO=120 LIST=/tmp/list.txt OUT=/tmp/out SHARD_N=1 SHARD_ID=0 \
  bash suite-run.sh
# see /tmp/out/failcauses.log and /tmp/out/raw.log
```

Note: this test class also has other, unrelated `BeanDefinitionStoreException`
failures caused by classpath-dump gaps in the suite harness (missing
cross-module Spring test-fixture jars) — ignore those, they're environmental,
not a CratonVM bug. Only `beanProviderSerialization()`'s `UnsatisfiedLinkError`
is this issue.
