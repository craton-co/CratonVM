# SIGSEGV executing a JIT code buffer that had been unmapped

**Status: OPEN — one occurrence, cause not located.** Seen once in 34 runs of
`BasicErrorControllerIntegrationTests` on 2026-08-01. Filed because it is the
2026-07-28 "JIT cache retirement `munmap`ped code a thread was executing"
signature returning, and because the *attribution* in that occurrence's crash
report cannot be trusted — see "Why the report may be lying" below, which is
the part that matters for whoever picks this up.

## Symptom

```
#  SIGSEGV at pc=0x723bd854e15e, addr=0x723bd854e15e, pid=3743400
#  code_frees_total=0x84
#  fault pc is inside a RECENTLY FREED code buffer: base=0x723bd854e000 len=0x2580 active_jit_executions_at_free=0x2
#  fault pc is in NO live registered code buffer
#  maps: fault pc is NOT MAPPED - it is the hole between these two
#    prev: 723bd854d000-723bd854e000 r-xp 00000000 00:00 0
#    here: 723bd8551000-723bd85a0000 r-xp 00000000 00:00 0
```

`pc == addr` is an instruction-fetch fault: the thread was executing at that
address and the page went away. The faulting PC sits in a hole between two
mapped ranges. This is byte-for-byte the shape of
[`../../internal/jit-cache-retirement-unmaps-executing-code-fixed-20260728.md`](../../internal/jit-cache-retirement-unmaps-executing-code-fixed-20260728.md),
which was resolved on 2026-07-28 by `3fe14734a` ("retire a superseded artifact
only when JIT execution is quiescent").

It happened during the second Spring Boot context boot of the run, immediately
after `No active profile set` — i.e. early, while the JIT is churning through
first-time compiles.

## Why the report may be lying

`active_jit_executions_at_free=2` is normally the damning number: the recent-frees
ring pairs every unmap with `ACTIVE_JIT_EXECUTIONS`, and a non-zero value is
supposed to mean a buffer was released while a thread was inside compiled code —
the signature of a release path bypassing `defer_jit_owner`.

**In this binary that inference did not hold, and the reason generalises.**
The run carried an experimental optimizing-tier code-buffer *retry* that dropped
a discarded compile attempt directly, on a compiler thread, while mutators were
running compiled code. Nothing points into such a buffer — no cache entry, no
baked direct call, no trampoline, so no thread can be inside it — but it was
still recorded in the ring with a non-zero active count, and its address is then
free for the next `mmap` to reuse. So the "RECENTLY FREED code buffer" the
report names may be a buffer that merely *occupied that address later*, while
the thread was stranded by an earlier free of a genuinely published body.

That retry was removed before merge (dev's census-fitted estimate made it fire
zero times), so the ring is back to recording only buffers something could have
pointed into. **The lesson outlives the retry**: any future code that frees an
`ExecutableBuffer` outside the `defer_jit_owner` retirement queue silently
breaks this ring's invariant and, with it, the sharpest tool the codebase has
for use-after-free in JIT code. Re-read the invariant before drawing
conclusions from this crash.

## Rate, and what it does and does not distinguish

Same class, same host and fixture, 3 concurrent:

| binary | runs | SIGSEGV | stalls |
|---|---|---|---|
| pristine `origin/dev` `5443fae920` | 34 | **0** | 0 |
| branch merged with dev | 54 | **1** | 1 |
| branch before merging dev | 20 | 0 | 2 |

One event in 54 against zero in controls of comparable size distinguishes
nothing: at a 2% rate a 20- or 34-run control comes up empty most of the time.
A same-binary A/B of that branch's only extra-executable-memory change — the
retry, 20 interleaved on/off pairs — was 20/20 clean on both arms, which is why
nothing here is attributed to it. Do not read the control as exoneration of either tree, and do not read
the single event as a regression. What can be said is narrower and still
useful: the shape is the retirement-unmaps-executing-code family, that family
had a fix land on 2026-07-28, and it has now been seen again.

## How to hunt it

Two flags exist for exactly this, both documented at their definitions in
`jit/src/lib.rs`:

* `CRATONVM_JIT_NEVER_FREE_CODE=1` — never unmap any executable buffer, for any
  owner. If the SIGSEGV vanishes under this it is a use-after-free of code.
* `CRATONVM_JIT_LEAK_CODE=1` — defer only the owners routed through
  `defer_jit_owner`. If the fault survives this but vanishes under the flag
  above, the release path at fault is one the retirement queue does not cover.

`CRATONVM_DBG_JIT_CODE_FREE=1` captures a backtrace at every unmap, which is
the only way to name *which* owner dropped last; the crash report gives an
address and nothing else. Run the class in a loop with the leak flag set and
compare — a single reproduction with `CRATONVM_DBG_JIT_CODE_FREE=1` armed
should name the release site.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` (1 occurrence in 34 runs). Nothing about the fault is specific to this class; it boots a Spring context per test, which is simply a lot of first-time compilation.
