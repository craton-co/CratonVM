# Wild jump to a page-aligned address under load (2026-07-28)

Status: ✅ **RESOLVED**. The crash is a **stale baked direct-call target**: a
compiled body was published carrying a `call` to a callee entry that nothing
kept alive, the callee was later retired and unmapped, and the next invocation
of the caller jumped into the freed page. Fixed on `dev` by `33827ce9b`
("fix(jit,osr): stop the OSR bail re-running committed loop iterations"), which
made `CRATONVM_JIT_STRICT_CALLEE_ROOTS` **default-ON** so
`JitCache::prepare_for_publication` *refuses* to publish such a body instead of
merely counting it.

This write-up retires `jit-wild-jump-page-aligned-pc-20260728`, which recorded
the fault as OPEN with an unverified "torn cache slot" hypothesis. That
hypothesis was wrong, and so was the evidence it rested on — see
"The report was truncated" below, the reason this took a second pass at all.

## Signature

```
#  SIGSEGV at pc=0x732201240000, addr=0x732201240000
#  r10=0x1 r11=0x20000ffcc98 rsp=0x73220a9b58d8 rbp=0x6262c7e1c608
#  jit pc  : org/springframework/core/ResolvableType.forType(Ljava/lang/reflect/Type;Lorg/springframework/core/SerializableTypeWrapper$TypeProvider;Lorg/springframework/core/ResolvableType$VariableResolver;)…
#  fault pc is inside a RECENTLY FREED code buffer: base=0x732201240000 len=0x2ea0 active_jit_executions_at_free=0x0
#  fault pc is in NO live registered code buffer
#  maps: fault pc is NOT MAPPED - it is the hole between these two
#    prev: 73220120a000-732201240000 r-xp 00000000 00:00 0
#    here: 732201241000-732201243000 r-xp 00000000 00:00 0
#  innermost JIT frame on the stack: [rsp+0xb0]=0x732201216000 org/springframework/core/ResolvableType.forType(Ljava/lang/reflect/Type;Lorg/springframework/core/ResolvableType$VariableResolver;)…
```

Every field is load-bearing:

* `pc == addr` is an instruction-fetch fault, and `pc` is **page aligned** —
  `CompiledMethod.entry` is `ExecutableBuffer::as_ptr()`, i.e. an `mmap` base, so
  a page-aligned faulting PC is always some artifact's **entry**, never mid-body.
* the PC is in the recently-freed ring at `base == pc`: that exact buffer was
  released by this process.
* `active_jit_executions_at_free = 0` — nobody was inside compiled code when it
  was released. This is a **legal, quiescent retirement**, so it is *not* the
  `defer_jit_owner` bug fixed by `3fe14734a` / `463bd32e2`; the reference that
  survived the free was a raw address baked into another body, which holds no
  `Arc` and does not participate in quiescence.
* `/proc/self/maps` shows a one-page hole flanked by two live `r-xp` buffers:
  part of the freed region has already been recycled by later compilations.
* the innermost JIT frame is the **2-argument** `ResolvableType.forType(Type,
  VariableResolver)`; the dead entry is the **3-argument**
  `forType(Type, TypeProvider, VariableResolver)`. In Spring the 2-arg overload
  is a one-line delegation to the 3-arg one, so this is that call — emitted as a
  direct call to a baked address. Nothing was pushed on the stack for it
  (`[rsp]` holds a return address from the frame further out, in native code),
  which is why the caller shows up 0xb0 bytes up rather than at `[rsp]`.

`r10 = 0x1` and `r11 = 0x20000ffcc98` are not the MIC/PIC cascade's slot base
and target: they are unrelated leftovers, which is what the retired doc's
"either this is not that path or R10 was clobbered" was groping at. It is not
that path.

## Root cause

`JitCache::prepare_for_publication` upgrades each entry in
`CompiledMethod::_direct_callee_entries` into a strong `Arc` in
`_direct_callee_roots`, so a baked `call` keeps its callee's mapping alive. When
an entry no longer resolves to a live owner — the callee was superseded by a
tier-up between the compile driver baking its address and this publication —
the artifact is unsafe to publish.

Before `33827ce9b` that enforcement sat behind `CRATONVM_JIT_STRICT_CALLEE_ROOTS`,
**default-OFF**: the failure bumped `UNROOTED_DIRECT_CALLEES` and published the
body anyway, with a `call` to an address nothing owned. The callee's last `Arc`
then dropped on an ordinary tier-up, `ExecutableBuffer::drop` unmapped it while
quiescent, and the caller's next invocation jumped into the hole.

## Verification

`repros/resolvabletype-array-receiver-mic-20260728/stress_rtq.sh`: 16 concurrent
VMs running `RtEqualsProbe 3000` under CPU hogs, `CRATONVM_JIT_THRESHOLD=1`.

| binary / arm | crashes |
|---|---|
| `463bd32e2` (the binary the original report was filed against) | **14 / 2880** (0.49%) |
| `463bd32e2` + `CRATONVM_JIT_OSR=0` | 5 / 960 — OSR is **not** involved |
| `463bd32e2` + `CRATONVM_JIT_STRICT_CALLEE_ROOTS=1` | **0 / 1920** |
| `dev` `ccc57a8cb` (strict default-ON) | **0 / 3520** |

The flag arm is the single-variable proof: same binary, same load, one boolean.
`P(0 crashes in 1920 runs | 0.49%)` ≈ 1e-4, and ≈ 1e-7 for the dev arm.

Load is the knob, as it was for the retirement bug: at load average ~40 the rate
is ~0.5%; an idle machine produces almost none. Unlike that bug, this one is
**not** suppressed by `CRATONVM_DBG_JIT_NAMES=1` (5/960 with, 4/960 without), so
crash reports for it can be taken with naming on.

## The report was truncated — and that is why this was mis-diagnosed

The original write-up argued from two negatives: no `jit pc :` line, and no
`fault pc is inside a RECENTLY FREED code buffer` line. **Both lines were in
fact never reached.** The handler's readability probe wrote to `/dev/null`,
whose driver returns the byte count without ever touching the user buffer, so it
reported *every* address as readable — including `r10 = 0x1`. The handler then
dereferenced address 1, took a nested SIGSEGV, hit the re-entry guard and
re-raised with `SIG_DFL`. Everything after `#  slot[r10]:` was lost, which
happened to be every provenance verdict in the report.

Four defects fixed here, in `vm/src/runtime/crash_handler.rs`:

1. the probe now writes to a **pipe**, which copies from the buffer for real and
   returns `EFAULT`. `probe_readable_rejects_a_non_pointer_through_a_pipe` is the
   regression; `dev_null_reports_even_a_bad_address_as_readable` pins down why
   the pipe is required, so nobody "simplifies" it back.
2. the raw memory dumps now run **last**. A fault whose registers hold
   non-pointers is exactly when the verdicts matter most and exactly when a dump
   can fault again.
3. a lookup that could not take its registry lock is reported as
   `<… locked - this is NOT a negative>` instead of as silence
   (`JitNameLookup` / `JitRegionLookup` in `jit/src/lib.rs`).
4. three verdicts added: the live-code-region check (which covers OSR
   trampolines and bodies still being emitted — buffers no `put` ever names),
   the `/proc/self/maps` neighbourhood of the faulting PC (hole vs mapped
   non-executable — previously obtainable only from a core dump), and the
   **innermost JIT frame on the stack**, which names the caller even when the
   transfer pushed no return address. That last line is what identified the
   2-arg `forType` overload here.

## Lessons

* **A missing diagnostic line is not a negative result.** Check that the line
  after it printed before reasoning from its absence.
* `write(2)` to `/dev/null` is not a memory-readability probe. Only a fd whose
  write path actually copies (a pipe, a socket, a file) returns `EFAULT`.
* A page-aligned faulting PC in this VM means "an artifact's entry", and the
  matching question is *who transferred here*, not *what was in the cache slot*.
  `[rsp]` answers it only for a `call`; scan the stack when it does not.
* `active_jit_executions_at_free = 0` **excludes** the quiescence family of
  lifetime bugs. It points at a raw address held outside the `Arc` graph —
  a baked direct call, or an inline-cache entry published without an owner.
