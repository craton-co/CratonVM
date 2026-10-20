# ZGC round zt, wave t7 lane x4: what the adversarial review of wave t6 left open

> **STATUS (2026-10-19, zt t7/x4): OPEN -- 10 items, none on the default ZGC path's correctness.** Filed by the review of the t6 diff `4eda547a1f..cd144e3556` (lanes nd4, ob4, rv3, tc). The fixes are in `docs/internal/zgc-round-zt-20261018/t7-x4-report.md`. Every line number is on branch `zt7-x4`. HotSpot evidence is from local 25.0.3 runs (`java -XX:+UseZGC ... -XX:+PrintFlagsFinal -version`, `-Xlog:gc*` with `-XX:ZCollectionInterval=0.05`).

## 1. `-XX:ZAllocationSpikeTolerance` in `strtod`'s hexadecimal form

`vm-cli/src/main.rs:7558` `hotspot_parse_double_flag` follows HotSpot's double syntax, with one exception: Rust's `f64` parser has no hexadecimal form. HotSpot 25.0.3 takes `-XX:ZAllocationSpikeTolerance=0x1p3` (it prints `8.000000 {command line}`). The parser here refuses it, and the launch ends with `Improperly specified VM option`.

Subnormal values (`1e-310`) are kept here. Whether the platform `strtod` sets `ERANGE` for them was not checked.

**What would close it:** a hex-float arm in `hotspot_parse_double_flag` (`0x`/`0X`, hex mantissa, mandatory `p` exponent), plus a `1e-310` row against HotSpot on Windows and Linux.

## 2. Off ZGC a malformed `-XX:ZAllocationSpikeTolerance` warns; HotSpot exits

HotSpot refuses a malformed value under any collector. `-XX:+UseG1GC -XX:ZAllocationSpikeTolerance=abc` prints `Improperly specified VM option` and exits 1. The x4 fix applies HotSpot's rule on ZGC only (`vm-cli/src/main.rs:9518`). G1 and Generational keep t6's `Warning: ignoring ...` line, because their behaviour is out of this round's scope.

**What would close it:** the launcher's flag-syntax owner decides whether ZGC-family `-XX:` values take HotSpot's rule under every collector. It already made the same decision for `ConcGCThreads` (`zgc_refuse_malformed_gc_threads_enabled`).

## 3. `-XX:ZProactive=false` (a boolean in `name=value` form) is ignored

HotSpot 25.0.3 prints `Missing +/- setting for VM option 'ZProactive=false'` and exits 1. `normalize_java_launcher_argv` (`vm-cli/src/main.rs:4457`) rewrites only `-XX:+ZProactive` and `-XX:-ZProactive`. Any other spelling is silently ignored by the `-XX:` catch-all at `vm-cli/src/main.rs:4671`. This applies to every boolean `-XX:` flag, not only this one.

**What would close it:** a launcher-wide check that a known boolean flag given as `-XX:Name=value` is refused, with HotSpot's line and exit status.

## 4. `setVMOption("SoftMaxHeapSize", "0")` sets the capacity; HotSpot keeps 0

Under the opt-in `CRATONVM_ZGC_SOFT_MAX_MANAGEABLE=1`, `ZgcRealHeap::apply_soft_max_heap_bytes` (`gc/src/zgc.rs:18212`) maps 0 to the capacity. HotSpot keeps 0: `-XX:SoftMaxHeapSize=0 -Xlog:gc+init` prints `Soft Max Capacity: 0M`.

t6's test `zt6tc_soft_max_is_writeable_at_run_time` pins the capacity answer (`gc/src/zgc.rs:50592`, `"0 is the capacity again"`). The launcher's own `-XX:SoftMaxHeapSize=0` also warns and is ignored, but that behaviour predates t6.

**What would close it:** let a management write of 0 store 0, keeping the capacity only for "unset". Flip the test's assertion with it. The deciding row is `Zt6TcSoftMaxManageable` extended with a `setVMOption("SoftMaxHeapSize","0")` line and its HotSpot twin.

## 5. The manageable `SoftMaxHeapSize` row exists on ZGC only

HotSpot lists `SoftMaxHeapSize` as `{manageable} {ergonomic}` under `-XX:+UseG1GC` as well. Under the opt-in, `soft_max_flag_row` (`native-builtins/src/jmx.rs:986`) answers only on a ZGC heap.

The out-of-range message (`native-builtins/src/jmx.rs:1030`) is not HotSpot's, as t6/tc already notes.

**What would close it:** the G1 owner's decision on the row, and the HotSpot twin of `Zt6TcSoftMaxManageable` for the message. Its stdout prints only the exception class.

## 6. ZGC `-Xlog` `gc,heap` block: one `O:` capacity triple, no statistics tables

HotSpot 25.0.3 prints the `Min / Max / Soft Max Capacity` triple twice per major collection: once under `Y:` and once under `O:`. Each triple is followed by the `Heap Statistics:` table and a per-generation statistics table.

`render_xlog_zgc_capacity_lines` (`vm/src/runtime/interpreter/gc_events.rs:5699`) prints the `O:` triple once and no tables. t6/ob4 records this as a known shape difference.

**What would close it:** a `Y:` triple before the `O:` one on a major, and the three tables from the collection's edge samples. The deciding row is R9-cap of `t6-ob4-report.md`, with `-Xlog:gc*` compared line by line.

## 7. An allocation-failure collection can still count as a warm-up step

`CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR` (`gc/src/zgc.rs:30788`) clears the warm-up bit only when the collecting thread carries `major_gc_requested()`. A stalled allocation's collection carries no mark the gc crate can read. If a poll raised the warm-up bit first, that collection is still counted as a warm-up step. On HotSpot it would be an `Allocation Stall` cycle, not a `Warmup` one. t6/tc states this limitation itself.

**What would close it:** a thread-local door mark set by `run_collection_pause` for `GcDoor::AllocationFailure`, cleared after the pause, and read beside `major_gc_requested()`. The deciding row is W7 of `t6-tc-report.md` with a fill that stalls before the first warm-up step.

## 8. The exit report can block on the vtable manager's lock

`rss_report::print_exit_report` (`vm-cli/src/rss_report.rs:289`) takes `vtable_manager.read()` at shutdown. If a thread still holds the write lock at exit, the report blocks. It is diagnostic only (`CRATONVM_DBG_RSS_BY_MAPPING`).

**What would close it:** `try_read()`, printing `on=? deferred=? built=?` when the lock is busy.

## 9. The Spring builder gate reads three switches per builder call

`builder_instance_collects` (`native-builtins/src/spring_startup_bootstrap.rs:2707`) runs before every lookup-override and replace-override mint, not only after a refusal. Each call reads `heap_is_zgc`, the native-doors switch and an environment variable. It is correct, but the cost is paid on every bean instance of those kinds.

**What would close it:** evaluate the gate inside `builder_instance_mint`, after the first attempt was refused. The four unit tests then pass the gate as a closure.

## 10. The `mirror-reserve` census row is not closed on the presize and reservation error paths

`define_class_via_full` returns from the presize error at `native-builtins/src/classloader.rs:6987` without calling `phase_end(MirrorReserve, ..)`. It does the same from the reservation error just below. A refused define is therefore not timed in its row. This is diagnostics only (`CRATONVM_DBG_DEFINE_CENSUS`).

**What would close it:** call `phase_end` before each early return, or hold the clock in a guard whose `Drop` charges the row.
