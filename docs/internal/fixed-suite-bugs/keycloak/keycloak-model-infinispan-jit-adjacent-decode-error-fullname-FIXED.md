# Keycloak RealmModelTest Infinispan `FileDescriptor.fullName` decode error - FIXED/retired

Status: fixed / retired

Retired: 2026-07-08

Original status: `docs/known-issues/keycloak-model-infinispan-jit-adjacent-decode-error-fullname.md`
was opened for a default-JIT crash during Infinispan ProtoStream bootstrap:

```text
internal error: decode error at pc=51 in fullName.(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;: unexpected end of data at position 51
```

`org.infinispan.protostream.descriptors.FileDescriptor.fullName(String, String)`
is a tiny method whose bytecode ends at pc=15, so the historical symptom
looked like a JIT-to-interpreter frame/pc corruption in a compiled caller.

## Retirement result

The exact `fullName` decode error no longer reproduces on current `dev` as of
2026-07-08. Rerunning `RealmModelTest` with the same Keycloak/Infinispan
classpath now advances through the ProtoStream `fullName` path and exposes
later, separate runtime/native layers instead.

This pass fixed the adjacent residuals that surfaced after the stale decode
error was gone:

- real `DefaultCacheManager.defineConfiguration` now delegates to the real
  `ConfigurationManager.putConfiguration(...)`, so Infinispan's
  `___protobuf_metadata` cache configuration is retained instead of failing
  with `ISPN000436`;
- real-JDK `StampedLock` and its lock-view classes now use one coherent
  side-table/native implementation, including `unstampedUnlock*`,
  `tryUnlock*`, and `ReadLockView`/`WriteLockView` entry points, eliminating
  the follow-on `IllegalMonitorStateException`;
- essential native registration now includes the Windows PE symbol lookup
  bridge, and process-native symbol lookup can resolve C runtime symbols such
  as `malloc`/`free`;
- default conservative JIT now keeps RxJava3 interpreted after a focused
  `CRATONVM_JIT_DENY=io/reactivex/` probe showed that this clears the
  Infinispan publisher wait and advances into Liquibase parsing.

## Validation

Focused Rust coverage added or rerun:

- `cargo test -p cratonvm-vm default_process_lookup_finds_c_runtime_allocator_symbols --lib`
- `cargo test -p cratonvm-vm stamped_lock_force_native_covers_registered_surface --lib`
- `cargo test -p cratonvm-native-builtins register_essential_includes_symbol_lookup_bridge --lib`
- `cargo test -p cratonvm-native-builtins t19_10_define_configuration --lib`
- `cargo test -p cratonvm-native-builtins infinispan_local --lib`
- `cargo test -p cratonvm-native-builtins m18_stamped_unstamped --lib`
- `cargo test -p cratonvm-native-builtins m18_stamped --lib`

Keycloak probes:

- default JIT after the native fixes no longer reports the historical
  `fullName` decode error;
- `verify-fullname-jiton-after11-20260708-001` reached the Infinispan
  reactive publisher wait instead of failing in ProtoStream, cache
  configuration, or `StampedLock`;
- `verify-fullname-deny-rxjavaonly-20260708-001` completed the publisher
  request through `node-1#6` and reached Liquibase parsing before the watchdog.
- `verify-fullname-compiled-rxskip-20260708-001` used the rebuilt unique
  binary with the RxJava3 skip-list entry compiled in; it also reached
  Liquibase parsing and showed no `fullName`, `ISPN000436`, `ISPN000659`,
  `IllegalMonitorStateException`, or GC breadcrumb signatures.

The remaining later timeout is not this bug. It is tracked separately in
[`keycloak-model-realmmodeltest-post-infinispan-liquibase-timeout-FIXED.md`](keycloak-model-realmmodeltest-post-infinispan-liquibase-timeout-FIXED.md).
