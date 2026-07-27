# WildFly datasource/module-loader and Infinispan capability ServiceName null residuals

Status: FIXED / CLOSED -- fixed 2026-07-08 on branch `codex/fix-wildfly-null-residuals-20260708-175605`.

## Symptom

After the `ContextNames$BindInfo` fix, the direct WildFly 32.0.1.Final `standalone.sh` probe advanced further into runtime subsystem boot and reported two functional failures before the already-known STW timeout:

```text
WFLYCTL0158: Operation handler failed: java.lang.NullPointerException: Cannot invoke "org.jboss.modules.ModuleLoader.loadModule(org.jboss.modules.ModuleIdentifier)"
WFLYCTL0158: Operation handler failed: java.lang.NullPointerException: Method parameter cannot be null
```

The first occurred in `JdbcDriverAdd.performRuntime`; the second occurred in `ServiceSupplierDependency.register` -> `ServiceBuilderImpl.requires` during Infinispan cache service installation.

## Root Cause

Two separate bridge gaps were exposed:

1. `JdbcDriverAdd` calls `org.jboss.modules.Module.getCallerModuleLoader()` and immediately invokes `loadModule(ModuleIdentifier)` on the result. CratonVM already had `LocalModuleLoader.loadModule(...)` natives, but did not provide `Module.getCallerModuleLoader()` / `Module.getBootModuleLoader()`, so the real JBoss Modules caller-loader path could produce a null receiver.
2. `XAResourceRecoveryServiceConfigurator.configure()` calls `OperationContext.getCapabilityServiceName(name, type)` for `org.wildfly.transactions.xa-resource-recovery-registry`, stores that in a `ServiceSupplierDependency`, and later passes it to `ServiceBuilder.requires(...)`. WildFly's real `OperationContextImpl` has a fallback to `ServiceNameFactory.parseServiceName(capabilityName)` when the capability registry cannot resolve a registry-backed capability. Under CratonVM, the under-modeled registry path could return null instead of throwing the exception that triggers that fallback.

## Fix

`../../../../native-builtins/src/jboss_module_loader.rs` now registers `Module.getBootModuleLoader()` and `Module.getCallerModuleLoader()` and returns the existing boot `LocalModuleLoader`, which is the loader CratonVM uses for WildFly module-path resolution.

`../../../../native-builtins/src/wildfly_core.rs` now registers the `OperationContext.getCapabilityServiceName(...)` overloads used by WildFly boot and mirrors the real fallback by parsing the capability name into an MSC `ServiceName`, appending dynamic parts for the overloads that carry them.

## Verification

```text
cargo test -p cratonvm-native-builtins t19_h4_get_caller_module_loader_returns_boot_loader -- --nocapture
cargo test -p cratonvm-native-builtins t19_2_a_operation_context_capability_name -- --nocapture
cargo check -p cratonvm-native-api -p cratonvm-native-builtins -p cratonvm-vm
cargo build --release -p cratonvm-cli --features java-bin-alias --bin cratonvm --bin java
```

Probe binaries:

```text
/data/data/cratonvm-builtins/cratonvm-wildfly-20260708-175605-nullres
/data/data/cratonvm-builtins/java-wildfly-20260708-175605-nullres
/data/data/fakejdk-wildfly-20260708-175605-nullres/bin/java
```

The follow-up WildFly standalone probe (`/tmp/wf-20260708-175605-nullres.log`) no longer reports either null residual. It still times out in the broader STW/cooperation residual tracked by `docs/known-issues/wildfly-domain-managed-servers-timeout.md`.
