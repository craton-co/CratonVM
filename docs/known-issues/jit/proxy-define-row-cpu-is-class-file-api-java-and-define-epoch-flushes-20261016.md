> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/jit/proxy-define-row-cpu-is-class-file-api-java-and-define-epoch-flushes-20261016.md`.

# ZGC: the `Zs3Nd3UpcallMints proxy` CPU gap, and the native-door residuals left after zs s9/nd8

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (JIT / class definition).** The row's gap is the JDK's Java (class-file API, module descriptor) under this VM's compiled code, plus the per-define epoch flushes; both named cuts are opt-in and not ZGC (`CRATONVM_LOADER_TABLES_HASHED`, `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH`; row ND-P3 SAME for their soundness, CPU rows P1 / P2 pending). Move to `docs/known-issues/jit/proxy-define-row-cpu-is-class-file-api-java-and-define-epoch-flushes-20261016.md`. §4 (the upcall abort's door, the `--compatible` 19/20 run) is carried to `zgc-zt1nd-*`.

> **STATUS (2026-10-19, zt t3/nd): the per-define half has both named cuts in code, OPT-IN, PENDING the A/B rows of the t3 report: the loader-table scans (`CRATONVM_LOADER_TABLES_HASHED`, t2) and the class-definition epoch flush of the JIT's class-identity dispatch memos (`CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH`: positive entries now survive a define that changes no name answer, edge, in-place origin or redefinition; only refusals drop). Not covered: the interpreter's site caches and the JIT type-check memo, which also stamp the definition epoch (listed in the report as the next cut), and the near-OOM collection share (lane tp). §4 unchanged. Report: `docs/internal/zgc-round-zt-20261018/t3-nd-report.md`.**

> **STATUS (2026-10-19, zt t2/nd): the quadratic term found by reading is cut OPT-IN (`CRATONVM_LOADER_TABLES_HASHED=1`, see `docs/internal/proposals/gc/zgc-zt1nd-proposal-address-keyed-loader-side-tables-20261019.md`), PENDING the A/B rows P1-P3 of the report; the rest of the gap (JDK Java under the class-file API / module descriptor, the per-define epoch flush) is unmeasured. §4 unchanged (host lines needed). Report: `docs/internal/zgc-round-zt-20261018/t2-nd-report.md`.**

> **STATUS (2026-10-19, zt t1/nd): OPEN, attributed further BY READING (no build, no host): every iteration makes a fresh loader, and two loader side tables are `Vec`s scanned linearly 3-5 times per iteration (`loader_namespaces`, `loader_meta`, native-builtins/src/classloader.rs:909-911; the O(1) index misses on a fresh loader and is cleared at every collection), i.e. a quadratic term; plus the per-define class-definition epoch's cache flushes (constant factor). Proposal with the deciding A/B: `docs/internal/proposals/gc/zgc-zt1nd-proposal-address-keyed-loader-side-tables-20261019.md`. Not cut this wave (needs a build to measure). §4 unchanged. Report: `docs/internal/zgc-round-zt-20261018/t1-nd-report.md`.**

> **STATUS (2026-10-16, zs s10/nd9): unchanged, nothing closable locally.** §1-§3 (the CPU gap) wait for host rows P1-P5 (queued in s8a); §4's two items wait for the host's U1 `DBG_RTERR` line and G1 stderr. Report: `docs/internal/zgc-round-zs-20261014/s10-nd9-report.md`.

> **STATUS (2026-10-16, zs s9/nd8): OPEN, attribution PARTLY measured (laptop), host rows pending.** Report: `docs/internal/zgc-round-zs-20261014/s9-nd8-report.md`. Host s6a ND5-P2: `Zs3Nd3UpcallMints proxy 4096 20000` (`--enable-native-access=ALL-UNNAMED -XX:+UseZGC -Xmx64m`) costs HotSpot 16.2 / 22.7 / 16.0 s CPU and CratonVM 114 / 145 / 72 s (3 interleaved rounds); the battery's stdout is SAME.

## 1. What the row is

The `proxy` mode is not an upcall row: each iteration is
`Proxy.getProxyClass(new ClassLoader(..){}, Runnable.class)` (a fresh loader,
a dynamic `jdk.proxyN` module for it, a `ProxyGenerator` class file through
the class-file API, a define, a `<clinit>` that reflects four `Method`s) plus
512 bytes of garbage, on a heap filled to 4 MiB free.

## 2. What the laptop measured (HotSpot 25.0.3, Windows; CratonVM debug only)

`tools/probes/Zs9Nd8ProxyCost.java` (per-define main-thread CPU, process CPU,
collections, and a 10 ms stack-sample histogram of the main thread):

| arm | HotSpot main CPU / define | HotSpot process CPU / define | collections / 3000 defines |
|---|---|---|---|
| `fill` (the row's heap) | 1.58 ms | -- | 8078 (GC beans: cycles + pauses) |
| `room` (no fill) | 0.79-1.20 ms | 2.7-4.7 ms | 28-34 |
| `define` (fresh loader + one prebuilt class) | 0.07 ms | 0.13 ms | 10 |
| `generate` (class-file API only) | 0.06 ms | 0.66 ms | 20 |

- HotSpot's samples, `room`: `reflect` 96, `generate` 101, `define` 45,
  `module` 30, `other` 25, `loader` 6. `reflect` is
  `Class.getDeclaredConstructors0` (the proxy constructor lookup) at about a
  third; on `fill` it is the same mix. So on HotSpot the row is the Java of
  `ProxyBuilder` / `ProxyGenerator` / the dynamic module plus HotSpot's own
  reflection natives, and the near-full heap costs about 1.3-2x on the main
  thread.
- CratonVM debug (`room`, 30-40 defines, samples at 50 ms): `generate` 63 %,
  `module` 24 % (`Checks.isJavaIdentifier`, `ModuleDescriptor` building),
  `other` 9 %, `define` 3 %, `reflect` 2 %. A debug build overstates the
  interpreter, so this is the SHAPE only: CratonVM's own natives on this path
  (`defineClass*`, `getDeclaredConstructors0`, `Module.defineModule0`) are
  small; the time is the JDK's Java (the class-file API and the module
  descriptor), i.e. compiled-code quality on that Java, not a native door.
- CratonVM debug `fill` vs `room` per define: 0.61 s vs 0.86 s (no
  slowdown from the full heap in debug; the collections per define are few).
- `com.sun.management.OperatingSystemMXBean.getProcessCpuTime()` answers
  `-1` on CratonVM on Windows only (`jmx.rs` `os_metrics::process_cpu_time_ns`
  reads `/proc/self/stat`, Linux only; HotSpot answers on Windows too). The
  host (Linux) is unaffected. Not a door; noted for the MXBean owner (`ob6`).
- CratonVM debug, per-arm main-thread CPU per define (60 / 60 / 30
  iterations): `define` 18.5 ms, `generate` 25 ms, `room` 192 ms; i.e. the
  fresh loader + define is ~10 % of the debug row and the Java of
  generation + the dynamic module is the rest. The debug/release factor is not
  known here, so only the host's release rows can say what the 5-7x is.

## 3. What would decide it (host rows; the s9/nd8 report's P-rows)

Run `Zs9Nd8ProxyCost` in all four arms on HotSpot 25.0.4 and on the release
binary, both modes, `/usr/bin/time` around each:

- if `define` is >= 3x HotSpot: the define path (class manager define,
  mirror, the per-define epoch bumps that retire per-site caches,
  `class_definition_epoch` readers in `site_cache.rs` / `jit_bridge.rs` /
  `helpers.rs`) is the gap -- profile it with `perf record` on the `define`
  arm;
- if `generate` is >= 3x: compiled-code quality on the class-file API
  (`jdk.internal.classfile.impl.*`, lambdas, `SplitConstantPool`) -- a JIT
  round's item, not this lane's;
- if `room` ~ `generate` + `define` but `fill` is far above `room`: the
  near-full collections (ZGC cycle count and CPU per cycle at 60 MB live; see
  `-Xlog:gc` counts in the report);
- the `module` share (dynamic module per loader): compare the samples.

## 4. Residuals of the native doors

- **The upcall abort's door (`zgc-zs7nd6-native-door-residuals-FIXED-20261019.md`
  §3, nd5 page §1)**: still not named; the s6a host results this lane received
  give no `CRATONVM_DBG_RTERR=OutOfMemory` line for row M2 / RV4-1. What would
  close it: that line from one `Zs3Nd3UpcallMints mixed 4096 20000` run with
  both leak kills on (the report's U-row).
- **ND5-M `--compatible` 19/20**: one of 20 `Zs5Nd5ProxyGrowth 6 500` runs
  under `--compatible` did not match HotSpot. Not reproduced here (a debug run
  of 3000 proxy defines takes ~45 min on the laptop). What would close it: the
  missing run's stderr (`round=` / `used_kb=` lines and
  `growth_bytes_per_1000_defines=`), to tell a retained loader set (growth in
  every round) from a `System.gc()` that returned before its cycle finished
  (one high round, then flat).
