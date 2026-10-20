# The String table reports nothing HotSpot's does: no `-Xlog:stringtable`, no `-XX:+PrintStringTableStatistics`, and `jcmd VM.stringtable` always says 0 entries

> **STATUS (2026-10-19, gcj j2/ob2, rows on j2a): item 1 SAME on j2a (`ob2_st_dcmd`, `_cpt`; the kill row DIFFs); `-verbose` now in HotSpot's shape (FIXED IN CODE, PENDING BUILD); item 3 CLOSED (nothing to print); item 2 OPEN with HotSpot's text captured.**
> - **`-verbose`** (`ob2_cap_st_verbose.hs`) is `VERSION: 1.1` and then one `<modified-UTF-8 length>: <bytes>` line per String, with no statistics. In the bytes, `\t \r \n \\` are escaped, printable ASCII is kept, and other bytes are written `\xhh`. Built as `serviceability.rs` `hotspot_stringtable_verbose_text`. The statistics' literal average is an integer division (`697560 bytes, avg  34.000` for 20008 literals); fixed in `default_vm_stringtable_text`.
> - **Item 3** (`-Xlog:stringtable*=debug`): HotSpot 25.0.3 Serial prints nothing for `Gcj2Ob2StringTableChurn 20000` (`ob2_cap_stlog`, SAME), so there is nothing to add.
> - **Item 2** (`-XX:+PrintStringTableStatistics`, `ob2_cap_stst.hs`): at exit, after the program's output, HotSpot prints:
>   - `SymbolTable statistics:` (8 lines);
>   - `Shared Symbol Table statistics:` (3 lines, from CDS);
>   - `StringTable statistics:` (8 lines);
>   - `Shared String Table statistics:` (3 lines).
>
>   Not built, because the symbol-table and CDS blocks have no source here. The StringTable block alone would still DIFF.

> **STATUS (2026-10-19, gcj j2/ob2): item 1 FIXED IN CODE on Generational (default ON, `CRATONVM_GEN_JCMD_STRINGTABLE_LIVE=0` kill), PENDING BUILD/PROBE; items 2 and 3 OPEN, capture probe written.**
> - **Item 1** (`jcmd VM.stringtable` a constant; j1-st X3): `vm/src/vm/vm_init.rs` `impl VmDiagnosticState for SharedVm` now has `vm_stringtable`, overriding the trait default with the live `string_pool` (entry and literal counts, compact-String payload bytes, `-verbose` texts sorted; `serviceability.rs` `stringtable_census`, `dcmd_verbose_option`, `gen_jcmd_stringtable_live`). G1 / ZGC keep the constant text (round scope). The bucket lines are still the formula (the pool is not bucketed), and the `-verbose` shape (`<len>: "<text>"`, statistics first) is the earlier lane's, unverified: RECORD row `ob2_cap_st_verbose`. Rows `ob2_st_dcmd*` (probe `Gcj2Ob2StringTableDcmd`, through the DiagnosticCommand MBean).
> - **Items 2 and 3** (`-XX:+PrintStringTableStatistics`, `-Xlog:stringtable`): not written, because HotSpot's text has no local capture. Probe `tools/probes/Gcj2Ob2StringTableChurn.java` and RECORD rows `ob2_cap_stst`, `ob2_cap_stlog` produce it; the implementation follows the capture.

> **STATUS (2026-10-19, gcj j1/st): OPEN -- filed by reading; no code change. HotSpot 25's output must be captured before any line is written (the lane rules forbid inventing a trace).**

*Filed 2026-10-19 by gcj wave j1, lane st (string table). Carries item 6 of
`docs/internal/gc/gcg-g2w-string-table-review-findings-RETIRED-UNFIXED-20261018.md` and step 4 of
`docs/internal/proposals/gc/gcg-g2w-proposal-string-table-keyed-by-the-string-20261001.md`.*

## What is missing (today's tree)

1. **`jcmd <pid> VM.stringtable [-verbose]` is a constant.** The only implementation is the trait
   default `vm/src/runtime/serviceability.rs:525` `vm_stringtable`, which answers
   `default_vm_stringtable_text(0, 0, &[], false)` (`serviceability.rs:5355`): "Number of entries"
   and "Number of literals" read 0 whatever the pool holds, `-verbose` lists nothing, and the
   bucket figures are formulas (65,536 buckets, "Maximum bucket size" 2 whenever there is an
   entry, the variance printed as the mean) -- the pool is an `FxHashMap<String, ObjectRef>`
   (`vm/src/vm/realms/heap_realm.rs:57`), not a bucketed table. `rg 'fn vm_stringtable'` finds
   no override in `vm/src/vm`. HotSpot answers the live table's counts.
2. **No `-XX:+PrintStringTableStatistics`.** Neither the launcher (`vm-cli/src/main.rs`) nor
   `types/src/flag_groups.rs` knows the flag (`rg PrintStringTableStatistics` over the `.rs` files
   finds nothing). HotSpot prints the SymbolTable and StringTable statistics at VM exit under
   it.
3. **No `-Xlog:stringtable`.** HotSpot 25 has a `stringtable` log tag (and `stringtable+perf`);
   here the only per-pause String-table line is the private `CRATONVM_DBG_STRING_TABLE=1`
   `[string-table] gc=...` line (`vm/src/memory/string_table.rs` `sweep_after_pause`), which no
   `-Xlog` selector reaches.

## What would close it

* Capture on HotSpot 25.0.3 (`java -XX:+UseSerialGC`) with a probe that interns a known number of
  Strings, drops some and collects: `-XX:+PrintStringTableStatistics` at exit,
  `jcmd <pid> VM.stringtable` and `-verbose`, and `-Xlog:stringtable*=debug` (which levels print
  anything on a Serial heap, and when -- HotSpot cleans its table concurrently on the service
  thread, so the lines are not per pause). Record the captures under
  `docs/internal/gc-round-gcj-20261019/` before writing a line.
* `jcmd VM.stringtable`: feed the real figures -- `string_pool.read().len()` for the entry and
  literal counts, the summed text bytes for the literal footprint, the texts for `-verbose`
  (HotSpot prints each literal with its length; the exact shape comes from the capture). This is
  the cheap half and needs no `-Xlog` work: an override of `vm_stringtable` where the VM's
  `ServiceabilityOps` implementation lives, with access to `SharedVm` (owner: the serviceability
  / observability lane).
* `-XX:+PrintStringTableStatistics`: a launcher flag (lane ob's `vm-cli/src/main.rs`) that prints
  the same block at exit; the bucket lines either reproduce HotSpot's arithmetic for a table of
  this size or the page records why they cannot (the table is not bucketed).
* `-Xlog:stringtable`: only the lines HotSpot prints on a Serial heap, from
  `StringTableState::census()` and the sweep's `StringTableSweep` -- after the capture says which.

## How to verify

A probe `Gcj<N>St*StatsProbe` that interns N Strings, then runs `jcmd VM.stringtable` on itself
(or prints at exit under the flag); stdout compared with HotSpot's after masking the footprint
bytes the capture shows are layout-dependent. Positive control: the entry count changes with N.
