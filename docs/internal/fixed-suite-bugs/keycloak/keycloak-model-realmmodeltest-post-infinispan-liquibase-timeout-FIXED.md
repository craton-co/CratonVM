# Keycloak RealmModelTest timeout after Infinispan bootstrap reaches Liquibase

Status: fixed/retired

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

The class still does not pass under CratonVM. The current residual is later:
Keycloak now reaches and completes Liquibase changelog execution, then fails in
Hibernate/H2 connection bootstrap.

Update 2026-07-09: the specific no-JIT Liquibase/Xerces XML parse hotspot has
been fixed and retired to
`keycloak-model-liquibase-xerces-xml-parse-nojit-timeout-FIXED.md`.
The 2026-07-09 `scanQName` follow-up also cleared the intermediate
`XMLEntityScanner.skipString` empty-rawname failure and H2 `BitSet.clone`
failure. The 2026-07-09 checksum/status follow-up then moved the class through
all 195 Liquibase changesets and retired the Liquibase timeout to
`keycloak-model-liquibase-checksum-status-nojit-timeout-FIXED.md`.
The current terminal residual is now
`keycloak-model-realmmodeltest-h2-auth-after-liquibase-nojit-FIXED.md`:
Hibernate bootstrap of `JdbcEnvironment` fails through H2 with
`Wrong user name or password [28000-240]`.

## Fixed evidence

Retired on 2026-07-10 after the focused Keycloak model class passed on the Azure host from the isolated worktree branch `codex/fix-keycloak-realmmodel-h2-auth-20260709-123713`.

Validation binary:

```text
/data/data/cargo-targets/keycloak-realmmodel-h2-auth-20260709-123713-baseline/release/cratonvm-keycloak-realmmodel-h2-auth-20260709-123713-fixed
```

Runner result:

```text
run: verify-realmmodel-h2-auth-fixed-jdk25-20260709-123713-r109-final-candidate-700
mode: all-nojit
status: PASS
seconds: 352.304
tests: 3
failed: 0
```

The final run no longer reproduced the tracked `Wrong user name or password [28000-240]` H2 bootstrap failure, the post-Liquibase timeout, the intermediate H2 `Command` cast failure, or the `java.util.Map.forEach` localization NPE. The relevant runtime fixes are the H2 `SessionLocal.prepareLocal` no-cache bridge plus the receiver-aware `Map.forEach` path for Hibernate `PersistentMap` backed by arbitrary map implementations.

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
  with the last observed file `../../../../apps/META-INF/jpa-changelog-1.3.0.xml`.
- `verify-fullname-compiled-rxskip-20260708-001`: HANG at 450 seconds with
  the RxJava3 skip-list entry compiled into the unique binary. It reached
  Liquibase parsing and recorded 3 `ChangeLogParserFactory Matched file ...`
  lines, with the last observed file
  `../../../../apps/META-INF/jpa-changelog-1.0.0.Final-db2.xml`.
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

The remaining failure needs a fresh Hibernate/H2 credential propagation
investigation.
Useful next steps:

1. Instrument H2 `Driver.connect` or Hibernate
   `DriverConnectionCreator.makeConnection` to log the URL/user/password shape
   for the successful Liquibase connection and the later failing Hibernate
   bootstrap connection in the same VM.
2. Compare with HotSpot `-Xint` using the same runner class list.
3. Use `docs/internal/hibernate-bugs/HIB-CV-06-emf-bootstrap-db-connect.md` as
   prior art for `Properties` credential propagation, but re-prove the
   Keycloak path before assuming it is the same bug.

## Repro

Use the single-class Keycloak model list with the unique CratonVM binary:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Vm craton `
  -Jit off `
  -Parallel 1 `
  -TimeoutSec 900 `
  -RunName verify-realmmodel-h2-auth-after-liquibase `
  -ClassList apps\keycloak-suite-runner\.suite\keycloak-model-realm-stw-20260708-001.tsv `
  -KeycloakRoot C:\craton\CratonVM\apps\keycloak `
  -WorkDir apps\keycloak-suite-runner\.suite `
  -Exe target\release\cratonvm-keycloak-liquibase-columnconfig-20260709-012.exe `
  -JdkHome "C:\Program Files\Java\jdk-25"
```
