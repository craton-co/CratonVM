> **UPDATE 2026-10-20:** Item 2's G1 thread-flag syntax/refusal gap is fixed
> and validated in both modes. ParallelGCThreads also reaches G1 and reports
> its applied pool size. See the [retired worker-count issue](../../internal/gc/g1-hotspot-spelling-of-parallelgcthreads-never-reaches-g1-FIXED-20261020.md).
> The broader typed-option work in item 3 remains open; older statuses below
> are historical.

> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/gc/launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-20261014.md`.

# Default-collector GC thread flags: where the default ZGC cannot answer as HotSpot's default G1 does

> **STATUS (2026-10-19, gcj j1/ob): items 2 and 3, the Generational half, FIXED IN CODE, PENDING BUILD/PROBE; G1 unchanged (still OPEN for G1).** `f900bdeec5`, switch `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS` (default ON, `=0` kill).
> - **Item 3:** one typed table, `vm-cli/src/main.rs` `GEN_HOTSPOT_VALUE_FLAGS` (+ `SPACE_TLAB_VALUE_FLAGS`), each `-XX:<Name>=<value>` the launcher takes with HotSpot 25's type (`HotspotFlagType`: `int`, `intx`, `uint`, `uintx`, `uint64_t`, `size_t`, `double`). One parser (`HotspotFlagType::parse`, `hotspot_value_flag_errors`) answers `Improperly specified VM option '<Name>=<value>'`, plus the range line for the ranges earlier lanes verified. `gen_hotspot_value_flag_refusal` checks the flags in command-line order, before the version banner, and only when the effective collector is Generational (`scan_effective_generational`: `-XX:+UseGenerationalGC`, `-XX:+UseSerialGC`, an unsupported collector, a deselecting `-XX:-UseG1GC` / `-XX:-UseZGC`).
> - **Item 2:** on Generational a malformed `-XX:ParallelGCThreads=` / `-XX:ConcGCThreads=` is refused by that scan. In `run` the `hotspot_thread_syntax` binding now also covers Generational, so `0x10` and `3k` are taken there.
> - **How it extends:** G1 (and ZGC's other flags) take the table by widening the collector test in front of the two callers (the early scan in `run`, the `hotspot_thread_syntax` binding). Nothing else changes. Not done here, by scope.
> - **Left open** (`docs/known-issues/gc/gcj-j1ob-launcher-typed-value-residuals-20261019.md`): bool `=value` spellings, ranges without a local row (`MaxGCPauseMillis`, `InitiatingHeapOccupancyPercent`, `G1HeapWastePercent`, ...), and a zero thread count on Serial (it still warns on stderr).
> - **Rows:** `docs/internal/gc-round-gcj-20261019/j1-ob-report.md` block VF.

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (launcher, G1 / Generational).** Item 1 FIXED (row OB6-2 is local: `cratonvm -XX:ParallelGCThreads=0 Zs3Ob2ThreadFlags` vs the HotSpot twin). Items 2 (G1 / Generational keep the malformed-value warning) and 3 (one typed `-XX:<flag>=<value>` table) change every collector's launcher policy. Move to `docs/known-issues/gc/launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-20261014.md`.

> **STATUS (2026-10-19, zt t3/ob): items 2 and 3 OPEN (scope), unchanged; one related fix.**
> - `-XX:+UseZGC -XX:-UseG1GC` used to select Generational, so the explicit-ZGC thread-flag refusals did not apply to it. It now keeps ZGC, as HotSpot does (`CRATONVM_LAUNCHER_OTHER_GC_OFF_NOOP`, `vm-cli/src/main.rs` `minus_gc_flag_deselects`), and the refusals apply.
> - Items 2 and 3 still change G1 / Generational launcher behaviour (item 2), or every `-XX` value (item 3). The new `-XX:AllocatePrefetch*` names join item 3's list.
> - Item 1: host row OB6-2 is still owed.

> **STATUS (2026-10-19, zt t1/ob): item 1 FIXED IN CODE, still PENDING HOST PROBE (s9-ob6 report OB6-2; the code, `zgc_implicit_zero_parallel_gc_threads_refusal` in `vm-cli/src/main.rs`, is unchanged by the dev merge); items 2 and 3 OPEN.** Item 2 changes G1 / Generational launcher behaviour, which this round may not change (LANE-RULES scope). Item 3 (one typed `-XX` value table) is a launcher-wide change with the same constraint. Both need a G1 / Generational owner's sign-off, not more ZGC work.

> **STATUS (2026-10-16, zs s9/ob6): item 1 FIXED IN CODE, PENDING HOST PROBE; items 2 and 3 OPEN, unchanged (G1 / Generational launcher policy and the typed flag table are outside this lane).**
> - **Item 1 decision: HotSpot's exact answer.** `AGENTS.md`: both modes target HotSpot 25's Java-visible behaviour for the same command line, and a command line that names no collector gets HotSpot's default-G1 refusal. So an implicit ZGC with a zero `-XX:ParallelGCThreads` prints `Error occurred during initialization of VM` / `The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0` on stdout and exits 1, also before a `-version` banner (local 25.0.3: `java -XX:ParallelGCThreads=0 -version` and `=0x0` both do exactly that; HotSpot prints `=0` in the text whatever the spelling).
> - Code: `vm-cli/src/main.rs`, `zgc_implicit_zero_parallel_gc_threads_refusal` / `scan_zgc_implicit_zero_parallel_gc_threads_refusal`. Switch: `CRATONVM_ZGC_IMPLICIT_ZERO_PARALLEL_GC_THREADS_REFUSE`, default ON, `=0` kill (the warning and the run).
> - `ConcGCThreads=0` stays silently accepted (s3/ob2), as HotSpot's G1 accepts it.
> - Rows: s9-ob6 report OB6-2.

> **STATUS 2026-10-15 (zs s5/pg): OPEN -- Still a launcher decision. vm-cli/src/main.rs:8658/8713 still prints `Warning: ignoring -XX:ParallelGCThreads=0 (expected a positive integer)` on implicit ZGC, and `hotspot_thread_syntax` (main.rs:8666) is still gated on `zgc_effective`, so G1 and Generational warn. There is no typed `uint`/`uintx` table. rv3's 3e06f7d2cf (explicit-ZGC `0x0`/`0k` refusal) is pending host row RV3-1.**

> **STATUS (2026-10-14, zs s4/rv3): STILL OPEN (a launcher decision).** Related fix, `3e06f7d2cf` (PENDING HOST PROBE, row RV3-1 in `docs/internal/zgc-round-zs-20261014/s4-rv3-report.md`):
>
> - an explicit `-XX:+UseZGC` now refuses a zero spelled `0x0` or `0k`;
> - the silent drop of `ConcGCThreads=0` applies to an implicit ZGC only, no longer to an explicit one.
>
> **STATUS (2026-10-14, zs s3/ob2): OPEN, a launcher decision.** Found by HotSpot 25.0.3 twins while fixing `zgc-zs2ob-observability-review-residuals-CLOSED-20261019.md` items 2 and 3. Not ZGC behaviour: these are the rows where HotSpot's answer names its own default collector (G1), which this VM does not run by default.

## 1. `-XX:ParallelGCThreads=0` without a collector flag

HotSpot 25.0.3 (local), `java -XX:ParallelGCThreads=0 -version` and `java
-XX:ParallelGCThreads=0 Main`: stdout `Error occurred during initialization of
VM` / `The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0`,
stderr empty, exit 1. (s2/ob recorded this case as accepted; it observed only
`-XX:ConcGCThreads=0 -version`, which HotSpot does accept.)

This VM's default collector is ZGC: `vm-cli/src/main.rs`, the zr z3/mt
`args.zgc_parallel_gc_threads` arm of `run`, prints `Warning: ignoring
-XX:ParallelGCThreads=0 (expected a positive integer)` on stderr and runs.

**Close** (decision): either refuse with HotSpot's exact text (stdout and exit
status equal, but the text names `-XX:+UseG1GC`, a collector that is not
running), or refuse with `-XX:+UseZGC` in it (the s2/ob refusal's text, true
of this VM, a one-word stdout diff), or keep running. Row:
`cratonvm -XX:ParallelGCThreads=0 -cp tools/probes Zs3Ob2ThreadFlags` vs `java
-XX:ParallelGCThreads=0 -cp tools/probes Zs3Ob2ThreadFlags`.

## 2. G1 and Generational keep the warning for malformed values

`CRATONVM_ZGC_REFUSE_MALFORMED_GC_THREADS` refuses a malformed
`-XX:ParallelGCThreads=` / `-XX:ConcGCThreads=` only when the effective
collector is ZGC (the zs round's scope). With `-XX:+UseG1GC` or
`-XX:+UseGenerationalGC` this launcher still warns and continues, where HotSpot
refuses for every collector. **Close**: drop the ZGC gate in `run` (the
`hotspot_thread_syntax` binding) once the G1 / Generational owners take the
change; the parser and the refusal are collector-independent already.

## 3. Other `uint` / `uintx` flags

Only the two GC thread flags use HotSpot's value syntax and refusal.
`-XX:MaxGCPauseMillis=`, `-XX:G1HeapWastePercent=`, `-XX:SoftMaxHeapSize=` and
the rest still warn on a malformed value. **Close**: one typed table of the
`-XX:<flag>=<value>` options this launcher accepts, each with HotSpot's type
(`uint`, `uintx`, `size_t`, `double`, `bool`), parsed by one function that
refuses with `Improperly specified VM option`.
