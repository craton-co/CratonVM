# bc math-ec JIT miscompile — investigation notes

Status: **PARTIAL**. Two genuine, sound codegen correctness fixes landed in
`jit/src/x64.rs`. The *specific* `org.bouncycastle.math.ec.test.AllTests`
SEGV is **not yet fixed**; the blanket `org/bouncycastle/` JIT ban in
`vm/src/jit/skip_list.rs` is therefore **kept in place** (not removed).

> ## ⚠️ RE-DIAGNOSIS 2026-06-04 (session "take it on") — the framing below is WRONG
> Two prior conclusions in this doc are **refuted** by fresh measurement (dev,
> uniquely-named binary `cratonvm_ecjit.exe` to dodge the cross-session
> `taskkill /F /IM cratonvm.exe` artifact; repro `-Xmx256m` forces GC and surfaces
> corruption in ~14 s):
>
> 1. **"needs many BC packages JIT-compiled together" is FALSE.** JIT-ing ONLY
>    `org/bouncycastle/math/ec/` (`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`
>    `CRATONVM_JIT_BISECT_ONLY=org/bouncycastle/math/ec/`) reproduces the heap
>    corruption / SEGV in 14 s. The prior round's "single-package bisection only
>    times out" was the taskkill artifact masking the corruption as a timeout.
>
> 2. **It is NOT (only) a JIT codegen miscompile.** `FixedPointTest` in isolation
>    FAILS **with JIT fully disabled** (`CRATONVM_DISABLE_JIT=1`): a flood of
>    `gen_heap::set_field: out-of-bounds field write dropped` (putfield index 0
>    into a bare `java/lang/Object`, `num_slots=0`, some with garbage `class_id`s
>    like 2121213 / 3114129) followed by a fatal
>    `expected object reference, got long(-844424930131964)` — i.e. a
>    **CompactValue long↔object NaN-box collision** reaching a *context-free
>    decoder* (`types/src/compact_value.rs`, `note_object_degradation` /
>    `object_degradation_count`). `-844424930131964 = 0xFFFD000000000004`: the top
>    bits are the quiet-NaN `SUB_OBJECT` tag, the payload is `4` (an invalid
>    pointer). Sometimes the bogus pointer (~4) passes a recovery path and is
>    dereferenced → `EXCEPTION_ACCESS_VIOLATION read at address 0x14` (= 4 + 0x10),
>    SEGV even with JIT off. The outcome (clean error / SEGV / heap-walker desync)
>    is nondeterministic, decided by whether the collided long happens to look like
>    a live heap address.
>
> **Real root cause:** a value-representation / operand-slot type-confusion in BC's
> EC arithmetic path. BC F2m `LongArray` (GF(2^m): `lxor`/`lshl`/`lushr` over
> `long[]`) produces 64-bit values whose bits land in the `SUB_OBJECT` NaN-box
> space (`0xFFFD…`); one such long ends up in a *reference-typed* slot and a
> context-free `to_value()` decode classifies it as an object (then degrades /
> derefs). The JIT ban only suppresses the JIT face of this; the interpreter face
> (`FixedPointTest` wrong result, `NISTECC: Exception`) is the same bug. The
> ongoing dated patches (`Round-8`, `BC SM2 2026-05-28 verbatim-long encode`) are
> in this same fight. cf. memory `reference_jca_synthetic_crypto_layers`
> ("interpreter long/float operand-stack bug").
>
> **Implication for the JIT ban:** lifting it is NOT the gating prerequisite the
> handoff assumed — the value-model collision must be fixed first (it breaks the
> interpreter too). And the RSA/AES native-accel lever is INDEPENDENT of this (those
> are pure interpreter *slowness*, not the EC collision) — it does not need the ban
> lifted or this bug fixed.
>
> Fast repro harnesses left in `/tmp`: `ecrun2.sh` (ALLOW/ONLY/SKIP/heap/timeout,
> classifies CORRUPT/SEGV/WATCHDOG/OK), `ectoggle.sh` (per-feature toggle).

## Reproduction (existing release binary, no rebuild needed)

```
cd apps/_test-suites/bc-java
CRATONVM_JIT_ALLOW_PACKAGES='org/bouncycastle/' \
  target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  -Xmx1g -cp "core/build/classes/java/main;core/build/classes/java/test;\
core/build/resources/main;core/build/resources/test;$TEMP/junit-3.8.2.jar" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests
```

Deterministic crash:

```
EXCEPTION_ACCESS_VIOLATION (0xC0000005) at pc=0x000000004D400029
Faulting access: read at address 0x0000000000000031
rax=1 rcx=<heap obj> rdx=1 rbx=0x000000004D400000 ... rip=rbx+0x29
```

`HEADER_SIZE=40 (0x28)`, `FIELD_CELL_PAYLOAD64_OFFSET=8` ⇒ field-0 payload is
at `obj+0x30`. The fault reads `[1 + 0x30] = 0x31`: **a small integer `1`
(or `3`, reading array-length at `[3+0xC]=0xF` with inline-getfield disabled)
is sitting in a slot consumed as an object/array reference.** It is later
dereferenced by an inline `getfield`/array op, and/or followed into a bad
pointer by the young-gen scavenge copy loop (frame-2 GC `movups` field-cell
copy + `and [rdx+0xD0],0x100000` flag mask in the backtrace).

## What was RULED OUT (all still crash identically)

- `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1` — still crashes ⇒ NOT scalar
  replacement.
- `CRATONVM_JIT_DISABLE_INLINE_NEW=1` — still crashes ⇒ NOT the inline TLAB
  `new` path.
- `DISABLE_INLINE_GETFIELD=1` — crash *moves* (now an array op, base `3`) but
  persists ⇒ the inline getfield is only a *dereference site*; the bad value
  is produced upstream.
- `dup2` (0x5C): a full disassembly scan of the entire `org/bouncycastle/math`
  tree found **no FORM-2 `dup2`** (every `dup2` is on `[arrayref,int]` — two
  category-1 values). So `dup2` is not the EC trigger (though it *is* a real
  latent miscompile — fixed below as defense-in-depth).
- Package bisection (`CRATONVM_JIT_BISECT_ONLY` / `CRATONVM_JIT_BISECT_SKIP`):
  no single BC sub-package (`math/`, `util/`, `asn1/`, `crypto/`, `internal/`,
  `test/`, …) reproduces in isolation — they all just time out. Only the FULL
  `org/bouncycastle/` allow-set crashes. ⇒ the miscompile needs **many
  packages JIT-compiled together** (a cross-package JIT→JIT direct-call / PIC
  interaction), consistent with the symptom (a primitive `1` landing in the
  callee's receiver/arg register `rdx`).

## Most likely remaining root cause (unconfirmed)

An **argument-marshalling / dispatch miscompile on a cross-package JIT→JIT
call**: the caller loads the wrong operand-stack slot (a primitive `1`) as the
receiver/first arg. The faulting callee is entered with `rdx=1` (Win64
ARG_REGS[1] = receiver for a needs-context call) and immediately does
`getfield field-0` on it. Suspects to audit next (needs a build to iterate):
- the direct-call / sibling-tail / PIC arg-slot marshalling around
  `jit/src/x64.rs:14609+` and `:15890+` (receiver/arg slot ordering, esp. when
  a category-2 arg occupies one JIT stack entry but the JVM descriptor counts
  two slots);
- operand-stack oop-mark propagation in `dup`/`dup2`/`swap`
  (`jit/src/x64.rs:11768+`): the `CalleeSaved`/`Xmm` arms push to `self.stack`
  WITHOUT a matching `self.stack_oop_marks` push, desyncing the oop-mark
  vector (the `emit_oop_map_for_safepoint` "lazy resync" pads at the END,
  mis-attributing marks). This is a real latent bug; whether it produces the
  EC value-corruption is unconfirmed.

To pinpoint: add a faulting-RIP code-bytes dump to the crash handler
(`vm/src/runtime/crash_handler.rs` — out of scope for the JIT agent) so the
exact miscompiled instruction at `0x4D400029` can be disassembled.

## Fixes that DID land (jit/src/x64.rs only)

1. **Escape-analysis / scalar-replacement control-flow soundness.**
   `analyze_escapes` and `plan_scalar_replacement` are single LINEAR passes
   that carried abstract operand-stack + per-local provenance straight through
   branches with no per-block reset/merge — unsound for any method with
   control flow (the documented "allocate-then-putfield" class). Added a hard
   provenance barrier at every control-transfer **source** (each branch /
   switch / goto / throw / ret arm) and every branch **target**
   (`compute_branch_targets`), confining scalar replacement to objects whose
   whole `new; dup; <init>()V; (putfield|getfield)*` lifecycle is within one
   straight-line region. (Did not fix the EC crash, but is a correct fix for a
   real miscompile class.)

2. **`dup2` (0x5C) category-2 guard** (`dup2_category_safe`, gating in
   `jit_scan`). The codegen `dup2` handler unconditionally implements the
   two-category-1 form; for a single category-2 (long/double) operand it
   duplicates an unrelated lower slot, desyncing the stack. New analyzer
   rejects (interpreter fallback) only the provable FORM-2 case; the common
   `arr[i] op= x` FORM-1 `dup2` still JITs. (Sound; not the EC trigger since EC
   has no FORM-2 dup2.)

## Ban status

`vm/src/jit/skip_list.rs:457-461` blanket `org/bouncycastle/` ban is
**unchanged**. Do not remove until the cross-package dispatch miscompile above
is root-caused and fixed, or the EC AllTests run completes cleanly under
`CRATONVM_JIT_ALLOW_PACKAGES='org/bouncycastle/'`.
