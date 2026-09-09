# A reference is already dead on the operand stack when an invoke pops it

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-09. Deterministic. Narrowed to the operand stack by four probes; the producing PUSH is not yet named. |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. Passes at every threshold `>= 524288`, and unset. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64, ~15 s |
| **Not the collector** | Eight hypotheses about the GC were each refused by a measurement — see the ruled-out table in [the internal page](../../internal/springboot/bindabletests-stale-objectref-family-across-allocation-20260909.md). |
| **Was** | the residual of `bindabletests-local-holds-an-interior-word-of-a-retired-tlab-filler-20260909.md`, which is retired: four defects of this family are fixed and that page's own finding was one of them. |

## The finding, in one line

An object reference sitting on an interpreter frame's OPERAND STACK names memory
that no live object occupies, and the invoke path faithfully carries it into the
callee's locals. The invoke path is not the defect — four probes, each a step
further upstream, put the value already dead before the first of them. What
pushed it onto the stack is the open question.

The page was first filed with the invoke path as the culprit; the section
[Where the chain actually terminates](#where-the-chain-actually-terminates-measured-2026-09-09-after-the-page-was-filed)
is the measurement that moved it, and the reason the title changed.

## The report that names it

`CRATONVM_DBG_DEADREF_STORE=1`:

```text
[deadref-arg] INACTIVE-SEMISPACE push_args_to_locals: arg[5] = 0x20012000228 is
  already dead as it is laid into the callee's locals — the argument slice was
  held across a collection.
    at frame::push_args_to_locals          frame.rs:801
    at frame::Frame::new_pooled            frame.rs:1300
    at interpreter::invoke::try_stackless_invoke   invoke.rs:4779
    at interpreter::invoke::execute_invoke_kind    invoke.rs:2618
```

`INACTIVE-SEMISPACE` means the address is inside the semispace the last moving
cycle emptied, which holds no live object by construction. The value was already
dead at the moment it became a Java-visible local.

## Repro

```bash
CRATONVM_DBG_GC_STRESS=262144 \
CRATONVM_GC_RESERVE=0 \
CRATONVM_DBG_DEADREF_STORE=1 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

`CRATONVM_GC_RESERVE=0` keeps decommitted granules mapped so a stale read is
reportable instead of a SIGSEGV. **`--nojit` reproduces identically and names the
same sites** — that is what says the JIT is not involved.

## Where the chain actually terminates (measured 2026-09-09, after the page was filed)

The title above is where the report FIRES, not where the value goes bad. Four
probes, each one step further up, moved it:

| probe | fires? | what it means |
|---|---|---|
| `push_args_to_locals` | yes | the callee's locals get a dead value |
| `try_stackless_invoke ENTRY` | yes | it was dead before that function's prologue — the prologue is not the producer |
| `InvokeArgsRootGuard::refresh` | yes | the pin slots themselves hold a dead value |
| `InvokeArgsRootGuard::new` | **yes** | **it was dead before the guard pinned it** |

`InvokeArgsRootGuard::new` runs immediately after `execute_invoke_kind` reads
the arguments out of the caller's operand stack, and nothing between the two
allocates. So:

* the invoke path's argument pinning is **not** the defect — the guard is doing
  exactly what it claims, on values that were already wrong;
* `load_and_forward`, applied to every popped reference a few lines earlier,
  did not recover it either — it reads the forwarding word at the old address,
  and that word is gone once the allocator has re-served the span, which is
  precisely the window these references are read in;
* therefore **the caller's operand-stack slot held a dead reference**, and the
  remaining question is what pushed it there.

`[deadref-local]` covers `Frame::set_local` and reports zero, so it is not a
local store. The next probe is the operand-stack push — most plausibly a
return-value push or a `getfield` result — and it needs to be cheap enough for
that path, which is why it was not simply added alongside the others.

## What the next step is

Probe the operand-stack PUSH with the same predicate
(`gen_heap::dead_young_ref_reason_global`) and read the Rust backtrace. That path
is hot, so the probe wants the same `OnceLock<bool>` gate the other five arms
use, and it wants to sit on the few pushes that can carry a reference in from
outside the frame — a method return value and a `getfield`/`aaload` result —
rather than on `ValueStack::push` itself.

Do **not** reach for the pin idiom here. It closed four sibling defects on
2026-09-09 (see the internal page) and it is the right tool when a Rust local
outlives an allocation, but an operand-stack slot is already a GC root: if the
value in it is dead, either something wrote it dead, or the frame-root remap
missed that slot. Those have different fixes and the probe distinguishes them.

## Ruled out

* **Not the TLAB allocator.** Three full-span tripwires — a carve handing out
  occupied memory, a tail filler burying live objects, a bump running through a
  retired chunk — all read zero on the failing run.
* **Not the object-start walk or the evacuator.** The referent was *copied*:
  `POST-COPY fate: pointer_map=Some(0x20012000048)` on the cycle that moved it.
  Nothing was reclaimed while referenced; a holder was left un-rewritten.
* **Not the per-bci local-liveness filter.** `CRATONVM_NO_LOCAL_LIVENESS=1`
  reproduces.
* **Not the card table / remembered set.** The `java/lang/Module slot=0` missing
  old→young edge that earlier pages chased was a dangling pointer written by
  `build_module`, now fixed; `[rset-verify]` reports zero missing edges.
* **Not a heap field or a frame slot left un-forwarded.** `[heap-stale]
  UN-FORWARDED` and `POST-GC STALE LOCAL`/`STACK` are all zero.
* **Not the JIT.** `--nojit` fails at the same sites.

## Instruments this page depends on

All added 2026-09-09, all default-off, all documented at their sites:

| flag | report |
|---|---|
| `CRATONVM_DBG_DEADREF_STORE` | `[deadref-arg]`, `[deadref-pin]`, `[deadref-store]`, `[deadref-local]`, `[deadref-capture]`, `[tlab-audit]` |
| `CRATONVM_DBG_WATCH_ADDR=<hex>` | `[watch 0x…]` — one address's carves, fillers, bumps, hand-outs, object-start verdicts and post-copy fate, in cycle order |
| *(always on)* | `evacuator_verdict=` on `POST-GC RECLAIMED-WHILE-HELD` |
