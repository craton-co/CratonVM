# PERF-02 — every IR method ran 46 NOPs on entry (fib 1.96x) — FIXED 2026-08-05

**Found and fixed 2026-08-05. Introduced 2026-07-31 (`545c99add`).**
Fix: `f83b0d4e0` + `8c2929f17`, branch
`fix/ir-lazy-thread-fetch-jmp-over-20260805`.

Not a tiering decision, not a register allocator, not the shadow stack being
on. A 46-byte range of dead code that both backends erase, which one of them
erased without jumping over it.

## The symptom

`CratonBench fib` (recursive Fibonacci(44), ~2.27e9 calls) roughly doubled.
Same host, same JDK 25.0.3, same classes, interleaved against HotSpot:

| date | CratonVM `fib` |
|---|---:|
| 2026-07-18 | 4,268 / 4,242 ms |
| 2026-07-20 | 4,240 / 4,235 ms |
| 2026-07-23 | 4,240–4,246 ms |
| 2026-08-03 | 8,430 / 8,339 ms |
| 2026-08-05 | 8,507 / 8,348 ms |

**1.96x**, host-confound-free — the arms were interleaved in one window, not
compared across days.

## What it was

Both backends emit the shadow-stack `get_current_thread` fetch in the prologue
unconditionally, then discover after the body is lowered that the method
published no register-resident oop and the fetch is dead. Neither can *remove*
it: every recorded downstream offset — branch patches, deopt points, oop-map
native PCs — is already keyed on the current layout. So both erase it in place.

The single-pass backend erases it with a `JMP rel8` over the span
(`maybe_nop_out_shadow_fetch`, `jit/src/x64/frames.rs`, since `aa2ae19ae`,
2026-06-16). The IR backend, which got the same lazy prologue on 2026-07-31 in
`545c99add`, only ever did the NOP-fill half:

```rust
fn finish_lazy_thread_fetch(&mut self) {
    if self.shadow_pushed_any { return; }
    if let Some((start, end)) = self.thread_fetch_span.take() {
        for off in start..end {
            let _ = self.buf.try_patch_byte(off, 0x90);   // and nothing else
        }
    }
}
```

The span is ~46 bytes. So **every IR method that publishes nothing retired 46
one-byte NOPs on entry, on every invocation.** `fib` is two lines of straight
arithmetic entered 2.27e9 times; it paid it 2.27e9 times.

## How it was found

`CRATONVM_DBG_JIT_DISASM=CratonBench.fib` — a good-vs-bad codegen diff needing
no builds at all, which is what broke the investigation open after a bisect on
this host had been killed twice by the OOM reaper.

```
 2026-07-23 binary (single-pass body, 404 bytes)     dev (IR body, 746 bytes)
   4: sub rsp,0F0h                                     4: sub rsp,0C0h
   b: mov [rbp-10h],rdi                                b: mov [rbp-10h],rdi
   f: mov [rbp-8],rsi                                  f: mov [rbp-8],rsi
  13: mov rax,[rbp-8]                                 13: mov qword [rbp-20h],0
                                                      1e: mov qword [rbp-30h],0
                                                      29: mov [fs:0FFFFE080h],rbp
                                                      32: 90 nop     <-- 46 of these,
                                                      ...                straight-line,
                                                      5f: 90 nop         no jump over
                                                      60: mov r11,<safepoint flag>
```

**No flag moves it.** `CRATONVM_JIT_MY_SHADOW_EMISSION=0`,
`CRATONVM_NO_MOVING_YOUNG=1`, `CRATONVM_JIT_MY_SELFCALL_PROOF=0`,
`CRATONVM_TIER_C2_THRESHOLD=2000000000`,
`CRATONVM_TIER_C2_MIN_INVOCATIONS=2000000000` all produce a byte-identical
body — 176 instructions, 46 NOPs, every time. This is unconditional codegen,
which is why no lever-based bisect would ever have reached it.

## The fix

One shared `ExecutableBuffer::erase_range_with_jump_over(start, end)`, called by
both backends, so the two halves cannot drift apart again:

```
base  len=746   nops=46   jmp_rel8=0
fix   len=746   nops=44   jmp_rel8=1
```

```
29: mov [fs:...E048h],rbp
32: eb2c        jmp short 0x...060     <- 0x32 + 2 + 0x2C = 0x60
34..5f: 90      (unreachable; kept 0x90 so a disasm dump stays readable)
60: mov r11,<safepoint flag>
```

`len` is unchanged at 746 — the erase moves nothing, which is the whole reason
it is an erase.

`8c2929f17` adds the invariant the jump depends on as a `debug_assert!`: nothing
may branch INTO the span, because a site patched at `start + 1` would overwrite
the rel8 displacement and a site targeting `start + 1` would decode it as an
opcode. It holds structurally (the span is emitted at the top of the prologue,
before any block is lowered, so every recorded patch site is past `end`) and
`finish_lazy_thread_fetch` runs before `patch_branches` / `patch_self_calls`, so
both lists are intact there and it is checkable rather than merely true.

## Verification

Both arms built from the same base in the same target dir — the fix and its own
parent commit. `fib`, pinned cpu 13, `-Xmx8g`, isolated processes, interleaved
with the order flipped on alternate pairs.

Measured in **user CPU time**. The first pass used wallclock and was
directionally clean (6 of 6 pairs) but its spread was wide: host load fell
monotonically 25 -> 15 across the run, so later samples were simply cheaper.

| arm | user CPU (s), 8 pairs | median |
|---|---|---:|
| parent | 13.05 / 14.80 / 12.58 / 11.13 / 14.87 / 13.02 / 10.62 / 10.35 | **12.80** |
| fix | 10.28 / 7.72 / 7.85 / 7.05 / 7.65 / 6.73 / 6.66 / 6.57 | **7.35** |

**1.74x**, 8 of 8 pairs favouring the fix, ranges disjoint (fix max 10.28 <
parent min 10.35). Per-pair ratios 1.27 / 1.92 / 1.60 / 1.58 / 1.94 / 1.93 /
1.59 / 1.58.

### Correctness

All seven CratonBench phase checksums identical, fix vs parent — `arithmetic`
`5000000003999999995`, `fib` `701408733`, `sieve` `9592`, `matrix` `173943680`,
`hashmap` `1549999915000000`, `stringregex` `5000050000`, `bintrees` `68332206`.
This matters more than the one benchmark: the fix puts a taken branch on the
entry path of *every* IR method that publishes nothing, not just `fib`.

### Residual against the pre-regression body

The 1.74x is against the parent commit. It is **not** a claim that `fib` is back
where it was — the parent still had the IR tier on the method. Measured against
the 2026-07-23 binary (single-pass body, predates all of this) in one window,
6 pairs, order flipped:

| arm | user CPU (s) | median |
|---|---|---:|
| 07-23, single-pass body | 7.60 / 7.00 / 7.70 / 7.80 / 8.14 / 7.76 | **7.73** |
| fixed IR body | 10.33 / 8.61 / 10.26 / 10.21 / 10.68 / 10.11 | **10.23** |

**1.32x residual**, 6 of 6 pairs. That figure is the "Residual and follow-up"
section below, and instruction counting says so independently: the post-fix IR
body carries ~13 extra instructions per call over the single-pass body's ~47,
which is 28%. The residual is instrumentation, not code quality — which is what
makes the follow-up worth doing.

A note on arithmetic, so nobody tries to reconcile these numbers too precisely:
1.74 x 1.32 = 2.30x, while the regression observed across July/August was 1.96x.
The three figures were taken in different load regimes on a badly shared host
(the A/B ran at load 13-20, the residual run at 30-50). Only the two
same-window ratios above are evidence; their product is not.

## What this is not

**Not a `cov-*` problem, and not PERF-01 again.** PERF-01 was the optimizing
tier producing a genuinely worse *body* for `sieve` — the right answer there was
to veto the method. Here the IR body is fine; it was carrying dead weight that
the single-pass body had known to skip since June. Widening the IR tier onto
`fib` only made an existing, unconditional codegen defect visible on a workload
that could feel it.

**Not specific to `fib`.** Every IR method that publishes nothing paid this.
`fib` is simply the shape that makes it measurable: tiny body, enormous entry
count. Anything call-heavy and shallow was paying it in proportion.

## Residual and follow-up

The fix does not restore `fib` to its 2026-07-23 single-pass number, and is not
expected to. The IR body still carries per-call instrumentation the single-pass
body did not:

- the two prologue slot zeroings, which exist only so the null guards skip;
- `mov [fs:...],rbp` (frame record) on entry **and after every call return**;
- a bytecode-index store to `[rbp-18h]` before each call;
- the epilogue savetop-restore `mov r10,[rbp-20h]; test r10,r10; je ...`,
  emitted on all four exit paths.

**Every one of these is provably dead whenever `!shadow_pushed_any`** — the same
condition that erases the fetch. The slot zeroings exist only to make the
epilogue guard skip; erase the guard and the zeroings go too. That is the
obvious next increment: record those spans and erase them the same way. It is
strictly more bookkeeping than this fix (four exit spans instead of one prologue
span) and was deliberately not bundled in, so that the measurement above
attributes to one change.
