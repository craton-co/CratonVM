# A native's argument snapshot goes stale when the Java it re-enters collects

| | |
|---|---|
| **Status** | **FIXED 2026-09-06.** 0/5 on every configuration that previously failed 3/3 or 5/5, answers byte-identical to HotSpot. |
| **Symptom** | `EXCEPTION_ACCESS_VIOLATION` reading `[obj+8]` inside `gen_heap::get_field`, on `main-vm`, under `-XX:+UseGenerationalGC` with the JIT on |
| **Found via** | `bench-gpu/residency-gc.sh`, red on `dev` — but the GPU had nothing to do with it |
| **Fix** | `native-builtins`: `stream_write` / `stream_writeln` / `stream_writeln_inner` keep the receiver in a `NativeHandleScope` and re-read it after every Java re-entry |
| **Reproducer** | `GpuResidencyGc 0 1024 800` under Generational, ~20 s, no GPU |

## The defect

`vm_exec::safe_native_call_impl` hands a native a `&[Value]` snapshot of its
arguments, taken before the callback. It pins every argument into
`thread.native_pin_roots`, so nothing is ever collected out from under a
native — but it rebuilds the snapshot from those pins only for a collection it
runs **itself**, at one of the three GC hooks that sit *before* the callback.
Its own comment says why they sit there:

> the wrappers — `ctx.alloc_object`, `new_array`, … — must stay GC-free
> mid-callback, since their callers hold unrooted local `ObjectRef`s

A native that **re-enters Java** breaks that premise. The bytecode it invokes
allocates, so it can collect, and a Generational young collection RELOCATES —
by Cheney copy on the moving path, and by selective promotion even on the
non-moving one. The pins keep the object alive at its new address. The
snapshot keeps naming the old one.

`stream_writeln_inner` is the instance that crashed. It uses its `args` slice
five times across two Java-interpreting helpers:

```rust
let sep = host_line_separator(ctx);
if printstream_refuse_if_closed(ctx, args) { return; }
if surefire_forwarding_write(ctx, args, text, true) { return; }
let encoded = printstream_encode(ctx, args, &line);   // runs the JDK charset encoder
if route_write_through_out(ctx, args, &buf) { ... }   // runs Writer.write
if let Some(fd) = stream_fd(ctx, args) { ... }        // <-- dereferences a PRE-CALL address
```

`stream_fd` then reads the receiver's mark word. Under Generational that
address is the inactive semi-space or — with `CRATONVM_GEN_UNCOMMIT` on, the
default since 2026-09-05 — a **decommitted page**, and the process dies.

## The two faces of one bug, and why only one of them was visible

`CRATONVM_GEN_UNCOMMIT=0` does not fix it. It changes what the stale read
finds:

| `CRATONVM_GEN_UNCOMMIT` | what the read hits | what happens |
|---|---|---|
| `1` (default since 2026-09-05) | a decommitted page | `EXCEPTION_ACCESS_VIOLATION`, process dies |
| `0` | a still-mapped page, zeroed | header decodes as `ClassId(0)` / `java.lang.Object` with no slots; the field guard drops the read and warns; **one line of output silently vanishes** |

The uncommit default is what turned a silent output loss into a crash, which
is why `residency-gc.sh` was green on 2026-09-05 and deterministically red on
2026-09-06. That is worth stating plainly: the crash was the *good* outcome —
it is the only reason anybody looked.

## Scope

Generational and the JIT. Nothing else:

| arm | result |
|---|---|
| `-XX:+UseGenerationalGC`, JIT on, rounds=800 | **3/3 bad** |
| `-XX:+UseGenerationalGC`, `--nojit`, rounds=800 and 2000 | 0/2 |
| `-XX:+UseZGC` | 0/5 |
| `-XX:+UseG1GC` | 0/5 |
| HotSpot | pass |

## The GPU was never in it

This was found through `bench-gpu/residency-gc.sh`, and the first page written
about it scoped it as "`--gpu` **and** the JIT **and** an offload that actually
happens". That was wrong, and the measurement that says so is one line:
**plain Generational with no `--gpu` anywhere fails 3/3 at `rounds=800`.**

GPU offload is an AMPLIFIER, not a cause. It makes the failure appear at
`rounds=200` instead of `rounds=800` — a 4x reduction in the pressure needed —
because `ArrayWriterPolicy::Barrier` admits primitive-array-writing methods to
the JIT that were previously refused, which raises the allocation rate and the
number of collections landing inside a native.

Every GPU-side hypothesis was tested and refuted, each on one binary:

| hypothesis | lever | verdict |
|---|---|---|
| the residency cache holds a stale key | `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow` (cache off) | 5/5 — cache irrelevant |
| the compiled-store barrier's register clobber | barrier wrapped in `PUSH`/`POPFQ` | 5/5 — clobber innocent |
| the barrier's dirty byte | dirty store replaced with `NOP`s | 5/5 — side effect innocent |
| the conservative JIT root scan | `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` | identical |
| the in-place old-gen sweep | `CRATONVM_OLD_SWEEP_JIT=0` | identical |
| a missed remap across a young COPY | `CRATONVM_GC_NO_MOVING_YOUNG=1` | identical — selective promotion relocates on that path too |

**A correction worth keeping.** An earlier pass reported a clean 2x2 showing
the barrier's dirty byte was "the whole cause" — 3/3 with it, 0/3 without.
That was measured at a configuration sitting exactly on the failure's
threshold, where any perturbation moves the outcome. Re-run at parameters that
fail 5/5, every cell of that 2x2 fails. The result was an artifact of a
marginal operating point, and the lesson is the one this tree keeps relearning:
a lever that looks decisive at the edge of reproduction has not been tested,
it has been sampled.

The same trap caught the investigation twice. Adding an off-by-default probe
inside `try_dispatch` made the default configuration stop crashing entirely —
not because the probe fixed anything, but because a few instructions of code
motion moved the threshold. Raising `rounds` brought it straight back.

## The fix, and the fix that was rejected

`stream_write`, `stream_writeln` and `stream_writeln_inner` now open a
`NativeHandleScope`, root the receiver, and rebuild the argument snapshot
through `args_with_live_receiver` after every step that can re-enter Java.
That is the tree's own documented mechanism for this hazard — see
`NativeHandleScope`'s doc comment, which describes exactly this pattern — and
it is already used at four other sites in the same file.

**A screen was written first and rejected.** The obvious cheap fix is to have
`stream_fd` validate each hop with `heap.is_object_address` before
dereferencing it. It was built, and its own instrumentation is what refuted
it: the report said **hop 0**, i.e. the receiver handed to the native was
already stale before the walk began. Screening it therefore does not repair
anything — it converts the crash into a silently dropped line of output, which
was visible in the very run that demonstrated it (`gc_mixed` missing from an
otherwise clean five-line result). A screen that turns a crash into silent
data loss is worse than the crash.

The `is_live_heap_object` predicate that screen needed was removed with it. If
a later speculative walk wants one, it is three lines in `vm_exec`.

## Verification

Same binary, same host, RTX 2060 box, Windows 11, JDK 25.0.3.

| configuration | before | after |
|---|---|---|
| Gen, no `--gpu`, rounds=800 | 3/3 bad | **0/5**, 5/5 complete |
| Gen, no `--gpu`, rounds=2000 | 3/3 bad | **0/5**, 5/5 complete |
| Gen + `--gpu`, rounds=200 | 5/5 bad | **0/5**, 5/5 complete |
| Gen + `--gpu`, rounds=800 | 5/5 bad | **0/5**, 5/5 complete |

Answers are byte-identical to HotSpot on `GpuResidencyGc 0 1024 800`
(`diff` clean over all six output lines).

## What this does NOT close

The hazard is a PROPERTY OF THE CONTRACT, not of these three functions. Any
native that holds a raw `ObjectRef` across an `invoke_*` has it. Two things
follow:

* `docs/known-issues/springboot/generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md`
  is the same shape — Generational, JIT on, ZGC/G1/`--nojit` all clean, and
  victims (`sun/nio/ch/NativeThreadSet`, `FileChannelImpl`) that are exactly
  the objects an `sun/nio/ch` native holds across a re-entrant call. It is a
  DIFFERENT native, so this fix does not touch it, but the mechanism named
  here is the one to test there first — and that page now has a 20-second
  local reproducer to calibrate against instead of a multi-minute Spring Boot
  Kafka test on Azure.
* A general remedy exists and was not attempted: `safe_native_call_impl` could
  expose the current address of any pinned argument, so a native never has to
  hold a raw one. That is a hot-path change to every native call and needs its
  own measurement.
