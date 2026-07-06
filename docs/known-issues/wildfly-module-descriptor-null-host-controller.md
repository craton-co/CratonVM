# WildFly: Host Controller fails module load because synthetic Module has null descriptor

## Status

Root-cause mechanism fixed 2026-07-06 (branch `fix/wildfly-module-descriptor-null-20260706`). Verified with a standalone repro; live re-confirmation via the original `HostExcludesTestCase` is blocked by the same domain-boot infrastructure gap (`CRATONVM_MSC_REAL_START`, [bug-15](../internal/wildfly-suite-bugs/bug-15-msc-real-start-servicenotfound-and-domain-hang.md)) documented in [wildfly-domain-managed-servers-timeout.md](wildfly-domain-managed-servers-timeout.md) — see "2026-07-06 investigation" below.

## Symptom

`HostExcludesTestCase` now reaches WildFly domain startup, but the Host Controller aborts while parsing the host configuration and loading `org.jboss.as.jmx`:

```text
WFLYHC0033: Caught exception during boot
Caused by: javax.xml.stream.XMLStreamException: WFLYCTL0083: Failed to load module org.jboss.as.jmx
Caused by: java.util.concurrent.ExecutionException: java.lang.NullPointerException: Cannot invoke "java.lang.module.ModuleDescriptor.isAutomatic()" because "this.descriptor" is null
```

The test reports a managed-server startup timeout afterwards because the Host Controller process has already exited.

## Evidence

- Host: Azure `victor@20.84.156.31`
- Worktree: `/data/wt/wt-wildfly-nonpassed-20260705-035722`
- Binary: `cratonvm-wildfly-nonpassed-20260705-035722-writeint`
- Run: `apps/wildfly-suite-runner/out/azure-hostexcludes-writeint-006-jit-real-failed-20260705-051151`
- XML/log line: `surefire-reports/00001-org.jboss.as.test.integration.domain.HostExcludesTestCase/TEST-org.jboss.as.test.integration.domain.HostExcludesTestCase.xml:173-197`

## Original root-cause hypothesis (partially superseded — see 2026-07-06 below)

The default real-JDK path returns synthetic `java.lang.Module` mirrors for named modules but leaves their real `descriptor` field null. Public access checks are mostly native-overridden, but JDK private module helpers and module-definition code can still read `this.descriptor` directly and then call `ModuleDescriptor.isOpen()` / `isAutomatic()` / collection accessors. The synthetic descriptor support is richer in `register_p59_module` than in the essential real-JDK path, so WildFly can reach real bytecode against a null descriptor.

## 2026-07-06 investigation and fix

`register_p59_module` (`native-builtins/src/phases_late.rs`) is dead code in the
default `cratonvm-cli` build — it's only reachable via the `synthetic-jdk`
feature, which real-JDK-mode WildFly runs don't use (same
essential-vs-synthetic-jdk gap documented for `canRead`/`getDescriptor`
elsewhere in `native-builtins`). So the fix had to live entirely in the
essential real-JDK path, not in that function.

Decompiling `jboss-modules-2.1.6.Final.jar` end to end found **zero** bytecode
references to `ModuleDescriptor` or `isAutomatic` outside `JDKModuleFinder`
(used only to resolve platform-module dependencies via `ModuleLayer
.findModule`, which CratonVM already answers safely — traced the full method
and confirmed its one `getDescriptor()` call is null-checked before use).
`Class.getModule()` (`native-builtins/src/lib.rs`) and
`jboss_jdkspecific.rs::build_module` (backing `ModuleLayer.findModule`) both
already populate the real `descriptor` field unconditionally — a same-day
earlier fix (`a3728860`, "Fix WildFly non-passed suite blockers") landed
*after* this doc's evidence run, so a plain `Thread.class.getModule()
.canUse(...)` no longer NPEs even on pre-this-session `dev`.

The remaining, still-live gap: `java.lang.Module.canUse(Class)` and
`.addUses(Class)` read `this.descriptor` directly in real bytecode (same
pattern as the already-fixed `getDescriptor`/`isExported`/`isOpen`), so *any*
named Module mirror the VM ever hands out without a populated `descriptor` —
from a construction path other than the two above, e.g. real JDK-internal
module-definition/service-lookup helper code invoked while resolving
`org.jboss.as.jmx`'s `java.management`/`java.xml` dependencies — NPEs the
instant either method runs. `canUse` already had a registered native (S109
Wave3, added for an earlier Console-bootstrap NPE) meant to cover exactly
this, but it was never added to
`interpreter.rs::force_native_over_real_jdk_bytecode`, so real bytecode won
by default and the native silently never ran in real-JDK mode (it only took
effect in synthetic-jdk mode, where there's no competing bytecode). `addUses`
had no native at all.

Fix (`fix/wildfly-module-descriptor-null-20260706`):
- `vm/src/runtime/interpreter.rs`: added `canUse`/`addUses` to
  `force_native_over_real_jdk_bytecode`'s `java/lang/Module` entry.
- `native-builtins/src/lib.rs`: added a registered native for `addUses`
  (identity passthrough returning the receiver, NPEs on a null service class
  per spec) alongside the existing `canUse` native.

Verified with a standalone repro (`ModDescRepro.java`: `Thread.class
.getModule()`, then `canUse`/`addUses`/`getDescriptor` plus null-argument
NPE-spec checks) built against both the pre-fix and post-fix binary. Both
`canUse`/`addUses` already worked for the `getModule()` path specifically
(thanks to `a3728860`), confirming that hypothesis is resolved; the fix here
closes the `canUse`/`addUses` force-list gap as defense-in-depth against any
other Module-construction path (present or future) that leaves `descriptor`
unset, matching the exact NPE signature reported here.

**Not independently re-confirmed against the original `HostExcludesTestCase`
end to end**: hand-driving `bin/domain.sh` against a binary WildFly
32.0.1.Final distribution under a fresh build of this branch does not reach
extension loading — domain-mode boot stalls (non-deterministically, across
two attempts) shortly after `WFLYSRV0049 ... starting`, consistent with the
`CRATONVM_MSC_REAL_START` gap ([bug-15](../internal/wildfly-suite-bugs/bug-15-msc-real-start-servicenotfound-and-domain-hang.md))
already tracked against this same boot path. A full Maven + `wildfly-core`
testsuite rerun (as the original evidence run used) would be needed for
live end-to-end confirmation.
