# `CRATONVM_JIT=deopt-real=0` — the indy trap's stub spilled over the caller's return address

**Status:** FIXED 2026-08-03. Regression test:
`x64::deopt_stubs::spill_region_contract`. Executable reproducer:
`probes/IndyDeoptProbe.java`. **Residual, unrelated to the crash:** one Spring
Boot test still fails under this flag — see [What is left](#what-is-left).

## The bug

`Compiler::new` reserves the 256-byte `SavedRegisters` region — the frame slot
the frame-deopt stub spills 16 GPRs and 16 XMMs into — on

```rust
deopt_real_enabled() || precise_exception_frames
```

`emit_deopt_stubs` takes the *spilling* path on

```rust
deopt_real_enabled() || matches!(reason, 8 | 9 | 10)
```

Reasons 9 and 10 are the precise-exception-frame stubs, which the first
condition covers through `precise_exception_frames`. **Reason 8 — the
unconditional `invokedynamic` trap — is covered by neither.** It is emitted
whether or not `deopt_real` is on, deliberately: its imprecise fallback has
proven silent-corruption risk, so the `0xba` lowering always records a precise
snapshot (see the comment on `emit_osr_exit_map_at_reason`).

With `deopt_real` off and an `invokedynamic` in the method, `deopt_regs_base` is
therefore `0`, and the stub's

```rust
for r in 0u8..16 { self.emit_store_local(base - (r as i32) * 8, r); }
```

emits `[rbp - 0]`, `[rbp + 8]`, `[rbp + 0x10]`, … — walking **up** out of the
frame, over the saved `rbp` and the **return address**, then 30 slots further
into the caller. The epilogue's `ret` jumps to whatever register landed on the
return slot (`rcx`).

That is the whole crash. It is not a null entry pointer, not a freed code
buffer, and not an inline cache: `validate_code_ptr` was never the thing that
failed.

## How it presented

```
#  SIGSEGV at pc=0x14ee8, addr=0x14ee8
#  fault pc is in NO live registered code buffer
#  maps: fault pc is NOT MAPPED
```

The fault pc is a **constant** across runs and across five JIT-feature
configurations, which is what a clobbered return slot looks like: `rcx` holds
the same value every time. It changed to `0x0` on a binary where a different
register happened to land there, which is why the first reading of this bug —
"a compiled entry of 0" — was wrong. gdb's `bt` reinforced the wrong reading:
with the return address overwritten, the only frame it can name is the Rust
caller, so the trace looks like `try_call_with_context` called address 0.

## What actually located it

Diffing the same method's machine code compiled both ways
(`CRATONVM_DBG=jit-disasm`, normalising absolute addresses out):

```
-mov eax,[rsp-2B0h]        ← deopt_real ON:  frame 0x2B0
+mov eax,[rsp-1B0h]        ← deopt_real OFF: frame 0x1B0
...
-mov [rbp-280h],rax        ← spills land inside the frame
-mov [rbp-278h],rcx
+mov [rbp],rax             ← …and here they do not
+mov [rbp+8],rcx
```

`[rbp+8]` is the return address. Two lines of diff.

Before that, the useful narrowing steps were: `--nojit` passes;
`CRATONVM_JIT_DENY=IndyDeoptProbe` passes while denying `java/lang` does not
(so the wild transfer is in the probe's own compiled body); and five JIT-feature
toggles change nothing.

## The fix

One predicate, `x64::deopt_spill_region_reserved(deopt_real,
precise_exception_frames, has_indy_sites)`, used by the frame computation, with
`has_indy_sites` threaded from `!indy_info.is_empty()` at the `Compiler::new`
call site. Plus a fail-closed backstop in `emit_deopt_stubs`: if the spilling
path is ever selected with `deopt_regs_base == 0`, bail the compile instead of
emitting the stores. One interpreted method is a cheap price for not corrupting
a stack.

Byte-identical on the default path: with `deopt_real` on the region was already
reserved, and a method with no `invokedynamic` reserves nothing, exactly as
before.

The regression test is a pure predicate rather than a compile, because
`deopt_real_enabled()` latches a process-wide `OnceLock` and no in-process test
can turn it off — so a test that went through `Compiler::new` would have passed
vacuously in the only configuration CI runs.

## What is left

The crash is gone; `deopt-real=0` is a usable configuration again. Spring Boot
`core/spring-boot-autoconfigure`, one run each, after the fix:

| class | default | `deopt-real=0` | `deopt-real=0,bytecode-loop-xform` |
|---|---|---|---|
| `AutoConfigurationSorterTests` | PASS 18/18 | PASS 18/18 | PASS 18/18 |
| `ConditionalOnClassTests` | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| `ConditionalOnPropertyTests` | PASS 38/38 | **FAIL 1/38** | **FAIL 1/38** |

`ConditionalOnPropertyTests.disableIfNotConfiguredOtherwiseWithConfigDifferentCase`
fails deterministically (2 of 2 runs) under `deopt-real=0`, with and without the
loop rewriter. That is a **wrong answer, not a crash**, it is a different defect
from this one, and it was invisible until now because the process died first.
Whoever picks it up should start from the same place this investigation ended
up: compare the method's machine code with the flag on and off.
