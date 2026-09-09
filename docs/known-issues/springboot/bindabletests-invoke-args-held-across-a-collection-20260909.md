# An invoke's argument slice is held across a moving collection, and the callee's locals get the pre-move addresses

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-09. Deterministic. **Root-caused to a mechanism and a call path**; the fix is scoped but not written. |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. Passes at every threshold `>= 524288`, and unset. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64, ~15 s |
| **Not the collector** | Eight hypotheses about the GC were each refused by a measurement — see the ruled-out table in [the internal page](../../internal/springboot/bindabletests-stale-objectref-family-across-allocation-20260909.md). |
| **Was** | the residual of `bindabletests-local-holds-an-interior-word-of-a-retired-tlab-filler-20260909.md`, which is retired: four defects of this family are fixed and that page's own finding was one of them. |

## The finding, in one line

`interpreter::invoke::try_stackless_invoke` receives its arguments as
`args: &[Value]` — a slice the caller popped off the operand stack into a Rust
`Vec` — and builds the callee's frame from it after a prologue that can complete
a moving young collection. The `Vec` is neither scanned as a GC root nor
remapped, so every object argument in it names a pre-move address, and
`push_args_to_locals` lays those straight into the callee's locals.

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

## What the fix has to be careful about

The idiom is settled: pin the object arguments, run the GC-capable work, read
them back from the pins. Four sibling defects were fixed exactly that way on
2026-09-09 (see the internal page). What makes this one different is only that it
sits on the hottest path in the VM, so it must **not** become an unconditional
pin per argument per invoke.

The prologue between "args received" and "frame built" is what needs bracketing,
not the whole function. Two candidates for the GC-capable step:

* class initialisation of the callee's declaring class (`<clinit>` runs
  arbitrary Java and allocates), and
* `monitor_enter_synchronized_method`, which blocks at a GC-safe point.

Confirm by which one, bracket that, and check `[deadref-arg]` goes silent. The
same audit applies to `try_stackless_invoke`'s siblings —
`execute_invokestatic`, `execute_invokevirtual_cached`, `execute_invoke_kind` —
all of which reach `Frame::new_pooled` / `Frame::new_from_arcs` with a
caller-owned slice.

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
