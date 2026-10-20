# gcj j1/ob: Generational observability residuals (safepoint statistics, JMX, JFR banner, PrintFlagsFinal)

> **STATUS (2026-10-19, gcj j2/ob2): items 4 and 5 FIXED IN CODE behind opt-in switches (default OFF), PENDING BUILD/PROBE; items 1, 2 and 3 OPEN (unchanged).**
> - **Item 4** (JFR banner): `CRATONVM_JFR_STARTUP_BANNER`. The text is NOT redacted: the local HotSpot 25.0.3 captures (`C:/craton/gci-runs/i48p/s_jc9/jc9_leg_g1_egc.hs`, `gci-runs/i47x/q3-vi9-jc/jc9_leg_gen.hs` and 7 more) print the real pid (`Use jcmd 44168 JFR.dump name=1 ...`), and the empty middle line keeps the blank after `[jfr,startup]`. Only the `filename=`-only option shape has rows, so only it prints (`vm-cli` `jfr_startup_banner_texts`; written by `unified_logging::log_jfr_startup_lines` with the logger's stdout decorations, through a new non-selectable `LogTag::JfrStartup`). Rows `ob2_jb_*`; the other shapes have RECORD rows `ob2_cap_jfr_*`.
> - **Item 5** (`-XX:+PrintFlagsFinal`): `CRATONVM_GEN_PRINT_FLAGS_FINAL`, the proposal's scope (a), Generational only (`vm-cli` `gen_print_flags_final_lines`, `print_flags_final_line`). The column layout is read from `jvmFlag.cpp`, not captured: rows `ob2_pf_*` and RECORD row `ob2_cap_pff` decide it.
> - **Items 1-3:** no change (item 1 needs the two RECORD rows it names; item 3 a dispatch trace).

> **STATUS (2026-10-19, gcj j1/ob): OPEN.** Filed by lane ob of wave j1, from the review of its area against HotSpot 25 `-XX:+UseSerialGC`. Lines are on branch `gcj1-ob`.

## 1. `safepoint,stats`: summary order and the `Exit` operation

`vm/src/runtime/unified_logging.rs` `HOTSPOT_VM_OPERATION_ORDER` orders the exit summary's `<operation> <count>` lines. Only `Halt` before `SerialCollectForAllocation` is in the 25.0.3 oracle (`xl47_r_stats_capture`). The relative order of `SerialGCCollect`, `HeapDumper`, `ThreadDump`, `GC_HeapInspection`, `CollectForMetadataAllocation` and `RedefineClasses` comes from reading `vmOperation.hpp`'s `VM_OPS_DO`, not from a row.

On the `System.exit` arm HotSpot also runs `VM_Exit`, a safepoint named `Exit`, which would add an `Exit` row and an `Exit <n>` count. Here the summary counts `Halt` on every arm and never `Exit`.

**Close:** two RECORD rows on HotSpot:
- `Gcj1ObSafepointStats 3` with a forced young collection, so it has two operations;
- the same with a `System.exit(0)` probe.

Then fix the order table and add `Exit` on that arm.

## 2. `safepoint,stats`: thread columns

`initial_running` is always 0 and `total` is the registry's alive count (HotSpot also counts its hidden JavaThreads). This matches the `safepoint` line's `Threads:` figures (gci i45/sl). These are VM-internal figures: rows compare them only after normalisation. No fix proposed. Recorded so a reader does not take the 0 as a measurement.

## 3. `--compatible` `MemoryMXBean` singleton: attribution

`docs/internal/gc/gci-i46hc-compatible-memory-mxbean-is-not-the-mbean-servers-RETIRED-UNFIXED-20261018.md`: rows M1 and M2 are SAME since the `CRATONVM_GC_SYSTEM_GC_TENURES_ALL` flip. The kill row also answered the JDK helper's bean, so no probe door reaches `jmx.rs` `platform_memory_mxbean` under `--compatible`. The fix may be dead on the probed paths. **Close:** find a caller the native answers (a `CRATONVM_DBG` dispatch trace on `ManagementFactory.getMemoryMXBean` / `getPlatformMXBean` under `--compatible`). Then either a row whose kill arm DIFFs, or retire the native in favour of the JDK bytecode (AGENTS.md: real class bytes are authoritative).

## 4. `-XX:StartFlightRecording` start-up banner

See `docs/internal/gc/gci-i48-start-flight-recording-prints-no-jfr-startup-banner-RETIRED-UNFIXED-20261018.md`'s 2026-10-19 status. This is a collector-independent JFR / logging change, so it is not gated on Generational here. It needs:
- a `jfr,startup` `LogTag`, on at info on stdout by default when a start-up recording is given;
- `DCmdStart`'s text, taken from captured rows of each option combination.

## 5. `-XX:+PrintFlagsFinal` prints nothing

`rg PrintFlagsFinal vm-cli/src vm/src` finds no implementation. The flag falls into the launcher's silent catch-all, where HotSpot prints the `[Global flags]` table (~900 lines) on stdout and runs the program. Any script that greps `PrintFlagsFinal` output (`UseSerialGC`, `MaxHeapSize`, `ParallelGCThreads`) reads nothing. A design is proposed in `docs/internal/proposals/gc/gcj-j1ob-proposal-print-flags-final-from-the-typed-table-20261019.md`.
