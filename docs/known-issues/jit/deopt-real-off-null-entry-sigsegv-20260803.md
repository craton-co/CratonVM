# `CRATONVM_JIT=deopt-real=0` calls a compiled entry of 0 — SIGSEGV at pc=0

**Status:** open, reproduced deterministically, minimal reproducer checked in as
`probes/IndyDeoptProbe.java`. **Found:** 2026-08-03, while measuring
`docs/known-issues/c2/loop-02-planner-admission-gates.md`'s four refusals.
**Not** caused by the loop rewriter — the rewriter is off in every run below.

## Why this matters beyond itself

`deopt_real` is default-ON, and turning it off is the **only** configuration in
which the bytecode loop rewriter (`loop-01`) can transform anything: it is the
first of `plan_bytecode_loop_xform`'s four whole-compile refusals, and the
measurement in `loop-02` shows it fires on **100%** of compiles. So this defect
is what stands between that lane and any real-code validation, and it blocks
`loop-02`'s own "then narrow the cheapest gate" step: narrowing a gate is
pointless while the configuration behind it crashes.

## Reproducer

```bash
cratonvm --java-home <jdk> -cp probes IndyDeoptProbe                  # PASS
CRATONVM_JIT='deopt-real=0' cratonvm --java-home <jdk> -cp probes IndyDeoptProbe
#  SIGSEGV at pc=0x0, addr=0x0
```

45 lines of Java, no framework. The crash is in `lambdaLoop` — the output stops
before `lambda=` prints — which is a hot method containing an `invokedynamic`
(`LambdaMetafactory`). `concatLoop` (a `StringConcatFactory` indy) has not been
shown to be innocent; the run never reaches it.

Originally found on Spring Boot, where it kills the suite ~3s in:

```bash
CRATONVM_JIT_SPEC='deopt-real=0' /data/sbrun.sh <exe> jit \
  core/spring-boot-autoconfigure \
  org.springframework.boot.autoconfigure.AutoConfigurationSorterTests /tmp/out 1 240
# rc=139 CRASH   (the same class is PASS with the default JIT spec)
```

## What is established

* **It needs the JIT.** `--nojit` with `deopt-real=0` passes the same Spring
  class.
* **It is not one of the obvious passes.** `-osr`, `-bce`,
  `-scalar-replacement`, `-unroll`, `-bg-compile`, `-c2-supersede` and
  `tier-c2-threshold=2000000000` all still crash.
* **It is not the loop rewriter.** Nothing arms it in any run above, and its
  planner refuses every compile regardless.
* **Small workloads do not reproduce it.** `LoopXformProbe`,
  `LoopVersionOsrProbe` and three `CratonBench` phases (hashmap, stringregex,
  sieve) all pass under `deopt-real=0`. The shape that does reproduce is a hot
  method carrying an `invokedynamic`.
* **The fault is a call to address 0, from the VM, not a bad branch inside JIT
  code.** Under gdb:

  ```
  #0  0x0000000000000000 in ?? ()
  #1  0x0000555556daa74e in try_call_with_context () at jit/src/lib.rs:2475
  #2  execute_jit_call () at vm/src/runtime/interpreter/invoke.rs:20206
  #3  execute_invokestatic_cached () at .../invoke.rs:13642
  rip 0x0   rdi 0x0   rsi 0x28
  ```

  `rsi = 0x28` is 40, `lambdaLoop`'s argument, so this is the compiled
  `lambdaLoop` being invoked. `rdi` is the `vm_ptr` argument and it is **also**
  zero.

  On the Spring workload the same failure lands on a *fixed* non-code address
  (`.debug_loc`, VA `0x11c0b4`) instead of 0, with `rbp = 0` and no stack — the
  same "called something that was never a function" shape with different
  garbage.

## The contradiction to resolve first

`try_call_with_context` calls `validate_code_ptr(self.entry)` before
transmuting, and that function rejects null, rejects misalignment, and rejects a
pointer outside the registered code regions. A null `entry` should therefore
have surfaced as `Err(CompileError::InvalidCodePtr)`, not as a jump.

So either

* `self.entry` was non-null and valid at validation time and the *first
  instruction* of that artifact transferred to 0 without building a frame — in
  which case the artifact's prologue is the suspect; or
* the artifact reaching this call is not the one that was validated.

`rdi = 0` argues for the second: `vm_ptr` is the caller's own argument, and it
has no business being zero. Settle this before reading anything else — the two
stories need different fixes.

## Where to look

The `invokedynamic` lowering (`x64/bytecode_walk.rs`, the `0xba` arm) emits an
unconditional trap and pushes a `deopt_stubs` entry with reason 8
(`UNREACHED_CODE`). Two things about that path are explicitly *not* gated on
`deopt_real_enabled()` while their neighbours are — the comment in
`x64/deopt_stubs.rs` around the `frame_box_ptr` computation
(`crate::deopt_real_enabled() || matches!(reason, 8 | 9 | 10)`) says so — which
makes reason 8 the one path that behaves differently from the rest of the
family when the flag is off.

`driver.rs` also has publication gates keyed on whether a compiled `0xba` site
is present (`cm.can_deopt_resume`, `cm.can_osr_exit`, and the note about not
publishing such an artifact's entry where machine code calls it directly). A
gate that is correct only when `deopt_real` is on would produce exactly this:
an artifact published to a caller that must not have it.

## What this is not

Not the loop rewriter, and not `loop-02`'s to fix — that lane owns
`jit/src/x64/loop_rewrite.rs` only. It is recorded here because it is the
measured blocker for that lane and because the reproducer is cheap.
