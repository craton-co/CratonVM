# WildFly standalone boot: `ObjectName.getCanonicalKeyPropertyListString()` NPE on `_ca_array` blocked 100% of boots — FIXED

Status: RESOLVED — fixed 2026-07-14
Severity: was Critical (blocked every WildFly-standalone-boot-based verification attempt on real-JDK25 mode, at the very first JMX MBean registration, before any subsystem/extension loads)
Regression window: introduced by `d8092acb` ("fix-tests-real-jdk-contracts"), confirmed present through `220ebb7d` and later, bisected 2026-07-14
Fixed: 2026-07-14, branch `fix/objectname-canonical-key-proplist-20260714`, commit `974c0838`

## Symptom

Booting WildFly standalone (`jboss-modules.jar org.jboss.as.standalone -c=standalone.xml`) under CratonVM in real-JDK25 mode failed immediately, on the very first JMX MBean registration (`JMImplementation:type=MBeanServerDelegate`), before any extension/subsystem loaded:

```
Exception in thread "main" java/lang/IllegalStateException: Can't register delegate.
Caused by: java/lang/NullPointerException: Cannot read the array length because "this._ca_array" is null
    at javax/management/ObjectName.getCanonicalKeyPropertyListString(ObjectName.java:1633)
    at com/sun/jmx/mbeanserver/Repository.addNewDomMoi/addMBean(Repository.java:130)
    at com/sun/jmx/mbeanserver/JmxMBeanServer.initialize/newMBeanServer(JmxMBeanServer.java:1199)
    at java/lang/management/ManagementFactory.getPlatformMBeanServer(ManagementFactory.java:469)
```

Confirmed deterministic: 20/20 and 5/5 across multiple independent binary builds from `220ebb7d` through `dev` tip. This blocked essentially all WildFly-boot-based verification work on real JDK25 on this host.

## Bisect

Known-good reference point: a binary built ~2026-07-13 12:27 (dev tip at the time, commit `8f679a7d`) booted past this point fine against `wildfly-32.0.1.Final`. Known-bad: `220ebb7d` (2026-07-14). `git bisect` between `8f679a7d` and `220ebb7d` (219 commits), rebuilding `cratonvm-cli` in `--release` and running the repro at each step, landed cleanly on:

```
d8092acbaa9c13575f9fa1a0f37752a292177bf1  fix-tests-real-jdk-contracts
```

which does **not** touch `native-builtins/src/jmx.rs` at all — the regression is an emergent side effect of a change elsewhere, not a direct edit to the JMX/`ObjectName` code.

## Root cause

`d8092acb` added `native_methods.set_drop_synthetic_stubs(true)` to both real-JDK-mode registration paths in `vm/src/vm/vm_init.rs` (`:1337`, `:1731`). This makes real-JDK mode unconditionally drop every `NativeKind::SyntheticStub`-tagged registration at population time (`native-api/src/registry.rs:3397`) — correct policy in general ("don't let an approximation shadow real bytecode"), and this specific instance of it was itself fixing a real, separate bug: `register_mbean_server_factory_synthetic` (the `MBeanServerFactory.createMBeanServer`/`newMBeanServer` overrides) was previously called unconditionally from `register_jmx_natives` in **both** real- and synthetic-JDK paths, which meant `ManagementFactory.getPlatformMBeanServer()` always handed back a synthetic, bare-interface-typed `MBeanServer` even in real-JDK mode, instead of letting real bytecode construct a concrete `com.sun.jmx.mbeanserver.JmxMBeanServer`.

That synthetic-server shadow was not an accident — [`managerwebapp-deploy-bare-assertion-FIXED.md`](managerwebapp-deploy-bare-assertion-FIXED.md)'s "Bug 3a" (2026-07-10) had already found and documented that the *real* `JmxMBeanServer` construction chain throws exactly this `_ca_array` NPE, and explicitly called the synthetic-server shadow the reason real bytecode wasn't safe to let run yet ("not something this session fixed, just confirmed and documented as the reason Bug 3's workaround is necessary"). `d8092acb` correctly finished the KAFKA-MBEAN fix's original intent (making `register_mbean_server_factory_synthetic` a standalone function invoked only from the synthetic-JDK path) without realizing it was thereby re-exposing the already-known, already-deliberately-unfixed Bug 3a — which, once unmasked, fires on 100% of boots instead of being confined to the narrow probe that found it in July.

The actual defect (Bug 3a itself): `javax/management/ObjectName`'s `<init>` (all three overloads) is natively intercepted (`NativeKind::Bridge`, registered in `register_object_name`, `native-builtins/src/jmx.rs`) and constructs a bare 1-field synthetic object via `alloc_concurrent_synthetic(ctx, "javax/management/ObjectName", 1)` that stores only the canonical name string in field 0 (`object_name_new`/`object_name_set_text`). Several of `ObjectName`'s *other* methods are natively covered against that same 1-field text model (`getDomain`, `getKeyProperty`, `toString`/`getCanonicalName`, `quote`, `apply`, `equals`, `hashCode`), but `getCanonicalKeyPropertyListString()` was not, so it fell through to real JDK bytecode, which reads the real private field `_ca_array` (a `Property[]`, never populated because construction was shortcut). An out-of-bounds read of that reference field yields `null`, and real bytecode's `_ca_array.length` then throws a genuine `NullPointerException`. `Repository.addMBean` also calls `ObjectName.isPattern()` immediately before hitting this path; that one wasn't throwing only by luck — an out-of-bounds read of the `int _compressed_storage` field defaults to `0`, which happens to decode as "not a pattern" for concrete (non-wildcard) names.

## Fix

`native-builtins/src/jmx.rs`, `register_object_name`: added five new natives, all derived from the same canonical-string text model the rest of the file already uses (no real-field-layout overhaul, and no change to the `<init>` Bridge shortcut — `jmx.rs`'s own `MBeanServer`/registry helpers already assume the 1-field shape pervasively):

- `getCanonicalKeyPropertyListString()` — substring after the domain colon, minus a trailing `",*"`/`"*"` pattern suffix. Mirrors real JDK's `_canonicalName.substring(domainLength + 1, len)`.
- `isPattern()` / `isDomainPattern()` / `isPropertyPattern()` / `isPropertyListPattern()` — derived from the existing `object_name_parts()` helper (domain wildcard characters, trailing `",*"`/`"*"`, any wildcarded property value). Added defensively alongside the required fix, since `Repository.addMBean` calls `isPattern()` on this same path and was previously "working" only by the OOB-read-defaults-to-zero accident described above.

Three candidate approaches were considered (see the task this fix originated from): (a) make the synthetic model real-class-layout-aware like `StringJoiner`'s `sj_real_layout` pattern; (b) natively implement the specific un-intercepted methods against the synthetic layout; (c) let `<init>` yield to real bytecode entirely. (b) was chosen: (a) would be substantial new machinery for a handful of read-only accessor methods, and (c) would break every other `ObjectName` native in this file that assumes the 1-field synthetic shape (`object_name_text`, `object_name_set_text`, the `MBeanServer`/registry key-resolution helpers, etc.) — plus this investigation's whole history is full of GC-safety/staleness bugs specifically in the "half-real object" family that real-bytecode construction of a previously-synthetic class tends to produce.

## Verification

- 45/45 WildFly-standalone-boot repro runs (three batches of 10, 20, 15) against a private copy of `wildfly-32.0.1.Final`: 0 occurrences of the `_ca_array` NPE post-fix, versus 5/5 deterministic reproduction on unfixed `dev` HEAD (`82f5b33e`) with the same harness. Boot now proceeds past `MBeanServerDelegate` registration into `ManagementFactory$PlatformMBeanFinder.<clinit>` before hitting an unrelated, separate, pre-existing gap (see below).
- `cargo test -p cratonvm-native-builtins --lib --features experimental-jmx` (the `jmx` module is gated behind that feature — off by default for this crate in isolation, on by default for `cratonvm-vm`/`cratonvm-cli`): 3017 passed / 0 failed before (git-stash same-tree baseline) vs 3020 passed / 0 failed after — exactly +3 for the three new tests added alongside this fix (`test_object_name_new_natives_are_registered_bridge`, `test_object_name_get_canonical_key_property_list_string`, `test_object_name_is_pattern_family`), 0 regressions.

## Separate, NOT-fixed finding: `d8092acb` also silently disabled the entire `sun.management`/`com.sun.management.internal` native surface

The same `set_drop_synthetic_stubs(true)` hardening also drops ~130 other `SyntheticStub`-tagged registrations across the whole native registry in real-JDK mode (confirmed via the `CRATONVM_DBG_DROPPED_STUBS=1` diagnostic), including the *entire* `sun/management/VMManagementImpl`, `MemoryImpl`, `ClassLoadingImpl`, `GarbageCollectorImpl`, `MemoryPoolImpl`, `MemoryManagerImpl`, `OperatingSystemImpl`, `HotSpotDiagnostic`, and `Flag` native surfaces (`register_vm_management_impl` and neighbors in `native-builtins/src/jmx.rs`, all explicitly tagged `NativeKind::SyntheticStub`). Unlike `ObjectName`, these are genuine JNI-only natives with **no real bytecode body at all** — there is nothing for them to "shadow", so tagging them `SyntheticStub` was simply wrong (same bug class, different manifestation, as the same-day `register_real_charset_natives`/`String.getBytes()` regression, see [`string-getbytes-empty-real-jdk-mode-FIXED.md`](../string-getbytes-empty-real-jdk-mode-FIXED.md), and the `java.util.Properties` regression fixed by `f62d2073`).

With the ObjectName fix above applied, WildFly boot now reaches this gap deterministically (confirmed 45/45):

```
Missing native method in real-JDK mode method=sun/management/VMManagementImpl.getVersion0()Ljava/lang/String;
Exception in thread "main" java/util/ServiceConfigurationError: Provider com.sun.management.internal.PlatformMBeanProviderImpl could not be instantiated
Caused by: java/lang/UnsatisfiedLinkError: sun/management/VMManagementImpl.getVersion0()Ljava/lang/String;
    at com/sun/management/internal/PlatformMBeanProviderImpl.<init>(PlatformMBeanProviderImpl.java:60)
    at sun/management/ManagementFactoryHelper.<clinit>(ManagementFactoryHelper.java:67)
    at sun/management/VMManagementImpl.<clinit>(VMManagementImpl.java:60)
```

This is now the next blocker for WildFly-standalone-boot verification on real JDK25. Not fixed here (separate root cause, separate fix location, and a large-ish blast radius that deserves its own dedicated pass) — likely fix shape: wrap `register_vm_management_impl` (and the neighboring `register_*` functions covering `MemoryImpl`/`ClassLoadingImpl`/`GarbageCollectorImpl`/`MemoryPoolImpl`/`MemoryManagerImpl`/`OperatingSystemImpl`/`HotSpotDiagnostic`/`Flag`) in `registry.with_category(NativeKind::Bridge, |r| { ... })`, mirroring the exact fix already applied to `register_real_charset_natives` and `register_properties_sidetable`.

## Repro

```bash
cd <wildfly-32.0.1.Final-copy>
rm -f standalone/log/server.log
rm -rf standalone/data standalone/tmp
mkdir -p standalone/data standalone/tmp
CRATONVM_JAVA_HOME=<real-jdk25-home> timeout 45 <cratonvm-fake-javahome>/bin/java \
  -Xmx512m -XX:MetaspaceSize=128m \
  -Djboss.home.dir=. -Djboss.server.base.dir=standalone \
  -Djboss.server.log.dir=standalone/log -Djboss.server.config.dir=standalone/configuration \
  -Dorg.jboss.boot.log.file=standalone/log/server.log \
  -Dlogging.configuration=file:standalone/configuration/logging.properties \
  -jar jboss-modules.jar -mp modules \
  org.jboss.as.standalone -Dts.wildfly.version=32.0.1.Final -c=standalone.xml
```
