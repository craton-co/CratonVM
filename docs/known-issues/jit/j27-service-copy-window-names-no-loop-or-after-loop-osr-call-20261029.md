# The service-copy window names no homes at an OSR body's direct calls (loop calls are "on a cycle")

- **Status:** OPEN (j27 wave 9, lane W9RPL, 2026-10-29). Retention gap, not a wrong value: an
  `OutOfMemoryError` HotSpot 25 does not throw.
- **Area:** JIT optimizing tier, `jit/src/ir_lower.rs` `Lowerer::argument_only_homes` /
  `snapshots_readable_after_call` and `ir_direct_call_site_admits_service_copy_window`; the OSR
  term `jit/src/osr_service_copy_window.rs` (`CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW`, opt-in).
- **Parents:** `t1-ARGFIX-ir-owned-args-never-reach-osr-or-spliced-callers-20261027.md` (item 3,
  OSR half), `j27-osr-direct-call-service-copy-window-proposal-20261028.md`.

## Evidence (Windows, `C:\craton\j27-rpl-bin\cvm-rpl-1.exe`, release LTO-off build of the j27 tree + wave-9 W9RPL edits)

Probe `tools/probes/jit/j27/J27OsrServiceCopyRows.java`, `-Xmx96m`, `CRATONVM_JIT_IR_INLINE=0`
(so `drop` stays a call), both modes:

| arm | `osr-oome` | `after-oome` |
|---|---|---|
| HotSpot 25 (`-XX:-UseCompressedOops`) | false | false |
| CratonVM, OSR window switch unset | true | true |
| CratonVM, `CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW=1` | true | true |
| CratonVM default (splicing on) | false | false |

With `CRATONVM_DBG_IR_SLOTS=1`:

- The switch reaches the OSR body: `[ir-owned-args] window n33 bci=20: none, the call is on a
  cycle` is printed for the OSR compile of `osrAfter` only with the switch on. So the wiring works.
- The call in the loop then names no homes: `snapshots_readable_after_call` answers `None` for a
  call on a cycle, in method-entry compiles too (`window n22 bci=20: none, the call is on a cycle`).
  An OSR body's calls are almost always in its loop, so the OSR window records nothing.
- The call AFTER the loop (`osrAfter` bci 41) prints no `window` line in either kind of compile:
  `ir_direct_call_site_admits_service_copy_window` refuses it on one of its site terms (not
  diagnosed further: `armed_exits` / `protected` / `in_splice` are the candidates).

No `[jitrootscan] service-copies cleared=` line in any arm.

## What would fix it

1. Let `argument_only_homes` name homes for a call on a cycle when every argument node is defined
   inside the cycle after the previous iteration's call returned (a fresh allocation per
   iteration). The next iteration overwrites the home before any snapshot reads it, so the copy
   has no reader past the callee's window. This needs its own liveness proof; keep it opt-in.
2. Print the refusing term in `ir_direct_call_site_admits_service_copy_window` under
   `CRATONVM_DBG_IR_SLOTS`, then decide for the after-loop shape.

Until then `CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW` stays opt-in. It has no effect on any row
measured here, and HotSpot facts stay SAME with it on (`J27W8ArgDeclineRows`,
`T1W12ArgfixOwnedTempArg`, `R21OomArgWindow`, `J27ArgfixSpliceRows`, both modes).
