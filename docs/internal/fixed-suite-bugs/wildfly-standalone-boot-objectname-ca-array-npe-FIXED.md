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

## Interaction with a concurrent fix: `getPlatformMBeanServer()` is masked again, but the underlying `ObjectName` defect is genuinely closed

A concurrent same-day session landed `6a0eedd8` ("retag JMX + Function$Identity natives Bridge, close TIMEOUT_NO_WARN root cause") independently while this fix was in progress. It found the exact same `_ca_array` NPE (calling it "a genuine, uninvestigated bug") but took a different, narrower path: it retagged `register_management_factory_platform_server_stub` (which backs `ManagementFactory.getPlatformMBeanServer()` itself) from `SyntheticStub` back to `Bridge`, alongside the ~130 other mistagged registrations described below (`VMManagementImpl`, `MemoryImpl`, `ClassLoadingImpl`, `GarbageCollectorImpl`, `MemoryPoolImpl`, `MemoryManagerImpl`, `OperatingSystemImpl`, `HotSpotDiagnostic`, `Flag` — this fully addresses the "Separate finding" this doc originally flagged as a follow-up here; no further action needed on that front). Tagging `getPlatformMBeanServer()` itself back to `Bridge` means it always wins again, so `ManagementFactory.getPlatformMBeanServer()` once more returns CratonVM's synthetic `MBeanServer` in real-JDK mode — the same shadow `d8092acb` had removed — and WildFly's boot path (`getPlatformMBeanServer` -> `MBeanServerFactory.createMBeanServer` -> `PluggableMBeanServerBuilder` -> real `JmxMBeanServer` -> `Repository.addNewDomMoi` -> `ObjectName.getCanonicalKeyPropertyListString`) no longer reaches real bytecode through *that specific call*, so it no longer exercises this fix either.

Verified via a direct A/B on the fully-merged tree (`origin/dev` tip `7eccd8e0`, which includes `6a0eedd8`, both with and without this fix's `native-builtins/src/jmx.rs` changes): **both** configurations show 0/10 `_ca_array` NPEs and reach well past `MBeanServerDelegate` registration (10/10 and 8/10 "past blocker" respectively, the remainder being known-separate STW/timeout residuals unrelated to JMX). So for *this specific* WildFly-standalone-boot repro, `6a0eedd8`'s retag alone is now sufficient, independent of this fix.

This fix is not redundant, though: it closes the actual, real, previously-"uninvestigated" `ObjectName` defect (open since [`managerwebapp-deploy-bare-assertion-FIXED.md`](managerwebapp-deploy-bare-assertion-FIXED.md)'s "Bug 3a", 2026-07-10) rather than re-masking it, so it protects against the same regression recurring the next time someone tries to correctly finish unmasking `getPlatformMBeanServer()` (which remains the intended long-term direction per the original KAFKA-MBEAN note — a synthetic, bare-interface-typed `MBeanServer` is itself a known-imperfect stand-in, see `managerwebapp-deploy-bare-assertion-FIXED.md`'s Bug 3). It also covers any other real-bytecode path that reaches a synthetic `ObjectName` directly (e.g. a probe that constructs a real `JmxMBeanServer` directly, bypassing `getPlatformMBeanServer()` entirely, exactly as the original Bug 3a finding did).

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
