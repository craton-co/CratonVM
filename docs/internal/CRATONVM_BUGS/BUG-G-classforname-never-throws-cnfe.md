# BUG-G — `Class.forName` synthesizes a stub for any `java.*` name (never throws ClassNotFoundException)

**Test:** `jakarta.el.TestImportHandler` (1 of 2 failures — `java.net.ArrayList`
"conflict"). **Status: FIXED.**

> **FIX (`class_manager.rs`):** In `load_class`, when delegated lookup misses a
> standard-JDK-namespace name (`java/`, `javax/`, `sun/`, `jdk/`, `com/sun/`)
> *and* `has_real_boot_classes()` (real-JDK mode — the boot jimage is
> authoritative), throw `ClassNotFoundException` instead of synthesizing a stub.
> Stubs are still created for array classes, the enterprise app prefixes
> (`org/jboss`, `org/wildfly`, …) that legitimately use the stub mechanism, and
> all of synthetic-JDK mode. Internal synthetic helpers reach the class store via
> `ensure_synthetic_class`, which registers directly when this load fails, so
> they are unaffected. Verified: `Class.forName("java.net.ArrayList")` → CNFE,
> real classes still resolve; the spurious import conflict is gone (the one
> remaining `TestImportHandler` failure is an unrelated nested-class resolution
> issue: `DigestAuthenticator$AuthDigest`).

## Symptom

`Class.forName("java.net.ArrayList", false, loader)` returns a `Class` instead
of throwing `ClassNotFoundException`. Probe (`cratonvm`):

```
java.net.ArrayList     => FOUND class java.net.ArrayList
java.lang.Nonexistent  => FOUND class java.lang.Nonexistent
java.util.ArrayList    => FOUND class java.util.ArrayList
```

`jakarta.el.ImportHandler` imports `java.util.*` and `java.net.*`, then resolves
the simple name `ArrayList` by probing each package via `Class.forName`. Because
the `java.net.ArrayList` probe wrongly succeeds, ImportHandler reports
*"The class [java.net.ArrayList] could not be imported as it conflicts with
[java.util.ArrayList]"*. Any "does this class exist?" probe (ServiceLoader,
optional-dependency detection, etc.) is similarly affected.

## Root cause

`classloading/src/class_manager.rs` `load_class` (~line 2024):

```rust
Err(_) if is_jdk_class(name) => {
    // JDK class not found as a .class file — create a synthetic stub.
    self.create_synthetic_stub(name)
}
```

`is_jdk_class` returns true for **any** name in a JDK package prefix
(`java/`, `javax/`, ...), so a non-existent `java/net/ArrayList` —
which is absent from the jimage — falls into the synthetic-stub fallback and is
returned as a real class. In a real-JDK build the jimage is authoritative, so a
bootstrap-delegated miss on a `java.*` name means the class genuinely does not
exist and should surface as `ClassNotFoundException`.

## Why deferred

The synthetic-stub fallback is load-bearing for the WildFly / Keycloak /
JBoss-Modules app gauntlet (VM-internal classes handled by natives with no
`.class` file rely on it). Narrowing it to throw CNFE for genuinely-absent
`java.*` names needs to distinguish "natively-handled JDK class with no
classfile" from "non-existent name", and must be verified to not regress the
gauntlet's class loading. Tracked for a careful, separately-verified change.
