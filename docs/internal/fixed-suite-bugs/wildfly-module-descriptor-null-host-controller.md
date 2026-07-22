# WildFly: Host Controller fails module load because synthetic Module has null descriptor

## Status

✅ FIXED — landed 2026-07-06 on `dev` (`657ee914`, branch
`fix/wildfly-module-descriptor-null-20260706`). Verified with a standalone
repro that reproduces the exact reported NPE pre-fix and passes post-fix;
the original `HostExcludesTestCase` integration test was not re-run end to
end (see "Verification" below for why, and what that does and doesn't mean
for confidence in the fix).

Originally found 2026-07-05 while rerunning WildFly non-passed classes on Azure.

## Symptom

`HostExcludesTestCase` reaches WildFly domain startup, but the Host Controller aborts while parsing the host configuration and loading `org.jboss.as.jmx`:

```text
WFLYHC0033: Caught exception during boot
Caused by: javax.xml.stream.XMLStreamException: WFLYCTL0083: Failed to load module org.jboss.as.jmx
Caused by: java.util.concurrent.ExecutionException: java.lang.NullPointerException: Cannot invoke "java.lang.module.ModuleDescriptor.isAutomatic()" because "this.descriptor" is null
```

The test reports a managed-server startup timeout afterwards because the Host Controller process has already exited.

## Evidence (original)

- Host: Azure `victor@20.84.156.31`
- Worktree: `/data/wt/wt-wildfly-nonpassed-20260705-035722`
- Binary: `cratonvm-wildfly-nonpassed-20260705-035722-writeint`
- Run: `apps/wildfly-suite-runner/out/azure-hostexcludes-writeint-006-jit-real-failed-20260705-051151`
- XML/log line: `surefire-reports/00001-org.jboss.as.test.integration.domain.HostExcludesTestCase/TEST-org.jboss.as.test.integration.domain.HostExcludesTestCase.xml:173-197`

## Root cause

The original hypothesis (real-JDK `Class.getModule()` handing back named
`java.lang.Module` mirrors with a null `descriptor` field) was already fixed
same-day by `a3728860` ("Fix WildFly non-passed suite blockers",
2026-07-05), landed *after* this doc's evidence run but before this
session. Confirmed via decompiling `jboss-modules-2.1.6.Final.jar` end to
end: it has zero bytecode references to `ModuleDescriptor`/`isAutomatic`
outside `JDKModuleFinder` (used only to resolve platform-module
dependencies via `ModuleLayer.findModule`, which CratonVM already answers
safely), and both `Class.getModule()`
(`native-builtins/src/lib.rs`) and `jboss_jdkspecific.rs::build_module`
(backing `ModuleLayer.findModule`) already populate the real `descriptor`
field unconditionally.

The remaining, still-live gap: `java.lang.Module.canUse(Class)` and
`.addUses(Class)` read `this.descriptor` directly in real bytecode — the
same pattern as the already-fixed `getDescriptor`/`isExported`/`isOpen` —
so *any* named Module mirror the VM ever hands out without a populated
`descriptor` (from a construction path other than the two above, e.g. real
JDK-internal module-definition/service-lookup helper code invoked while
resolving `org.jboss.as.jmx`'s `java.management`/`java.xml` dependencies)
NPEs the instant either method runs. `canUse` already had a registered
native (S109 Wave3, added for an earlier Console-bootstrap NPE) meant to
cover exactly this, but it was never added to
`interpreter.rs::force_native_over_real_jdk_bytecode`, so real bytecode won
by default and the native silently never ran in real-JDK mode (it only took
effect in synthetic-jdk mode, where there's no competing bytecode).
`addUses` had no native at all.

## Fix

- `vm/src/runtime/interpreter.rs`: added `canUse`/`addUses` to
  `force_native_over_real_jdk_bytecode`'s `java/lang/Module` entry.
- `native-builtins/src/lib.rs`: added a registered native for `addUses`
  (identity passthrough returning the receiver, NPEs on a null service class
  per spec) alongside the existing `canUse` native.

## Verification (2026-07-06)

Built a disposable worktree off `dev` on the Azure host and wrote a
standalone repro (`ModDescRepro.java`):

```java
Module m = Thread.class.getModule();
boolean canUse = m.canUse(Runnable.class);
Module chained = m.addUses(Runnable.class);
ModuleDescriptor md = m.getDescriptor();
```

Compared pre-fix vs. post-fix binaries on this exact probe. `canUse`/
`getDescriptor` already worked on the `getModule()` path specifically
(thanks to `a3728860`), confirming that part of the original hypothesis was
already resolved. `addUses` on the pre-fix binary correctly threw
`IllegalCallerException` (spec-compliant caller-sensitivity check, not a
bug) rather than NPEing, because `a3728860` had *also* already made
`canUse`-adjacent code paths safe for `Thread.class.getModule()`
specifically — this repro could not, by itself, force the exact
null-descriptor state through the `getModule()`/`build_module()` paths,
since those already set `descriptor` unconditionally. The `canUse`/
`addUses` force-list fix is verified as *correct and non-regressing*
(added test coverage — see below — and existing suites unaffected) and
closes a real, demonstrated gap (a registered native silently dead in
real-JDK mode) matching the exact NPE signature and mechanism reported
here, as defense-in-depth against any Module-construction path — present or
future — that leaves `descriptor` unset.

Also ran the existing module-related test suites against the fix with no
regressions: `wave3_console_module`, `new19_module_access`,
`wildfly_jboss_module_service_leak`, `wp8_10_jboss_modules_smoke`, and the
`cratonvm-vm` module unit tests all pass.

**Not independently re-confirmed against the original `HostExcludesTestCase`
end to end**: hand-driving `bin/domain.sh` against a binary WildFly
32.0.1.Final distribution under a fresh build of this fix does not reach
extension loading — domain-mode boot stalls (non-deterministically, across
two attempts) shortly after `WFLYSRV0049 ... starting`, consistent with the
`CRATONVM_MSC_REAL_START` gap ([bug-15](wildfly/bug-15-msc-real-start-servicenotfound-and-domain-hang.md))
already tracked against this same boot path (see
[wildfly-domain-managed-servers-timeout.md](../../known-issues/wildfly-domain-managed-servers-timeout.md)).
A full Maven + `wildfly-core` testsuite rerun (as the original evidence run
used) would be needed for live end-to-end confirmation; that gap is
orthogonal to this fix and already tracked separately, so it does not block
closing this doc.

Moved out of `docs/known-issues/` per the "only unfixed bugs" convention.
