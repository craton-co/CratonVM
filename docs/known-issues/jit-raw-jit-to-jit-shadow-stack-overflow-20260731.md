# Raw JIT-to-JIT direct calls overflow the shadow stack (gate stays closed)

**Status: OPEN.** `direct_jit_callee_calls_enabled()` still refuses raw
JIT-to-JIT calls whenever the moving young generation is on. Three real defects
found while investigating this are FIXED and shipped; the edge itself is not yet
safe to reopen.

> **This gate now carries the throughput residuals of tomcat known-issue 30**
> (2026-07-31), which closed once all three of its admission bans were settled
> and every remaining item root-caused here — see
> [30 § Adopted](../internal/fixed-suite-bugs/tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md#adopted-2026-07-31--two-residuals-from-the-retired-tomcat32-and-where-they-went).
> With this gate closed, **every `invokevirtual` from compiled code takes
> `jit_invoke_dispatch`**, the generic helper: measured at 992 ns monomorphic
> and 6 027 ns polymorphic against HotSpot's 5 and 9 ns, and 12 981 ns for a
> nested chain like `Calendar.get` (HotSpot 13 ns). `CRATONVM_DBG=mic-prof`
> logs 99 373 `[DISP_TRACE]` entries for a 50 000-iteration loop over two tiny
> accessors.
>
> **Reopening this gate is necessary but NOT sufficient**, which is worth
> knowing before sizing the work. Forcing it with
> `CRATONVM_JIT_DIRECT_CALLEE_CALLS=force` buys **1.8×** on that path and no
> more, because the direct route admits only `invokestatic` and non-`<init>`
> `invokespecial` (`jit/src/lib.rs`, the `ir_direct && (is_static || is_special)`
> guard). There is no `final` / CHA devirtualization, so `invokevirtual` keeps
> taking the helper either way — including calls to methods declared `final`,
> which cannot be overridden and are trivially bindable. **Two prerequisites,
> not one.**

Supersedes the hypothesis recorded in
`jit/src/lib.rs::direct_jit_callee_calls_enabled` and in
`vm/src/jit/conservative_roots.rs::chain_entry_rbp_is_foreign`, which framed the
hazard as *"a GC at that boundary can select an incompatible oop map and reclaim
a live root"*. Measured, that is **not** what happens. The root scan behaves
correctly: the per-cycle coverage proof marks `FOREIGN_INNERMOST_RBP` (4115
times in one run) and `UNBOUNDED_FRAME_BAND` (4115), the collection diverts to
the non-moving sweep, and `roots.rs` runs the conservative backstop because
`moving_young_precise_only` requires a complete proof. Nothing is reclaimed.

## What actually happens

A **shadow-stack push runs off the end of the thread's 2 MiB shadow buffer** and
keeps storing, rewriting the mimalloc arena behind it — including the
`JvmThread` struct — until it walks out of the mapping ~170 MiB later and
faults.

Evidence (gdb, `BasicErrorControllerIntegrationTests`, default flags +
`CRATONVM_JIT_DIRECT_CALLEE_CALLS=force`):

```
=> 0x7fffec25d339:  mov    %rbx,0x0(%r11)          <-- faults
   0x7fffec25d340:  lea    0x8(%r11),%r11
   0x7fffec25d347:  mov    %r11,0x270(%r10)        <-- commit top
   0x7fffec25d304:  mov    0x270(%r10),%r11        <-- (earlier) read top
r11 = 0x20040000000   = one past the end of  0x20000000000-0x20040000000 rw-p
```

and the memory the run had already overwritten is a **repeating four-oop
group** — the signature of one push executing over and over with nothing
popping it:

```
0x20039a80900: 0x2001245eb48 0x200123a2068 0x200124f8988 0x2001245f0c8
0x20039a80920: 0x2001245eb48 0x200123a2068 0x200124f8988 0x2001245f0c8   (repeats)
```

Two facts make this a corruption rather than a clean failure:

1. **No backend emits the `end` guard.** `gc/src/shadow_stack.rs` documents
   `END_OFFSET` as *"read by the JIT-emitted overflow guard"* and
   `ShadowStack::push` returns `false` on overflow — but the JIT never calls
   that, and neither `x64::emit_shadow_push` nor `ir_lower::emit_shadow_push`
   emits a bounds check. An overflow is therefore silent memory corruption, not
   a bail.
2. **A raw JIT-to-JIT call removes the only heal.** `set_jit_thread` /
   `restore_jit_thread` (`vm/src/jit/helpers.rs`) snapshot and reset `top` at
   every Rust↔JIT boundary, "healing any push an abnormal JIT exit left
   unbalanced". With the gate closed every call crosses that boundary. With it
   open, a whole subtree runs with no boundary at all, so any unbalanced push
   accumulates without limit.

## Fixed by this change

1. **The inline PIC cascade never republished the innermost-RBP mirror**
   (`x64.rs`, the per-slot `CALL R11`). Its MIC neighbour always has, and so has
   the hashed vtable stub — but `pic_inline` wins over `mic_inline` whenever a
   PIC slot exists, and slots are allocated eagerly at every eligible site, so
   the PIC arm is the one that actually runs. After every inline virtual-call
   hit the mirror named a **dead** callee frame, and the next GC applied the
   caller's oop map at that address. Independent of the gate.
2. **The IR tier had no shadow-`top` watermark at all.** `ir_lower`'s
   `emit_epilogue` and its shared exception/deopt bail stub were bare
   `add rsp / pop rbp / ret`, so every `i64::MIN` sentinel return skipped the
   call site's reload and leaked its push. The single-pass backend has always
   restored a per-method `savetop` in its epilogue; the IR tier now reserves the
   same slot, captures it in the prologue and restores it on every exit.
3. **An IR self-recursive direct call emitted a push with no reload.**
   `emit_safepoint_map` publishes the safepoint (emitting the push) before the
   lowerer discovers `invoke_kind == 4`; the old code then cleared
   `pending_shadow`, which correctly withdrew the *coverage claim* but left the
   already-emitted push with nothing to pop it. The reload is now emitted
   explicitly after the call. This route only exists when the direct-call gate
   is open.

`runtime_lowering::emit_post_call_frame_republish` also now prefers the inline
TLS store the prologue itself uses (9 bytes, no call) instead of a
PUSH/SUB/CALL/ADD/POP sequence. Side effect worth knowing: that shrank IR bodies
enough that the `JIT try_patch_i32: offset out of bounds` flood the open gate
used to produce (605 per run) went to **0** — i.e. those IR compiles were
previously being discarded and silently falling back to C1.

## What remains

With all three fixed, the class still SIGSEGVs 4/4 with the gate forced open,
same signature. Bisected:

| arm | result |
|---|---|
| gate closed (shipping default) | clean |
| gate open | SIGSEGV, ~1 s after start, 4/4 |
| gate open + `CRATONVM_JIT_IR_RELOC_EMIT=0` (IR pushes off) | still SIGSEGV |
| gate open + `CRATONVM_JIT_IR_SELFREC_DIRECT=0` | still SIGSEGV |
| gate open + `CRATONVM_JIT_IR_DIRECT_CALL=0` | still SIGSEGV |
| gate open + `CRATONVM_JIT_IR_CALL_VIRTUAL=0` | still SIGSEGV |

So at least one more unbalanced push lives in the **single-pass** backend. Its
push/reload pairing is compile-time (`pending_shadow`, consumed by
`emit_oop_map_for_safepoint`), and a static audit of all 20
`emit_pre_safepoint_spill` sites found a matching map on each, so the remaining
leak is a *runtime* path that reaches a push but not its reload.

## Next steps, in order

1. **Emit the `end` overflow guard** in both backends. This is a memory-safety
   fix in its own right and it is what turns this class of bug from silent heap
   corruption into a named, non-corrupting bail. It needs a reload-skip
   protocol: on overflow the push must be skipped *and* the matching reload must
   not restore homes from slots that were never written (the savebase-out-of-
   range recovery path already pops `top` unconditionally, which would then
   under-pop).
2. Find the remaining single-pass leak. The accounting probe used here (a leaked
   `i64` per compiled method whose address is baked into the push's `ADD` and
   the reload's `SUB`) works, but its counters live in the same arena the
   runaway push overwrites — put them in a `static` array instead.
3. Only then reopen the gate.

## Reproduction

`CRATONVM_JIT_DIRECT_CALLEE_CALLS=force` opens the gate under moving-young in an
otherwise unmodified binary (added for this investigation; the default and the
`=0` off-switch are unchanged).

```
org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests
module/spring-boot-webmvc, default flags
```

Crashes within ~1 s, deterministically. `CRATONVM_NO_MOVING_YOUNG=1` is not a
usable control arm — the class does not finish inside 900 s that way.

**Acceptance gate for any reopening attempt:** that class, 14 consecutive runs,
default flags.
