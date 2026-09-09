# RETIRED — a stale VALUE reached `set_field`, and the audit could not see the native it came from

| | |
|---|---|
| **Status** | **RETIRED 2026-09-08.** Root-caused, fixed, and the blind spot that hid it closed. |
| **Was** | OPEN — reproducible and deterministic, NOT root-caused. |
| **Reproducer** | `probes/NativeLoopReceiverSweep.java`, the `growth` section alone, under three debug flags |
| **Verdict** | `MethodHandles.lookup()` stored a PRE-GC class mirror into a freshly allocated `MethodHandles$Lookup` |
| **Sibling** | `natives-loop-carried-stale-receivers-RETIRED-20260907.md`, whose probe found this |

## What it was

```
CRATONVM_DBG_GC_STRESS=65536 CRATONVM_DBG_FORCE_MOVING=1 CRATONVM_DBG_STALE_OBJREF=1 \
  cratonvm --XX:UseGc Generational -Xmx256m -cp out NativeLoopReceiverSweep growth
```

```
[storechk] set_field: STALE stored VALUE 0x…01c0 (receiver 0x…0438 slot 0) thread=main-vm cycle=460
# SIGSEGV
```

0 of 3 runs, at minor cycle **460 every time**. The page recorded that it needed
all three flags, that it was Generational-only, that `-Xint` did not change it,
and that it did not reduce — the same loop extracted into a standalone class
passed. That last observation is what made it look mysterious, and it was the
clue: the loop was never the defect.

## What it actually was

`CRATONVM_GC_RESERVE=0` keeps the evacuated granules mapped, so the canary
panics with a backtrace instead of faulting. The backtrace named the frame in
one line:

```
  9: set_field                gc/src/gen_heap.rs:5403
 10: {closure#0}              native-builtins/src/lang_invoke.rs:7042
 ...
 39: initialize_class_shared  vm/src/vm/vm_util.rs:1335
```

and the holder scan named the caller: *"native invoked from
`java/util/concurrent/ConcurrentSkipListMap.<clinit>()V`"*. The
`ConcurrentSkipListMap` in the probe's `growth` section was not the defect
either — its `<clinit>` calls `MethodHandles.lookup()`, and that native is:

```rust
let caller_class = /* … ensure_class_initialized … get_class_mirror … */;
let obj = try_alloc_concurrent_synthetic(ctx, ".../MethodHandles$Lookup", 3)?;   // ALLOCATES
ctx.set_field_by_name(obj, "lookupClass", caller_class);                          // pre-GC address
ctx.set_field(obj, 0, caller_class);
```

A class mirror resolved BEFORE an allocation and stored into a live object
AFTER it. Straight-line, textbook rule-1 shape, no loop anywhere.

**Three of the four `MethodHandles$Lookup` factories in that one registrar had
it, and the fourth already had the fix** — `privateLookupIn`, whose comment
reads *"`alloc_concurrent_synthetic` can run a moving GC, after which the
`target` ObjectRef the VM handed us in `args` is stale … the old code wrote the
pre-GC ref straight into `lookupClass`"*. `lookup()`, `publicLookup()` and
`Lookup.in` sat three screens away without it.

## Why nothing found it: a closure body was ONE statement

`register_p63_method_handles_lookup` is 410 lines and reassembled into **26
statements**. Both rules ask "is there a GC-capable statement BETWEEN the
binding and the use", and

```rust
r.register(cls, name, desc, |ctx, args| { ..the whole native.. });
```

opens a paren on the first line that does not close until the last, so the
entire body was a single statement and there was no "between" to range over.
That is how most natives in this tree are registered. The audit reported **0
candidates** for the function whose first run under a stale-reference detector
crashed.

Three corrections to `statements()`, each with the instance that forced it:

* **A closure body is its own paren-depth scope**, ended at the brace that
  actually closes it — counted, not guessed. A first attempt popped on any line
  starting with `}`, which `} else {` satisfies, and the body re-merged exactly
  as before.
* **A closure body is also a NAME scope.** Splitting alone let a binding in one
  `r.register` closure pair with a use in another: a row binding `t` at line
  9261 and "using" it at 15073, five thousand lines and a hundred closures
  later. Statements carry the path of closure bodies they sit in, and both rules
  stop at the end of the binding's own scope. The path only ever grew until a
  second bug was found in it — a nested closure's opening brace was charged to
  its parent, so no frame could ever reach its own close.
* **A `let` whose initializer is a BLOCK stays whole.** The existing guard
  suppressed only a `{`/`}` line ending, so `let caller_class = if let ..
  { let x = ..; .. };` tore into four statements and the binding kept none of
  the calls that produce the reference — no GC-capable RHS, nothing for the
  type filter to match.

With those, the pre-fix `MethodHandles.lookup()` reports two rows on the
DEFAULT rule, at :7023 and :7082, and none after the fix. That pair is a new
permanent calibration.

## What the blind spot was hiding, beyond this

`--loops` went from 0 to 25 the moment closure bodies became visible, in
`register_bc_sic_ctr` (a `processBlock` dispatch per cipher block),
`BufferedInputStream.read` (a delegate `read()` per byte), `Selector.select(J)`,
`HttpRequest.Builder.headers`, `Thread.dumpThreads`, `PosixFilePermissions.toString`,
two `HashMap.forEach` bodies, `Phaser.arriveAndAwaitAdvance`, six
`ArrayBlockingQueue`/`LinkedBlockingQueue`/`LinkedTransferQueue` monitor-wait
loops, and three `Stream.flatMap` variants. All 25 are fixed by the same
insertion the sibling page uses; `--loops` and `--launder` are back to **0**
across all four native crates.

The default rule's count rose from 617 to 938 in `native-builtins` over the same
change. That tranche is not this page's and not the sibling's — it is the
2026-08-25 straight-line family, and 321 of its members were simply never
scanned. Nobody has read them.

## Measurement

Two release binaries, same machine, same probe, `--XX:UseGc Generational -Xmx256m`,
all three flags:

| section | branch tip before this work | after |
|---|---:|---:|
| `growth` | **0/3 — SIGSEGV** | **3/3** |
| every other section | 3/3 | 3/3 |
| full sweep | — | 5/5 |

## What this does NOT establish

The three-flag harness is a detector, not a workload: under the default
configuration the probe passed on both binaries before and after. What the fix
establishes is that the stale store is gone at its source, and that the rule
which should have found it now does.
