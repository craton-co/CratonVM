# Handoff — WildFly MSC real `service.start()` & the boot cascade

**Date:** 2026-06-09  ·  **Branch:** `fix/wildfly-msc-real-service-start`
**Worktree:** `C:\craton\CratonVM-wfmsc` (off `dev`)  ·  **All work below is MERGED to `dev`.**
**Owns:** `bug-wildfly-msc-service-start-callback.md`, `plan-wildfly-msc-real-service-start.md`,
`findings-wildfly-msc-real-service-start.md` (the running session log).

---

## TL;DR — where it stands

Driving the **real `service.start(StartContext)`** callback (gated, default-OFF) took the WildFly health
`testSubsystem` from an immediate `IllegalStateException` all the way to **running to completion**
(`Tests run: 2`). Seven distinct VM/library defects were fixed and merged along the way. **Every VM-level
crash and hang on this path is now cleared.** What remains is a WildFly **model-validation** failure
(`validateDescriptionProviders`) — a different category — and it's gated on a logging-diagnostics fix.

- **Behavior change is GATED behind `CRATONVM_MSC_REAL_START` (default-OFF).** With it off, WildFly's real
  MSC bytecode runs as before — the regression pool is unaffected.
- Several fixes are **always-on** (general VM/JDK correctness): the GC roots, the interpreter TCO
  operand-stack grow, `getUnnamedModule`/`getModule` pin, `ControlledProcessState.getState`, and the
  `EnhancedQueueExecutor` shutdown shim. All verified pool-safe (15/18; see "Pool note").

## Reproduce

```bash
# In the worktree. Gate ON drives real start(); CRATONVM_DBG_MSC traces install/start.
cd /c/craton/CratonVM-wfmsc
CRATONVM_MSC_REAL_START=1 CRATONVM_DBG_MSC=1 bash run-health.sh
# To capture a HANG's Java stack (the harness disables this with 0):
#   add `--stack-dump-on-timeout 75` to the cratonvm args (see run-health.sh).
```
`run-health.sh` runs `org.junit.runner.JUnitCore org.wildfly.extension.health.HealthSubsystemTestCase`
with the cp from `apps/wildfly/health/cratonvm-health-cp.txt` (jboss-msc **1.5.6**). Current result:
`Tests run: 2, Failures: 2` — `testSubsystem` → `RuntimeException` at
`SubsystemTestDelegate.validateDescriptionProviders:470`; `testSchema` → the separate pre-existing JAXP
"Premature EOF" bug.

## Build (worktree, Windows — hostile parallel env)

```
C:\craton\CratonVM-wfmsc\build-wfmsc.bat     # renamed hmcargo/hmrustc survive parallel taskkill;
                                             # vcvars + libffi-seed + own target dir
```
~4–8 min incremental. **Verify the binary relinked** (`Finished` in `build_*.log`, mtime advanced, and
for a literal you added: `grep -a -c "<literal>" target/release/cratonvm.exe`). Run the binary as a copy
(`run-health.sh` copies to `hmvm.exe`) so other agents' `taskkill //IM cratonvm.exe` doesn't hit it.

## Fixes landed this session (all on `dev`)

| # | Fix (commit) | File(s) | On/Gated |
|---|---|---|---|
| P1 | GC roots for container-held service objects (`84120c7`) | `jboss_msc.rs`, `roots.rs`/`gc.rs` step 19 | always-on (inert unless gate on) |
| P2 | Drive real `service.start(StartContext)` via `ServiceBuilderImpl.install()` hook + StartContext (`84120c7`) | `jboss_msc.rs` | **gated `CRATONVM_MSC_REAL_START`** |
| 1 | xnio: populate `Options.*` static fields + Long-handle in trailing slot (OptionMap/Builder) + tolerant `Builder.set` (`940acad9`) | `xnio_async.rs` | always-on |
| 2 | interpreter: grow reused operand stack on tail-call frame replacement (`9991d36d`) | `value_stack.rs`, `frame.rs` | always-on |
| 3 | JDK: `ClassLoader.getUnnamedModule()` native + pin `Class.getModule` across alloc (`d43f91b9`) | `classloader_real.rs`, `lib.rs`, `phases_late.rs` | always-on |
| 4 | `ControlledProcessState.getState()` returns the real `State` enum constant (`3b1de4d1`) | `wildfly_core.rs` | always-on |
| 5 | `EnhancedQueueExecutor` shutdown lifecycle shim (was infinite CAS spin) (`8d25e2b4`) | `wildfly_core.rs` | always-on |

Each is documented in detail (root cause + diagnosis) in
`findings-wildfly-msc-real-service-start.md` ("Session N" sections).

### How P2 works (the core)
Modern WildFly installs every service via `ServiceTarget.addService(name[,svc]).setInstance(svc).install()`
(real bytecode), NOT the legacy 2-arg `ServiceContainer.addService`. So we hook
**`ServiceBuilderImpl.install()`** (`native_service_builder_install`): read `serviceId`/`service`/
`initialMode`/`requires`/`serviceTarget` out of the real builder (jboss-msc 1.5.6 layout), register in the
Rust container, and run a **re-entrant drive loop** (`drive_starts`) that invokes the real
`service.start(synthetic StartContext)` — never holding the container lock across `invoke_virtual`. A
synthetic `StartContext` provides `getController`/`getChildTarget` (returns the builder's own
`serviceTarget`, so child installs route back through the hook)/`failed`. `AbstractControllerService.start`
is synchronous and assigns `controller` at bci 507 *after* two child installs — hence getChildTarget must
work. See `jboss_msc.rs` and the findings doc.

## CURRENT blocker (next up) — `validateDescriptionProviders`

`testSubsystem` now fails as a `RuntimeException` (empty message) at
`org.jboss.as.subsystem.test.SubsystemTestDelegate.validateDescriptionProviders:470` (← `build:560`). A
boot management op failed during the subsystem `:add` (`ERROR WFLYCTL0013: Operation ... failed`), and the
test validates the resulting model/description providers.

**Diagnostics are blocked by a logging gap:** `WFLYCTL0013` logs **literal `%s`** — the jboss-logging
shim isn't substituting the operation/address/stage/**failure-description** args. So we can't yet see
*why* the op failed.

**Recommended next steps (in order):**
1. **Fix the `%s` substitution** (jboss-logging `Logger.logf`/`Messages`/`@Message` formatting path) so
   `WFLYCTL0013`'s failure description prints. This is the unlock — it turns a blind failure into a named
   one. (Grep the logging shims used by WildFly: `DelegatingBasicLogger`, `Logger.logf`, the generated
   `*_$logger` impls — several are already no-op'd in `jboss_msc.rs`, which may be eating the format.)
2. With the description visible, diagnose the health-subsystem `:add` op / description-provider mismatch
   (likely a missing native or model gap reached only now that boot runs).

## Roadmap beyond that (per the plan)

- **Value injection (P3 tail):** `provides(name).accept(v)` → `requires(name).get()` is **NOT wired** — a
  dependent's MSC-injected `Supplier.get()` returns null. (The `executorService` supplier survived only
  because it's ctor-passed, not MSC-injected.) Likely needed once more services with real value deps start.
- **P4:** async services + the standalone daemon to `WFLYSRV0025` + port 9990
  (`bench/wildfly-boot/run-under-cratonvm.sh`).
- **P5:** `stop()` lifecycle; remove the `CRATONVM_DBG_MSC` trace scaffolding before considering
  flipping the gate default-ON.

## Verification / pool note

Regression pool: `RJVM=C:/craton/CratonVM-wfmsc/poolvm.exe bash apps/probe/test-infra/regression-pool/run.sh`
(dev relocated it to `apps/probe/...`). Current result is **15/18**; the 3 "REGRESS" are
`wildfly32-modload`, `wildfly40-modload`, `kc16-modload` and are **path-only baseline staleness** — the
baselines embed the old `...\test-infra\regression-pool\...` path while the pool now lives under
`apps/probe\...`; diff is only the `Modules dir:` line (lines 3-4 `Loaded module`/`OK` match), and it
affects ANY binary. Re-recording those 3 baselines (`run.sh --record <name>`) would restore a clean 18/18;
left as-is to avoid touching pool infra. **0 real regressions** from any fix here.

## Traps learned (save yourself the cycles)

- **Edit the WORKTREE's paths, not the main checkout's.** Building the worktree while Edit-ing
  `C:\craton\CratonVM\...` compiles stale source → byte-identical binary (cost a full build cycle). See
  `reference_worktree_edit_path_trap`.
- **`invoke_virtual` takes the receiver SEPARATELY** — `args` is parameters only. `invoke`/`invoke_special`
  want `args[0]` = receiver. Getting it wrong shifts params → wrong-class `NoSuchMethodError`. See
  `reference_native_invoke_virtual_arg_convention`.
- **Native "alloc obj; create_string/alloc more; set_field(obj, …)" is a GC hazard** — the first obj is
  unpinned in a Rust local across the second allocation; a moving GC reclaims it → stale ref (reused slot,
  often a String). Pin with `pin_native_root`/`read_native_pin` (cf. the `getModule` fix), or alloc with
  no second allocation in between.
- **Hangs:** `--stack-dump-on-timeout N` (N>0) dumps every thread's Java stack then aborts — the harness
  passes `0` (disabled). Re-enable it to locate a hang.
- **MSYS path mangling:** `cmd /c` from Git-Bash mangles `/c`; run `.bat`s via the PowerShell tool, and
  set `MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1` for `;`-classpaths.

## State summary

- `dev` HEAD at handoff: `1fdf2dd6`. Worktree branch `fix/wildfly-msc-real-service-start` is merged in.
- The worktree retains build/run scaffolding (`build-wfmsc.bat`, `run-health.sh`, `*.log`, `hmvm.exe`/
  `poolvm.exe`) — all untracked/uncommitted, safe to keep for continuation.
