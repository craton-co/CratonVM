# RealmModelTest protobuf metadata cache configuration missing - FIXED

Status: FIXED (2026-07-08). Moved out of `../../../known-issues` per the
known-issues triage rule.

Date observed: 2026-07-08, while verifying the fixed
[`keycloak-model-stw-takeover-hang-eventloopgroup-shutdown`](keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md)
residual with the real Keycloak suite runner.

## Symptom

`RealmModelTest` no longer hung during Netty `EventLoopGroup` shutdown under
CratonVM `--nojit`. The same real runner then failed class initialization with:

```text
java.lang.ExceptionInInitializerError
Caused by: org.infinispan.commons.CacheConfigurationException:
ISPN000436: Cache '___protobuf_metadata' has been requested, but no matching cache configuration exists
```

HotSpot `-Xint` passed the same class/list in 142.649s.

## Root cause

The CratonVM Infinispan `DefaultCacheManager` natives still treated some real
manager calls as if they were synthetic-manager calls. In particular, the real
internal cache registry registered `___protobuf_metadata`, but CratonVM did not
delegate the real-manager configuration path back into Infinispan's real
`ConfigurationManager`. Later, `DefaultCacheManager.wireAndStartCache()` asked
for `___protobuf_metadata` and the real configuration manager had no matching
entry.

The durable fix is to detect real `DefaultCacheManager` receivers and delegate:

- `cacheExists(String)` through real `ConfigurationManager.selectCache(String)`
  and the manager's real `caches.containsKey(Object)` map.
- `defineConfiguration(String, Configuration)` through a real
  `ConfigurationBuilder.read(Configuration)`, `template(boolean)`, and
  `ConfigurationManager.putConfiguration(String, ConfigurationBuilder)` call.

## Follow-on residuals fixed in the same chain

Fixing the protobuf metadata cache exposed several independent CratonVM
residuals before the class could reach Liquibase XML parsing:

- `java/lang/foreign/SymbolLookup.find(String)` fell through to an abstract real
  interface declaration. The real-JDK essential registry now includes the
  Panama symbol-lookup bridge and the VM force-native gates route `find` to it.
- The default FFM lookup returned empty for C runtime symbols used by Infinispan
  off-heap memory setup. Default native lookup now checks already-loaded C
  runtime libraries on Windows.
- Real `StampedLock$WriteLockView.unlock()` reached
  `StampedLock.unstampedUnlockWrite()` bytecode and threw
  `IllegalMonitorStateException`. The private unstamped helpers are registered
  and forced to the existing native lock state.
- Liquibase command pipeline ordering was reversed for equal-order entries
  because native `TreeMap`/`TreeSet` compared `existing` against `new`. JDK
  `TreeMap` compares `new` against `existing`; the native comparator orientation
  now matches JDK behavior.
- Xerces `CMStateSet.hashCode`/`equals` dominated the next XSD DFA build
  timeout. Those methods now have real-JDK native fast paths with Java signed
  byte and wrapping-int semantics.

## Verification

Focused regression coverage:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-protobuf-metadata-20260708-001'
cargo test -p cratonvm-native-builtins t19_10 -- --nocapture
cargo test -p cratonvm-native-builtins xerces_cmstateset -- --nocapture
cargo test -p cratonvm-native-collections --test mock_treemap -- --nocapture
cargo test -p cratonvm-vm xerces_cmstateset_force_native_covers_hash_and_equals_hotspots -- --nocapture
```

Observed results included:

- `t19_10`: 23/23 passed.
- `xerces_cmstateset`: 3/3 passed.
- `mock_treemap`: 5/5 passed.
- VM force-native `xerces_cmstateset` test: passed.

Real Keycloak verification used the unique binary:

```text
C:\craton\cargo-targets\keycloak-protobuf-metadata-20260708-001\release\cratonvm-keycloak-protobuf-metadata-20260708-001.exe
```

The latest suite-runner run:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -Vm craton -Category others -Jit off `
  -ClassList 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\keycloak-model-realm-stw-20260708-001.tsv' `
  -RunName 'keycloak-protobuf-cmstateset-verify-20260708-001' `
  -Parallel 1 -TimeoutSec 900 `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' `
  -WorkDir 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite' `
  -Exe 'C:\craton\cargo-targets\keycloak-protobuf-metadata-20260708-001\release\cratonvm-keycloak-protobuf-metadata-20260708-001.exe'
```

Result: `HANG` at 900s, but the fixed signatures are gone:

- No `ISPN000436`.
- No `___protobuf_metadata` cache-configuration failure.
- No `SymbolLookup.find` `AbstractMethodError`.
- No FFM `NoSuchElementException`.
- No StampedLock `IllegalMonitorStateException`.
- No `ISPN000659` state-transfer failure.
- No Liquibase `ConcurrentHashMap does not permit null keys`.

The run now reaches Liquibase changelog parsing/checksum work. The new open
residual is tracked in:

[`keycloak-model-liquibase-xerces-xml-parse-nojit-timeout.md`](../../known-issues/keycloak-model-liquibase-xerces-xml-parse-nojit-timeout.md).
