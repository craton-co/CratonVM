# gcj j1/ob: what the Generational typed `-XX:` value table leaves open

> **STATUS (2026-10-19, gcj j2/ob2): items 4 and 5 FIXED IN CODE behind the opt-in `CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE` (default OFF), PENDING BUILD/PROBE; items 1, 2, 3 and 7 still need HotSpot rows (capture rows written); item 6 out of round scope.**
> - **Item 4** (`-version` skips the constraint lines): `vm-cli/src/main.rs` `gen_version_constraint_errors`, called in `run`'s version-query block before the banner (Generational, both switches on): the heap-free-ratio pair, then `SurvivorRatio` (only with an explicit `-Xmx` / `-XX:MaxHeapSize=`; the ergonomic maximum is decided on the parsed path) and `TLABSize` / `MinTLABSize` against Serial's TLAB maximum. Rows `ob2_op_ver_sr`, `ob2_op_ver_tlab`, `ob2_op_ver_hfr` (+ `_off` controls) in `docs/internal/gc-round-gcj-20261019/j2-ob2-report.md`.
> - **Item 5** (catch-all flags): `GEN_ORDERED_PARSE_EXTRA_FLAGS` (`NewSize`, `MaxNewSize`, `NewRatio`, `InitialHeapSize`, `MinHeapSize`, `MetaspaceSize`; syntax only, types read from `globals.hpp`) in the same ordered scan (`gen_value_flag_refusal_with`). Same switch: `gen_ordered_parse_argv` hands the consumers that re-parse with `parse_size` / `str::parse` the value HotSpot read (`0x10000000`, `1t`, `1k` no longer warn) and drops the inert G1 / ZGC knobs on Generational within their source ranges (`-XX:ZUncommitDelay=0x10` was a clap error). Rows `ob2_op_*`.
> - **Items 1, 2, 3, 7:** RECORD rows `ob2_cap_range_*`, `ob2_cap_bool_*`, `ob2_cap_pgt0`, `ob2_cap_mtt_big` (HotSpot's text to be captured by the orchestrator; nothing invented). New residuals (a negative `intx` value reaches clap as a separate `-1` token) are on `gcj-j2ob2-launcher-and-serviceability-review-residuals-20261019.md`.

> **STATUS (2026-10-19, gcj j1/ob): OPEN.** Filed by lane ob of wave j1 with the typed table (`f900bdeec5`, `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS`). Each item below needs a local HotSpot 25.0.3 row before code, or it is a scope decision. Lines are on branch `gcj1-ob`.

Context: `vm-cli/src/main.rs` `GEN_HOTSPOT_VALUE_FLAGS`, `HotspotFlagType`, `gen_hotspot_value_flag_refusal`, `scan_effective_generational`. Parent pages: `launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-20261014.md` (items 2 and 3) and `docs/internal/gc/gci-i48-launcher-version-skips-vm-option-checks-RETIRED-UNFIXED-20261018.md`.

## 1. Ranges no row has verified

The table checks HotSpot's value **syntax** for `MaxGCPauseMillis`, `InitiatingHeapOccupancyPercent`, `G1HeapWastePercent`, `SoftMaxHeapSize`, `MaxDirectMemorySize`, `MaxJavaStackTraceDepth`, `SoftRefLRUPolicyMSPerMB`, `MaxMetaspaceSize` and `ZUncommitDelay`. It does not check their **ranges**. From reading `globals.hpp` / `gc_globals.hpp`, HotSpot 25 declares at least these:

- `MaxGCPauseMillis` `range(1, max_uintx - 1)`;
- `InitiatingHeapOccupancyPercent` and `G1HeapWastePercent` `range(0, 100)`;
- `MaxJavaStackTraceDepth` `range(0, max_jint/2)`;
- `SoftRefLRUPolicyMSPerMB` `range(0, max_intx)`;
- `MaxDirectMemorySize` `range(0, max_jlong)`.

Ranges read from source are not used: a range line the VM prints must be byte-equal to HotSpot's, type name included. The out-of-range values keep the old lenient `Warning: ignoring ...` arms on Generational. **Close:** one row per flag at `lo - 1` and `hi + 1` (`java -XX:+UseSerialGC -XX:<Flag>=<v> -version`, `.hse` kept). Then one `Some((lo, hi))` per table row.

## 2. `bool` flags spelled `-XX:<Name>=<value>`

The table has no `bool` type. What HotSpot 25 answers to `-XX:UseTLAB=true`, `=1` or `=x` (refusal text, or acceptance) has no local row. **Close:** those three rows, then a `Bool` arm in `HotspotFlagType::parse`.

## 3. A zero `ParallelGCThreads` / `ConcGCThreads` on Serial

`-XX:+UseGenerationalGC -XX:ParallelGCThreads=0` still prints `Warning: ignoring -XX:ParallelGCThreads=0 (expected a positive integer)` on stderr and runs. HotSpot Serial most likely accepts it silently: by reading, only `G1Arguments` / `ParallelArguments` refuse a zero. Stdout is unaffected. **Close:** row `java -XX:+UseSerialGC -XX:ParallelGCThreads=0 Gcj1ObValueFlags` (stderr empty?), then a silent arm for Generational.

## 4. `-version` skips the constraint lines

The early scan covers the parse (types and ranges). The `AfterMemoryInit` constraints (`space_tlab_constraint_errors`, `g1_plab_constraint_errors`, the heap-free-ratio pair) run on the parsed path only, so `-XX:+UseSerialGC -Xmx64m -XX:SurvivorRatio=100000 -version` prints the banner where HotSpot refuses. **Close:** run the constraint functions in the version path too. They need the heap sizes, which `early_max_heap` already scans.

## 5. Flags still in the silent catch-all

A `-XX:<Name>=<bad>` for a flag the normaliser drops silently, for example `-XX:NewRatio=abc` or `-XX:MetaspaceSize=x`, is accepted. HotSpot refuses any known flag's bad value. **Close:** add each flag to the table with its type (types from `globals.hpp`, syntax only, so no range row is needed). The census of the catch-all is `normalize_java_launcher_argv`'s last arm.

## 6. G1 and ZGC

By scope, the table is applied on Generational only. G1 still warns and continues for every flag of item 1's list. ZGC refuses only its two thread flags and `ZAllocationSpikeTolerance` through their own arms. **Close (owners' decision):** widen the collector test in front of `gen_hotspot_value_flag_refusal`'s caller in `run` and the `hotspot_thread_syntax` binding. The table and the parser are collector-independent.

## 7. `MaxTenuringThreshold` above `u32`

`MaxTenuringThreshold=4294967296` is read as a `julong`, as the gci i23/tn parser did, and answered with the range line. HotSpot's `parse_each_vm_init_arg` reads a `julong` and then `FLAG_SET_CMDLINE`s a `uint`. Whether that truncates to 0, and is then accepted, has no row. The other `uint` flags answer `Improperly specified` above `u32`, as the GC thread flags were observed to (zs s3/ob2). **Close:** row `java -XX:+UseSerialGC -XX:MaxTenuringThreshold=4294967296 -version`.
