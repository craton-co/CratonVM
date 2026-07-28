# OPEN: rare wild jump to a page-aligned address under load (2026-07-28)

Status: **OPEN**, pre-existing on dev, ~0.3% of stressed runs. Distinct from
the retirement use-after-free fixed in
`docs/internal/jit-cache-retirement-unmaps-executing-code-fixed-20260728.md`,
which shared the same workload but a different fault signature.

## Signature

```
#  SIGSEGV at pc=0x70d01e48a000, addr=0x70d01e48a000
#  r10=0x1 r11=0x20000ffcc98
#  slot[r10]:                      <- r10 is not a readable slot pointer
```

with, crucially, **no** `jit pc :` line and **no** `fault pc is inside a
RECENTLY FREED code buffer` line. So, unlike the fixed bug:

* the faulting address is **not** in the JIT range registry — it does not
  belong to any live compiled body; and
* it is **not** in the ring of recently unmapped buffers (only ~5–12 buffers
  are ever unmapped per run, far below the 4096-entry ring, so this is a real
  negative, not a wrap-around).

`pc == addr` means the fault is the instruction fetch, and `pc` is **page
aligned** (`…000`) — the shape of a jump to the *start* of a buffer, i.e. to an
entry pointer, that is not mapped executable. `r10 = 0x1` is suspicious: the
inline MIC/PIC cascade and the hashed megamorphic stub both keep the slot base
in R10 across the guard sequence, so either this is not that path or R10 was
clobbered.

Working hypothesis (UNVERIFIED): a call target published into some cache was
never a valid entry — a torn/partially-initialised slot read, rather than a
lifetime problem. It is explicitly NOT the shard-retirement use-after-free: that
one always lands inside a registered, recently-freed range, and both of those
checks come back negative here.

## Reproduction

`docs/internal/repros/resolvabletype-array-receiver-mic-20260728/stress_rtq.sh`:

```bash
HOGS=20 ./stress_rtq.sh /path/to/cratonvm 8 16 CRATONVM_DBG_JIT_NAMES=1
```

16 concurrent VMs running `RtEqualsProbe` under 20 CPU hogs. Rate on the fixed
binary: **1 / 320**. It also occurred on pristine dev `77389fa06` (roughly 1 of
the 14 crashes there had this shape, the other 13 being the now-fixed
retirement bug), so it is not a regression from either fix on this branch.

## Next steps

1. Capture a core (`sudo sh -c 'echo /path/core.%e.%p > /proc/sys/kernel/core_pattern'`
   plus `ulimit -c unlimited`; the default apport handler writes nothing usable)
   and check `info files` for whether the address is in a *gap* or in a mapped
   non-executable region — that alone splits "never mapped" from "mapped but
   not RX".
2. Read `[rsp]`: if a return address was pushed the transfer was a `CALL`, which
   points at an inline-cache/dispatch entry; if not, it was a `JMP` or a
   fall-through.
3. `CRATONVM_DBG_JIT_PIN=1` / `CRATONVM_DBG_JIT_STALE_IC=1` count raw entry
   publications that carry no keep-alive (`unowned_ic_entry_publishes`); a
   non-zero count under this stress would name the publishing site.
