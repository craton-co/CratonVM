# H2 TestScript `--nojit` SEGV — findings (2026-06-05)

Companion to `docs/bc-math-ec-gc-0x4-handoff.md`. **The H2 `org.h2.test.scripts.TestScript`
`--nojit` FATAL `EXCEPTION_ACCESS_VIOLATION` is a SECOND reproduction of the bc-math-ec
`0x4` GC corruption** — same mechanism, different app/payload.

## The SEGV == bc-math-ec `0x4` GC corruption

Symbolized with `release-with-debug` + the new `file:line` crash symbolizer:

- Faults in `cratonvm_gc::gen_heap::GenerationalHeap::get_field` (`gen_heap.rs:920`, the
  header read) on a wild receiver `Value::Object(Some(ptr=6))`.
- **Faulting read at `0x16` = `6 + 0x10`** — identical signature to bc-math-ec
  (`Object(Some(0x4))` → SEGV at `0x14` = `4 + 0x10`). Small `Object` payload + a read at
  `payload + 0x10` (the `num_slots` header offset).
- Consumed by a `getfield` inside a Java method driven by native
  `cratonvm_native_builtins::register_essential_natives::closure$105` → `ctx.invoke_virtual`.
- Clusters at `testScript.sql` ~line 5539 (the DECIMAL-arithmetic region), GC-pressure-driven.
- Under `--nojit`, `gc_quiescence::is_active()` is always false (JIT-only), so the **moving
  Cheney young collector** runs — the collector the `0x4` hunt blames.

This is the deep, unsolved bug being worked in worktree `CratonVM-ecgc`. H2 is a cleaner,
higher-pressure repro than `FixedPointTest` (corrupts at `-Xmx1g`; `FixedPointTest` needs
`-Xmx256m`). Repro: `repro-h2.bat` (env `XMX`/`TMO`), or:
`target/release-with-debug/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --nojit -Xmx1g -cp ".;temp" org.h2.test.scripts.TestScript` from `apps/h2database/h2`.

## FIXED here (a SEPARATE real bug — NOT the SEGV)

`bi_alloc` / `bd_alloc` / `bd_write_into` (`native-builtins/src/lib.rs`) had the **same
native use-after-move** the handoff already fixed for `bi_alloc_int` (fact 7): allocate a
BigInteger/BigDecimal, then call a nested allocator (`bi_alloc` / `new_array` /
`create_string`) that can young-GC and relocate the not-yet-rooted object, then `set_field`
through the STALE ref. Fixed by pinning across the nested alloc
(`pin_native_root`/`read_native_pin`/`unpin_native_roots`).

- **Result:** the `set_field` OOB-flood (`class=java/lang/Object num_slots=0`) dropped
  **181 → 0** at the crash region. **The wild-ref SEGV PERSISTS** (flood=0) → the OOB-flood
  and the SEGV co-occurred but are DISTINCT; the SEGV is the deep GC bug, not this.
- **This very likely also helps bc-math-ec** — that handoff fixed only `bi_alloc_int`,
  leaving these three siblings unpinned.
- Status: UNCOMMITTED; built into `target/release-with-debug` only (rebuild `target/release`
  to ship). Needs a regression-pool pass before merge (BD is widely used).

## Separate HANG (the "nondeterministic hang")

Once the BD fix lets H2 worker threads stay alive, a **write-preferring `parking_lot::RwLock`
deadlock** on the class-metadata/resolution locks surfaces (cdb-localized, `cdb_hang.ps1` →
`cdbdump.log`): main-vm `initialize_class_shared` → `resolve_field_ref` → `lock_exclusive`
(WRITE, class-load) vs worker threads (via #105 `invoke_virtual`) → `resolve_method_ref` →
`lock_shared` (recursive READ). Fix direction: `read_recursive()` for the re-entrant
`class_manager.read()`, or don't hold it across the re-entrant `invoke_virtual`.

## Mismatches (~40, lower priority)

- **f64 BigDecimal arithmetic:** `native_bd_add/subtract/multiply/negate` compute via `f64`
  (`format!("{}", a + b)`) → scale + precision lost (`10*1.00`→`10`, `-1.00`→`-1`). Real fix:
  decimal arithmetic on unscaled-int + scale, not f64.
- **`CharBuffer.allocate`** (`charset.rs:203`) returns an abstract base `java/nio/CharBuffer`,
  so H2's real `charBuffer.compact()` bytecode → `AbstractMethodError`. Real fix: real
  `HeapCharBuffer`.

## Tooling added (all gated / standalone)

- Crash symbolizer now emits `file:line` (`crash_handler.rs` `SymGetLineFromAddrW64`).
- `CRATONVM_DBG_BADRECV` — logs Java frames + field + Rust backtrace on a non-heap getfield
  receiver and raises NPE instead of faulting (note: its per-getfield check perturbs timing
  toward the deadlock, so it tends to hang before reaching the SEGV region).
- `symbolize-crash.sh`, `cdb_hang.ps1`, `repro-h2.bat`, `DecChurn.java`.
