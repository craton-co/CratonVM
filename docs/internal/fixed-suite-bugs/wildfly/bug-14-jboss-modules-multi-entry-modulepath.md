# Bug 14 - JBoss Modules multi-entry module path only searched first root

Status: FIXED
Severity: High
First confirmed: 2026-07-05 on Azure worktree `codex/wildfly-nonpassed-probes-20260705-035722`

## Symptom

WildFly domain-mode tests launched the process controller with a multi-entry `-mp` value:

```text
<test>/primary/added-modules:<test>/added-modules:/data/cratonvm/apps/wildfly/build/target/wildfly-41.0.0.Final-SNAPSHOT/modules
```

CratonVM's JBoss Modules bridge kept only the first entry, so the process controller exited immediately with:

```text
org.jboss.modules.ModuleNotFoundException: org.jboss.as.process-controller
```

The parent test only saw this as `DomainLifecycleUtil.awaitServers` failing to start managed servers.

## Root Cause

`native-builtins/src/jboss_module_loader.rs` cached a single `MP_ROOT_CACHE` value. Top-level module loading and dependency closure resolution both used that primary root only. WildFly legitimately places per-test `added-modules` directories before the real distribution modules root, so `org.jboss.as.process-controller` and dependencies such as `org.jboss.logging` were invisible.

## Fix

The bridge now caches all module-path entries, appends all of them to top-level `loadModule` resolution, and uses the same full list from `ensure_resolved` when registering the transitive dependency/linkage closure.

## Verification

- `cargo test -p cratonvm-native-builtins wf_domain_split_module_path_keeps_all_entries -- --nocapture` passed.
- `cargo check -p cratonvm-native-builtins` passed.
- Built `cratonvm-wildfly-nonpassed-20260705-035722-mpmulti2`.
- Minimal repro with a three-entry `-mp` now resolves `org.jboss.as.process-controller` and exits `--help` with rc=0:
  `/data/wt/wt-wildfly-nonpassed-20260705-035722/probes/domain-direct-081/min-pc-help-mpmulti2.log`.
