# Keycloak RealmModelTest timeout after Infinispan bootstrap reaches Liquibase

Status: open

Date observed: 2026-07-08

## Summary

After the stale Infinispan ProtoStream `FileDescriptor.fullName` decode-error
note was retired, `RealmModelTest` now gets substantially further:

- ProtoStream bootstrap no longer fails with the impossible
  `fullName` pc=51 decode error;
- the real `DefaultCacheManager.defineConfiguration` path no longer loses the
  `___protobuf_metadata` cache configuration;
- the real-JDK `StampedLock$WriteLockView.unlock()` path no longer throws
  `IllegalMonitorStateException`;
- with RxJava3 interpreted under the conservative JIT policy, Infinispan's
  reactive publisher request advances through `node-1#6` instead of stalling
  at `node-1#2`.

The class still does not finish under CratonVM within the current watchdog.
The current residual is later: Keycloak reaches Liquibase changelog parsing
and remains much slower than HotSpot.

Update 2026-07-09: the specific no-JIT Liquibase/Xerces XML parse hotspot has
been fixed and retired to
`docs/internal/fixed-suite-bugs/keycloak-model-liquibase-xerces-xml-parse-nojit-timeout-FIXED.md`.
The active remaining layer is now
`docs/known-issues/keycloak-model-liquibase-checksum-status-nojit-timeout.md`,
where the main thread is in Liquibase checksum/status serialization rather than
Xerces schema parsing.

## Evidence

HotSpot control:

- `verify-fullname-hotspot-20260708-001`: PASS in about 36 seconds.

CratonVM controls:

- `verify-fullname-jiton-after11-20260708-001`: HANG at 600 seconds before
  the RxJava3 skip-list mitigation; last meaningful progress was the
  Infinispan distributed-stream publisher around `node-1#2`.
- `verify-fullname-deny-rxjavaonly-20260708-001`: HANG at 450 seconds, but it
  completed the publisher response for `node-1#6` and reached Liquibase
  parsing. It recorded 11 `ChangeLogParserFactory Matched file ...` lines,
  with the last observed file `META-INF/jpa-changelog-1.3.0.xml`.
- `verify-fullname-compiled-rxskip-20260708-001`: HANG at 450 seconds with
  the RxJava3 skip-list entry compiled into the unique binary. It reached
  Liquibase parsing and recorded 3 `ChangeLogParserFactory Matched file ...`
  lines, with the last observed file
  `META-INF/jpa-changelog-1.0.0.Final-db2.xml`.
- `verify-fullname-nojit-after11-20260708-001`: HANG at 900 seconds, but got
  further into Liquibase than the RxJava3-denied JIT run. It recorded about
  150 matched changelog files and reached the Keycloak 26.x changelog range.

No current CratonVM run in this pass reproduced the historical `fullName`
decode error, `ISPN000436`, `ISPN000659`, or the `StampedLock`
`IllegalMonitorStateException` after the corresponding fixes.

## What is fixed in the runtime

The committed runtime mitigation keeps RxJava3 interpreted under
`SkipPolicy::Conservative`, because direct suite controls showed:

```text
CRATONVM_JIT_DENY=io/reactivex/
```

is sufficient to move the run past the Infinispan publisher wait and into
Liquibase. The skip is deliberately narrow:

- it does not blanket-ban `org/infinispan/`;
- it does not blanket-ban Netty or JGroups;
- it does not require skipping `org/reactivestreams/`;
- it remains liftable with `CRATONVM_JIT_ALLOW_PACKAGES=io/reactivex/`.

## Not yet root-caused / fixed

The remaining timeout needs a fresh Liquibase checksum/status investigation.
Useful next steps:

1. Instrument `StringChangeLogSerializer.serializeObject` and
   `AbstractChange.generateCheckSum` under `--nojit`.
2. Compare the checksum/status path against HotSpot `-Xint` on the same
   class/list.
3. If JIT-on with the RxJava3 skip-list reaches the same layer, decide whether
   the shared no-JIT/JIT-denied cost is reflection/property traversal, repeated
   checksum work, string serialization, or another Liquibase visitor hot path.

## Repro

Use the single-class Keycloak model list with the unique CratonVM binary:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Vm craton `
  -Jit on `
  -Parallel 1 `
  -TimeoutSec 900 `
  -RunName verify-realmmodel-liquibase-timeout `
  -ClassList apps\keycloak-suite-runner\.suite\realm-model-test.tsv `
  -KeycloakRoot C:\craton\CratonVM\apps\keycloak `
  -WorkDir apps\keycloak-suite-runner\.suite `
  -Exe target\release\cratonvm-keycloak-infinispan-fullname-20260708-001.exe `
  -JdkHome "C:\Program Files\Java\jdk-25"
```
