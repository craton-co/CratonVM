# WildFly standalone boot: console logging black-holed after stdio install (no WFLYSRV lines ever, boot "invisible") + infinispan cache-containers fail with `ModuleNotFoundException: java.base` — FIXED

Status: FIXED 2026-07-22, branch `fix/wildfly-bootlog-javabase-20260722`
(worktree `/data/wt-wf-bootlog-20260722`, forked from `origin/dev @ f3fc07d40`), binary
`cvm-bootlog-20260722.bin`.

Found while closing the `wildfly-remoting-classcastexception-parallel-extension-add.md` family: what
looked like an "awaitStability null-slots wedge" (boot never printing `WFLYSRV0025`, `[msc]
awaitStability copy problems: skipped 16 null slots` looping under `CRATONVM_DBG_MSC=1`) turned out to
be **boot completing invisibly**: the `standalone/tmp/startup-marker` file IS written, the management
endpoint serves, `main` parks in the normal shutdown wait — but **not one line of WildFly's
jboss-logmanager console output ever reaches the console** (zero `WFLYSRV*` matches across the whole
boot; the visible ERROR lines in probe logs were CratonVM's own Rust-side `jboss_msc` tracing). The
"null slots" message is a benign trace of an empty (capacity-16, zero-entry) problems set. The
repeated `awaitStability(Set,Set)` calls (~2.6/s) are the management layer's normal per-operation
stability checks (deployment scanner et al.) — also benign.

## Root cause 1: `emit_framework_log` dispatched framework log records into WildFly's stdio redirect — circular black hole

All of CratonVM's logging natives (`jboss-logging` `doLog`/`doLogf`, `jboss-logmanager`
`logRaw`/`log`) converge on `native-builtins/src/lib.rs`'s `emit_framework_log`. Since the 2026-07-21
round-9 fix, that function preferred dispatching the record into the CURRENT `System.out` override
stream's own Java `println` (needed for delegating capture streams). But WildFly's boot installs
`org.jboss.stdio.StdioContext$DelegatingPrintStream` overrides whose whole purpose is to REDIRECT
stdout back INTO the logging framework (JUL "stdout" logger → logmanager). Dispatching a framework
log RECORD into them is circular by construction — on real HotSpot those records flow
logger → ConsoleHandler → the fd saved BEFORE the stdio swap, never through the live `System.out`.
Every boot log line after stdio install (which happens before the first `WFLYSRV` line) died in the
handlerless fallback at the bottom of that circle. Early lines (`INFO [org.jboss.modules] ...`)
printed only because they precede the stdio install.

**Fix:** `emit_framework_log` now detects an `org/jboss/stdio/*`-classed override stream and routes
framework records straight to the canonical fd-backed stream (what the real ConsoleHandler would
write to). Non-stdio override streams (test-harness capture streams) keep the dispatch-first
behavior; user `System.out.println` under WildFly still flows through the delegating stream's real
bytecode into the logging framework and comes out once on the console via this same path.

## Root cause 2: `loadModule("java.base")` threw `ModuleNotFoundException` — JPMS platform modules had no bridge

`org.wildfly.clustering.marshalling.protostream.ModuleClassLoaderMarshaller.<init>` does
`moduleLoader.loadModule("java.base")` on every infinispan cache-container start. JPMS platform
modules are not JBoss modules — real jboss-modules serves them through its JDK module bridge — and
CratonVM's `native_loader_load_module` (`native-builtins/src/jboss_module_loader.rs`) only resolved
from the module tree, so every infinispan cache-container-configuration service failed with
`IllegalStateException: org.jboss.modules.ModuleNotFoundException: java.base` (2 of the ~5 constant
background service failures on every standalone boot).

**Fix:** after tree resolution fails (tree entries always win), `java.base`/`java.*`/`jdk.*` names
now synthesize an empty-resource platform `Module` (built through the standard `build_module_object`
path, cached and var-handle-rooted like any other module). JDK classes resolve through the shared
bootstrap path regardless of the requesting loader, so no resource roots are needed.

## Verification

- 4/4 isolated `standalone.sh` boots on the fix binary print the full boot-message family —
  `WFLYSRV0049` (starting), `0039`, `0051`, `0060` (http mgmt), `0071`, `0212`, and
  **`WFLYSRV0025: WildFly Full 32.0.1.Final started in ~20s`** — vs 0 `WFLYSRV*` lines on every
  unfixed boot (dev@7379a391a AND the 2026-07-21 round-9 frozen binary, i.e. long-standing, not a
  regression).
- The infinispan `java.base` service failures are gone (were 2/boot, every boot).
- `cargo test -p cratonvm-native-builtins --lib`: 3063 passed, 0 failed, 6 ignored.
- Known cosmetic residual: the started line reports "Started 0 of 0 services" — the MSC shim's
  `StabilityMonitor` does not track per-service counters. Remaining separate background service
  failures (undertow default-server "Service unavailable", `applicationKS` `KeyStoreException: JKS
  not found`) are pre-existing, independently-tracked gaps.
