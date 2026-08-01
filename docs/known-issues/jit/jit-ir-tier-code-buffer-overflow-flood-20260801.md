# A flood of anonymous `try_patch_i32` code-buffer overflows

**Status: OPEN.** Re-filed 2026-08-01 from
`basicerrorcontroller-jit-only-failure-20260731.md`, which was retired that day
once the `<local5>` correctness bug it also recorded was fixed. This item was
NOT fixed. That document already asked for it to be treated as its own, and
this is that file.

## Symptom

Booting a Spring Boot application context under JIT emits thousands of:

```
WARN cratonvm_jit: JIT try_patch_i32: offset out of bounds; marking buffer overflowed offset=4096 len=4096
```

Measured on `BasicErrorControllerDirectMockMvcTests` (a 4-test class, ~30 s):
**3552 warnings in one run**, 4607 in another. Reported offsets cluster at
4092–4096 and then spread (4287, 4796, 5436, 7358, 8224).

## Why it matters, and why it is not a crash

`ExecutableBuffer::emit` is non-panicking: past capacity it sets a sticky
`overflowed` flag and DROPS the write, and both compile drivers check that flag
and discard the method. So this is a **silent de-optimization**, not
corruption — but the affected methods stop being compiled, which will later
read as an unexplained throughput loss with nothing pointing at a cause.

## It is the OPTIMIZING tier, not the single-pass backend

The retired document blamed `x64.rs`'s
`ExecutableBuffer::new(estimated_size.max(4096))` and the `rel8` → `rel32` PIC
widening in `7f1b1f263`. That is wrong. `CRATONVM_DBG_IR_BAILOUT=1` on the same
class settles it in one run:

```
try_patch_i32 warnings                    4607
JIT compile bailed (x64::compile's own)      5
[ir-bailout] code_buffer_exhausted          54
```

54 IR bailouts × the ~85 branch patches each one attempts after its buffer has
already stopped growing ≈ the 4607 warnings. The single-pass backend accounts
for 5. Its estimate is `code_len*96 + 8192 + invokes*512 + inline_extra` — never
below 8192 — and the five it reported had capacities of 22880–139744, which
cannot produce a `len` that stops at 4094.

The capacities the IR tier exhausts are dominated by the `.max(4096)` floor:

```
     17  needed 4096 bytes, capacity 4096
      6  needed 4095 bytes, capacity 4096
      6  needed 4094 bytes, capacity 4096
      2  needed 4092 bytes, capacity 4096
      2  needed 4415 bytes, capacity 4416
      1  needed 8224 bytes, capacity 8224     (and a tail of one-offs)
```

`ir_lower.rs`'s estimate is `nodes*32 + call_nodes*448 + 1024`, which falls
under the 4096 floor for any modest graph. Note `needed` is `buf.pos()`, which
CAPS at capacity — so every line above understates the real requirement and
none of them tells you how much more the method wanted. Fixing that reporting
is step zero.

## Why it was not simply fixed here

Raising the IR estimate is a two-line change, and that is the trap. It does not
merely stop the warnings — it changes **which methods the optimizing tier
produces bodies for**, and that tier is not uniformly faster: see
`reference_c2_tier_slower_because_fields_take_the_helper` (a 1.85x
regression, because compact `getfield`/`putfield` in that tier take
`jit_getfield` instead of an inline load). Widening admission without measuring
would be a blind perf change dressed as a warning cleanup.

Whoever takes this should: (1) confirm the tier attribution above; (2) raise the
estimate; (3) A/B the benchmark set on one binary, both arms, before and after.
