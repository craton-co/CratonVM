# The safepoint poll's flag byte was on the Rust heap — FIXED 2026-09-10

The RIP-relative safepoint poll added 2026-09-02
(`array-element-load-baseline-codegen-FIXED-20260902.md`,
`jit/src/x64/safepoint.rs::emit_safepoint_poll`) is supposed to be the whole
poll in one 7-byte instruction and no register:

```asm
test byte [rip+disp32], 0FFh     ; the GC barrier's stw_requested
```

`disp32` reaches ±2 GB. `GcBarrier::stw_requested` was an inline field of
`GcBarrier`, which is a field of `SharedVm`, which is only ever reached through
`Arc<SharedVm>` — i.e. the Rust global allocator, which in a shipping build is
mimalloc (`vm-cli/src/main.rs`). Whether the poll got its short form was
therefore a coincidence between two allocators that have nothing to do with
each other, re-rolled on every host and every process.

**On Linux it lost.** Reported from `/data/cratonvm` on 20.80.105.49 with
`CRATONVM_DBG_JIT_DISASM`: code buffer at `0x7DE4D7F9E000`, flag at
`0x2000CD6E2C0` — **123.9 TB apart**, so every compiled loop back edge and every
method entry in the process emitted the fallback instead:

```asm
mov  r11, <imm64>                ; 10 bytes
test byte [r11], 0FFh            ;  5 bytes, and R11 is gone
```

15 bytes and a clobbered register in place of 7 and none, at every poll site the
VM emits. **Nothing failed.** The fallback reads the same byte and branches the
same way; it is only longer. No test noticed for eight days, and no test could
have — nothing in the tree asserted anything about where the flag sits.

## The fix

Take the byte from the same OS primitive the code cache comes from.

`platform::alloc_code_adjacent_cell` (`jit/src/platform.rs`) bump-allocates
64-byte cache-line-isolated cells out of 64 KiB chunks obtained by the same
`platform_alloc` — the bare `mmap(NULL, …)` / `VirtualAlloc(NULL, …)` — that
`alloc_executable` hands the code cache. Chunks are never unmapped and cells are
never recycled, so a cell's address can be baked into generated code and is
valid for the rest of the process.

`CacheLineFlag` (`vm/src/threading/gc_barrier.rs`) becomes a `&'static
AtomicBool` into such a cell, with `Box::leak` as the correctness floor if the
OS refuses the mapping. Its four methods (`flag`/`load`/`store`/`swap`) are
unchanged, so all 75 call sites of `stw_requested` are untouched, and
`stw_requested_flag_addr` — the one the JIT bakes — now returns an address that
outlives the VM rather than one that must not.

Two allocations from one primitive land in one region of the address space.
That is the entire mechanism, and it is a **hint, not a guarantee**: the OS
picks. Both emitters keep their per-site ±2 GB test and their fallback.

### The interpreter had to be paid for

`stw_requested` is the single hottest load in the VM — every interpreter thread
reads it once per bytecode. Turning the field into a pointer would have made
each of the two per-bytecode reads a dependent PAIR of loads, and worse, the
pointer word shares a cache line with `gc_generation`, `threads_blocked` and the
barrier mutex — precisely the neighbours `CacheLineFlag` exists to stay away
from (see its doc for the 2026-09-05 `probes/SharedLine.java` bound).

`execute_frame` now resolves it once, next to the existing
`async_exception_slot` hoist, so each read is a single load from a line nobody
writes but the collector. That is not a wash with the old code; it is slightly
better, since the address no longer has to be computed off `shared`.

## Measured

Windows 11 x86-64, release build, JDK 25 boot classes, a hot counted loop
(`long s; for (int i=0;i<n;i++) s += i ^ (s>>>3);`) driven to the OSR/optimizing
tier and dumped with `CRATONVM_DBG_JIT_DISASM`. Baseline is the same tree with
only `gc_barrier.rs` reverted.

| build | flag | code buffer | apart | poll sites, short form |
|---|---|---|---:|---:|
| before | `0x19048D052C0` | `0x1902A440000` | 489 MiB | 394 / 394 |
| **after** | `0x1CA80000000` | `0x1CA80020000` | **128 KiB** | 394 / 394 |

**This host was already in reach, so it shows no encoding change.** mimalloc on
Windows allocates through `VirtualAlloc` and lands in the same low region as the
code cache; the 124 TB gap is a mimalloc-on-Linux property. What the Windows
numbers do show is the difference between 489 MiB of luck and 128 KiB of
construction — the margin stops depending on two allocators staying accidentally
neighbourly.

**The Linux confirmation is still owed**, and it is the one that matters. Run
the probe on 20.80.105.49 and read any loop body: `test byte [rel …]` means in
reach, `mov r11, <imm64>` means it is not.

## Guarded by

Two tests, both x86-64-only because `disp32` reach is what makes distance mean
anything:

- `jit::platform::tests::a_cell_is_within_disp32_of_a_code_buffer` — a bare cell
  against a bare code buffer.
- `vm::threading::gc_barrier::tests::stw_requested_flag_is_within_disp32_of_the_code_cache`
  — the byte that is actually polled, reached the way the JIT reaches it. This
  one also fails if `CacheLineFlag` is ever put back inside the `Arc`-allocated
  struct.

Plus `code_adjacent_cells_are_distinct_aligned_and_zero`, which is about the
allocator rather than the poll: "fresh anonymous pages are zero-filled" is a
property of the first cell of a chunk, not the 900th.

## Found while verifying: the kill switch reached 0.5% of the poll sites

`CRATONVM_JIT_RIP_SAFEPOINT_POLL=0` is documented as the lever that emits the
pre-2026-09-02 form so the two encodings can be priced inside one binary. There
are two x86-64 backends that emit this poll, and it reached one.
`jit/src/ir_lower.rs::emit_safepoint_poll` — the optimizing tier, where
everything hot is compiled — called its RIP emitter unconditionally and parsed
`CRATONVM_JIT_SAFEPOINT_POLLS` inline, once per emitted site rather than once
per process.

Measured on the same probe, before the fix: with the switch set, **392 of 394**
poll sites were still `test byte [rel …]`. Both gate functions in
`jit/src/x64/licm.rs` are now `pub(crate)` and the lowerer calls them, so there
is one definition of each switch rather than one definition and one omission.

| arm | `test byte [rel …]` | `mov r11` + `test byte [r11]` |
|---|---:|---:|
| default | 398 | 0 |
| `CRATONVM_JIT_RIP_SAFEPOINT_POLL=0` | **0** | **398** |

Both arms print the same result, which is the point: the two encodings are
interchangeable, and now the flag can demonstrate it.

## Not done

- **aarch64.** `Arm64Backend::emit_safepoint_poll` materializes the address with
  `MOVZ`/`MOVK` unconditionally. `ADRP` has a ±4 GB window that would now be
  reachable, and no emitter uses it.
- **Every other address the JIT bakes.** Observed in the same dump: all 394
  helper-call targets are `mov r64, <imm64>` + `call r64`, never `call rel32`,
  because the executable image (`0x7FF638……`) sits **125.7 TiB** from the code
  cache (`0x23DC0C70000`) on this host. Anything reached through a plain
  `static` is in that image and pays the same — `layout_replace_epoch_guard`
  (`types/src/field_layout.rs`) is one, though this probe's loop touches no
  field so its guard does not appear in the dump to confirm it. None of this was
  in scope here; `alloc_code_adjacent_cell` is the primitive such a word would
  use.
