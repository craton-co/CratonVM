# WildFly standalone BindInfo and collector residuals

Status: FIXED on 2026-07-08 in branch
`codex/fix-wildfly-hib-residuals-20260708-164823`.

## Symptoms

Two standalone WildFly residuals appeared after the earlier HIB-CV-32 guard stopped
firing:

```text
NoSuchMethodError: java/lang/String.getCanonicalName()Ljava/lang/String;
```

from datasource service-name construction, and later:

```text
NoSuchMethodError: java/lang/Object.supplier()Ljava/util/function/Supplier;
```

from the stream collector path used during WildFly boot.

## Fixes

`../../../../native-builtins/src/wildfly_naming.rs` now mirrors WildFly's real
`ContextNames$BindInfo` shape: parent `ServiceName`, binder `ServiceName`, stripped
bind name, and absolute JNDI name. `ContextNames.bindInfoFor("java:jboss/datasources/KeycloakDS")`
therefore returns `java.jboss` as the parent service and
`java.jboss.datasources.KeycloakDS` as the binder service, instead of placing strings in
the service-name fields.

`../../../../native-builtins/src/phases_late.rs` now keeps `Collectors.toUnmodifiableList()` and
`toUnmodifiableSet()` compatible with the core `native-collections` collector engine:
tags `1`/`2` and four collector fields. This prevents phase 56 from replacing a valid
collector with a three-slot helper whose class identity can degrade to `java/lang/Object`.

`../../../../native-builtins/src/lib.rs` also hardens `alloc_concurrent_synthetic`: if real-class
initialization reports success but resolves to a different class id, it preserves the
requested synthetic helper class instead of allocating an object whose header names the
wrong class layout.

## Verification

Built and copied a unique probe binary:

```text
/data/data/probes/wildfly-hib-residuals-20260708-164823/bin/cratonvm-wildfly-hib-residuals-20260708-164823
/data/data/probes/wildfly-hib-residuals-20260708-164823/javahome/bin/java
```

Focused test:

```text
cargo test -p cratonvm-native-builtins t19_2_b_context_names_bind_info_parses_absolute_name -- --nocapture
=> pass
```

The gated synthetic VM collector test could not be used because the pre-existing
`synthetic-jdk` inline test module does not currently compile (`Arc<str>` and
`LazyAttribute` drift unrelated to this change).

WildFly probe results:

```text
standalone-nojit-after-collector-20260708-180904.log: rc=124
standalone-jit-after-collector-20260708-181811.log: rc=124
```

Both logs have zero hits for `getCanonicalName`, `Object.supplier`, `HIB-CV-32`,
`NoClassDefFoundError`, and `NoSuchMethodError`. They still time out on the separate
STW/rollback blocker tracked in
`docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md`.
