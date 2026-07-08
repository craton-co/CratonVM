# `LinkedCaseInsensitiveMap` deserialization loses synthetic outer reference (`this$0`)

| | |
|---|---|
| **Status** | OPEN, found 2026-07-07 during a Spring suite non-passed rerun on Azure (dev, real-JDK, JIT on). |
| **Area** | Java serialization — inner (non-static) class instances don't get their synthetic `this$0` outer reference restored on deserialize. |
| **Related, but distinct** | [`DomainParameterXref` LinkedHashMap removeEldestEntry dispatch-to-Object NSME (FIXED 2026-07-08)](../internal/fixed-suite-bugs/hib-domainparameterxref-lhm-removeeldestentry-nsme-FIXED.md) — that fix guards against a *wrong-receiver* dispatch (`Object.removeEldestEntry`, a `NoSuchMethodError`). This bug is different: dispatch correctly resolves to `LinkedCaseInsensitiveMap`'s own `removeEldestEntry` override, but the synthetic `this$0` field inside that instance is null (a `NullPointerException`), specific to a deserialize path. Confirmed NOT fixed by the 2026-07-08 change (reproduced after that fix landed). |

## Symptom

```
java.lang.NullPointerException: Cannot invoke "org.springframework.util.LinkedCaseInsensitiveMap.removeEldestEntry(java.util.Map$Entry)" because "this.this$0" is null
```

Both `org.springframework.util.MimeTypeTests.serialize()` and
`org.springframework.http.MediaTypeTests.serialize()` fail identically.
HotSpot passes both.

`LinkedCaseInsensitiveMap` has a non-static inner class extending
`LinkedHashMap` that overrides `removeEldestEntry`; the inner class holds a
compiler-synthesized `this$0` field pointing back to the enclosing
`LinkedCaseInsensitiveMap` instance. `removeEldestEntry` is invoked
internally by `LinkedHashMap`'s own `put`/`afterNodeInsertion` path. After
CratonVM deserializes the object graph, `this$0` is null on the inner map
instance, so the internal `removeEldestEntry` call NPEs.

## Initial read

This is a serialization-reconstruction bug, not a dispatch bug: whatever
CratonVM's `ObjectInputStream`/deserialization path does to rebuild a
non-static inner class instance isn't restoring the compiler-synthesized
`this$0` field the way it restores ordinary declared fields. Worth checking:
- how deserialization enumerates/restores fields for a class with a
  synthetic `this$0` (is it treated as a normal serializable field, or
  skipped because it's synthetic/not declared in source?),
  and whether this is specific to `LinkedHashMap` subclasses or a general
  inner-class-deserialization gap (test with a simpler synthetic repro of a
  serializable outer class holding a serializable non-static inner class).

## Reproduction

Azure host suite runner (path depths on this host shift between `/data/data/`
and `/data/data/data/` — verify with `ls -d` at both before trusting either):

```bash
WT=/data/data/wt-osr-nonpassed-20260706-1945   # prebuilt Spring suite + frozen binary
cd $WT/apps/spring-suite-runner
printf 'org.springframework.util.MimeTypeTests\norg.springframework.http.MediaTypeTests\n' > /tmp/list.txt
SF=$WT/apps/spring-framework RUNNER=$WT/apps/spring-suite-runner \
  CRATONVM_BIN=$WT/cratonvm-osr-nonpassed-20260706.bin JH=/data/data/jdk25-real \
  BATCH=1 BATCH_TO=120 ONE_TO=120 LIST=/tmp/list.txt OUT=/tmp/out SHARD_N=1 SHARD_ID=0 \
  bash suite-run.sh
# see /tmp/out/failcauses.log and /tmp/out/raw.log
```
