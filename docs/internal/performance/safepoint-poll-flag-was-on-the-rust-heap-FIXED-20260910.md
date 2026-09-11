# The safepoint poll's flag byte, from the code cache's own allocator — FIXED 2026-09-10

The other half of
`c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`, which found that
on System V **neither** of the VM's two JIT-polled global cells was within
`disp32` of the code that reads them, and fixed it by moving the CODE. This page
is about the case that fix deliberately does not cover.

## What was wrong

The RIP-relative safepoint poll added 2026-09-02 is supposed to be the whole
poll in one 7-byte instruction and no register:

```asm
test byte [rip+disp32], 0FFh     ; the GC barrier's stw_requested
```

`disp32` reaches ±2 GB. Measured on Ubuntu 24.04 x86-64, one process — the
numbers the epoch-guard page reports, and the same ones reported independently
from `/data/cratonvm` on 20.80.105.49:

```text
LAYOUT_REPLACE_EPOCH  (mimalloc heap)   0x2001E8103F0
stw_requested_flag    (mimalloc heap)   0x2000CD6E2C0     295 MB away
optimizing tier code buffer (mmap)      0x7A53DBCB8000    ~130 TB away
```

The two cells reach each other and neither reaches the code.
`GcBarrier::stw_requested` was an inline field of a struct only ever reached
through `Arc<SharedVm>`, so it came from mimalloc; the code cache comes from
`mmap(NULL, …)`, which the kernel places in its own region. Nothing pulls those
two towards each other. Every back edge and every method entry in the process
emitted the fallback:

```asm
mov  r11, <imm64>                ; 10 bytes
test byte [r11], 0FFh            ;  5 bytes, and R11 is gone
```

**Nothing failed.** The fallback reads the same byte and branches the same way;
it is only longer. Nothing in the tree asserted anything about where the flag
sits, so no test could have noticed.

## What `near_globals` already fixed, and what it left

`CRATONVM_JIT_CODE_NEAR_GLOBALS` hints `mmap` to place each code buffer within
1.5 GB of `layout_replace_epoch_guard()`. The safepoint flag comes along for
free, because it is 295 MB from that anchor in the same mimalloc band. That is
the better half of the problem and it is not re-litigated here.

It is **default OFF**. On a default Linux run the code buffer is still ~130 TB
from the flag and every poll still takes the long form.

## The fix, for that configuration

`platform::alloc_code_adjacent_cell` (`jit/src/platform.rs`) bump-allocates
64-byte cache-line-isolated cells out of 64 KiB chunks obtained by the same
`platform_alloc` — the bare `mmap(NULL, …)` / `VirtualAlloc(NULL, …)` — that
`alloc_executable` hands the code cache. Chunks are never unmapped and cells are
never recycled, so a cell's address can be baked into generated code and is
valid for the rest of the process.

`CacheLineFlag` (`vm/src/threading/gc_barrier.rs`) becomes a `&'static
AtomicBool` into such a cell. Its four methods (`flag`/`load`/`store`/`swap`)
are unchanged, so all 75 call sites of `stw_requested` are untouched, and
`stw_requested_flag_addr` — the one the JIT bakes — now returns an address that
outlives the VM rather than one that must not.

### Exactly one strategy may own placement

`82bf52efd` is right that the two do not compose, and its reasoning applies
symmetrically. `near_globals` anchors on the epoch counter and works on the flag
only because the flag is in the same allocator band; handing the flag a cell
takes it OUT of the band the anchor is pulling the code towards and leaves the
poll ~130 TB behind — the precise failure that commit describes for the epoch
counter, in the other cell.

So `alloc_code_adjacent_cell` returns `None` whenever
`CRATONVM_JIT_CODE_NEAR_GLOBALS` is engaged, and `CacheLineFlag` falls back to
the leaked `Box`. With that flag on, behaviour is **bit-identical to before this
change**: the flag stays on the VM heap beside the anchor, which is where it
belongs when something else owns placement. What composes is not the two
mechanisms; it is that exactly one of them is ever engaged.

This is a choice, not a necessity. On Unix `platform_alloc` already routes
through `near_globals::place`, so a cell chunk WOULD have been hinted into the
band and reach would have worked either way — but it would also have walked that
strategy's ladder at VM init, seeded its cursor from a data mapping, counted in
its engagement census, and been able to retire it permanently before the first
compile. Declining is the version that leaves `near_globals` untouched.

### The interpreter had to be paid for

`stw_requested` is the single hottest load in the VM — every interpreter thread
reads it once per bytecode. Turning the field into a pointer would have made
each of the two per-bytecode reads a dependent PAIR of loads, and the pointer
word shares a cache line with `gc_generation`, `threads_blocked` and the barrier
mutex — precisely the neighbours `CacheLineFlag` exists to stay away from.

`execute_frame` now resolves the ADDRESS once, next to the existing
`async_exception_slot` hoist. This is not the hoist the loop-top comment
refuses: that one caches the flag's VALUE at frame entry and would cut poll
frequency, which is time-to-safepoint. Every poll still loads the byte.

## Measured

Windows 11 x86-64, release build, JDK 25 boot classes, a hot counted loop
(`long s; for (int i = 0; i < n; i++) s += i ^ (s >>> 3);`) driven to the
OSR/optimizing tier and dumped with `CRATONVM_DBG_JIT_DISASM`. Baseline is the
same tree with only `gc_barrier.rs` reverted.

| build | flag | code buffer | apart | poll sites, short form |
|---|---|---|---:|---:|
| before | `0x19048D052C0` | `0x1902A440000` | 489 MiB | 394 / 394 |
| **after** | `0x1CA80000000` | `0x1CA80020000` | **128 KiB** | 394 / 394 |

**This host was already in reach, so it shows no encoding change** — which is
also why `near_globals` is not built for Windows. mimalloc there allocates
through `VirtualAlloc` and lands in the same low region as the code cache. What
the Windows numbers do show is the difference between 489 MiB of luck and
128 KiB of construction.

**The Linux confirmation was still owed**, and it is the one that matters: build
with the default (no `CRATONVM_JIT_CODE_NEAR_GLOBALS`), run the probe on
20.80.105.49, and read any loop body. `test byte [rel …]` means in reach,
`mov r11, <imm64>` means it is not.

### Paid, 2026-09-10, on 20.80.105.49

Ubuntu 24.04 x86-64, release build of `dev`, **default flags** —
no `CRATONVM_JIT_CODE_NEAR_GLOBALS`, which is the configuration this section
exists for. `probes/MultiFieldLoop.java`, `CRATONVM_JIT_FORCE_C2=1`,
`CRATONVM_DBG_JIT_DISASM=MultiFieldLoop.sumGuarded`:

```text
[cratonvm-jit-disasm] osr/sp  MultiFieldLoop.sumGuarded(I)I entry=0x7b3f61876000
  8c: f6056d6f0000ff    test byte [rel 7B3F6187F000h],0FFh
```

The flag byte is **36 KiB** from the code buffer that reads it — a cell out of
`alloc_code_adjacent_cell`'s own chunk, exactly as designed. Across the whole
dump, **10 poll sites take `test byte [rel …]` and 0 take the R11 form**, in
both bodies and in both arms of the placement flag:

| body | arm | `test byte [rel …]` | `mov r11` + `test byte [r11]` |
|---|---|---:|---:|
| `full/ir` | default | **2** | 0 |
| `osr/sp` | default | **2** | 0 |
| `full/ir` | `CODE_NEAR_GLOBALS=1` | **2** | 0 |
| `osr/sp` | `CODE_NEAR_GLOBALS=1` | **2** | 0 |

So the answer to the question this page asked is *in reach*, and the Windows
table above — which could not show an encoding change because that host was
already in reach by luck — now has the platform where it was not.

**It also changes what the sibling page's flag is worth**, which is why this
is not just a box ticked. `near_globals` used to move the polls AND the guards;
on a tree with this fix the polls are already short without it, so all it moves
is the guards. Measured on the same probe, the `full/ir` body is 1851 bytes at
the default and 1815 with the flag: **36 bytes, which is 4 guards x 9 and no
poll component at all.** The 50-byte figure in
`c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`'s engagement
table was 4 x 9 + 2 x 7, and the 2 x 7 is what this page took away from it.

## Guarded by

- `jit::platform::tests::a_cell_is_within_disp32_of_a_code_buffer` — a bare cell
  against a bare code buffer.
- `vm::threading::gc_barrier::tests::stw_requested_flag_is_within_disp32_of_the_code_cache`
  — the byte that is actually polled, reached the way the JIT reaches it. Also
  fails if `CacheLineFlag` is ever put back inside the `Arc`-allocated struct.
- `code_adjacent_cells_are_distinct_aligned_and_zero` — about the allocator, not
  the poll: "fresh anonymous pages are zero-filled" is a property of the first
  cell of a chunk, not the 900th.

All three are x86-64-only, because `disp32` reach is what makes distance mean
anything, and all three stand down when `near_globals` is engaged — there is no
cell then, and the distance of one would mean nothing.

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
`jit/src/x64/licm.rs` are now `pub(crate)` and the lowerer calls them.

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
- **Helper call targets.** Observed in the same dump: all 394 are
  `mov r64, <imm64>` + `call r64`, never `call rel32`, because the executable
  image (`0x7FF638……`) sits **125.7 TiB** from the code cache (`0x23DC0C70000`)
  on this Windows host. Neither strategy addresses that — `near_globals` anchors
  on a heap cell, not the image, and a call target cannot be relocated into a
  cell.
