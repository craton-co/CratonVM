# WildFly `ContextNames$BindInfo.getBinderServiceName` returned a `String` instead of `ServiceName`

Status: FIXED / CLOSED — fixed 2026-07-08 on branch `codex/fix-wildfly-stw-park-20260708-165032`.

## Symptom

After the LogManager bootstrap fix, the direct WildFly 32.0.1.Final `standalone.sh` probe advanced into datasource subsystem boot, then failed with:

```text
NoSuchMethodError method="java/lang/String.getCanonicalName()Ljava/lang/String;"
caller="org/jboss/as/connector/subsystems/datasources/AbstractDataSourceService.getServiceName(...) @pc=16"
```

The real bytecode invokes `ContextNames$BindInfo.getBinderServiceName()` and then `ServiceName.getCanonicalName()`. CratonVM's synthetic `BindInfo` object had stored the binder service's canonical text as a `String`, so virtual dispatch correctly saw a `String` receiver and failed on the impossible `String.getCanonicalName()` method.

## Root Cause

`../../../../native-builtins/src/wildfly_naming.rs` allocated `ContextNames$BindInfo` with only two synthetic slots and populated them as strings. The real WildFly class layout is:

```text
parentContextServiceName : org.jboss.msc.service.ServiceName
binderServiceName       : org.jboss.msc.service.ServiceName
bindName                : java.lang.String
absoluteJndiName        : java.lang.String
```

Returning a `String` from the `binderServiceName` field broke callers that use the real `ServiceName` API.

## Fix

The naming bridge now constructs real synthetic `ServiceName` mirrors for both `parentContextServiceName` and `binderServiceName`, stores all four real fields by name, and splits `java:jboss/...` style names into the same parent/bind-name shape WildFly expects.

## Verification

```text
cargo test -p cratonvm-native-builtins t19_2_b_context_names_bind_info_parses_absolute_name -- --nocapture
cargo test -p cratonvm-native-builtins t19_2_b_service_based_naming_store_registers_msc_service -- --nocapture
cargo check -p cratonvm-native-api -p cratonvm-native-builtins -p cratonvm-vm
cargo build --release -p cratonvm-cli --features java-bin-alias --bin cratonvm --bin java
```

The follow-up WildFly standalone probe no longer reports `String.getCanonicalName` and reaches later datasource/clustering failures. Those later failures remain separate open residuals in `docs/known-issues/wildfly-domain-managed-servers-timeout.md`.
