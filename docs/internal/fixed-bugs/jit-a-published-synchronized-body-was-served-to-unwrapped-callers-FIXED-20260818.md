# A `synchronized` method's compiled body was served to callers that supply no monitor, so it ran unlocked

**Status: FIXED 2026-08-18** on `fix/jit-synchronized-monitor-lost-20260818`.
Regression vector: `regression-suite/src/RSyncMethodJit.java` (already existed
and is what caught it). Source witnesses:
`cratonvm_jit::tests::every_jit_cache_publication_stamps_the_wrapped_entry_requirement`,
`…::the_callee_cache_fast_path_refuses_a_wrapped_entry_body`,
`…::the_dispatch_helpers_jit_cache_arm_refuses_a_wrapped_entry_body`.

## The symptom

`RSyncMethodJit` went red on dev with

```
java.lang.AssertionError: static monitor lost updates: 239965 != 240000
```

Four threads × 60 000 iterations of a `static synchronized` method's `++`.
The lost count varied run to run — 239947, 239965, 239974, 239987, 239994 —
and the **two instance counters in the same loop were exact every time**. That
asymmetry is the whole clue: `bump()`/`bumpTwice()` lock the receiver,
`bumpStatic()` locks the class mirror, and only the static one leaked.

0.015% loss is what an *occasionally* unlocked counter looks like. A never-locked
one loses thousands. That distinction is why the number is small enough to
dismiss as noise and must not be.

## The bug

CratonVM's compiled bodies carry **no** `ACC_SYNCHRONIZED` monitor
prologue/epilogue. The *caller* supplies it: exactly two entry points wrap the
activation in a `JitSynchronizedMonitorGuard` — `execute_jit_call` and
`execute_jit_call_decoded`. Every other consumer CALLs the raw entry pointer,
so for those a synchronized method simply does not lock.

The codebase knew this. `jit_bridge.rs` carries a comment enumerating the
entries that "would NOT be wrapped" and stating each "must keep" refusing a
synchronized callee. The refusals it names are all real and all still there.
They are also all on **compile** paths — and a compile path is not consulted
when the body already exists:

```rust
// try_jit_compile_callee — the by-name entry point for the UNWRAPPED doors
let jit_cache = shared.jit.jit_cache.read();
if let Some(compiled) = jit_cache.get(class_name, method_name, descriptor, probe_class_id) {
    let entry = compiled.entry_ptr() as usize;
    return Some((compiled, entry, needs_ctx));   // <- no refusal here
}
...
// try_jit_compile_callee_slow — 300 lines further down, the COMPILE path
if method.is_synchronized() && !allow_synchronized_wrapped_entry {
    return None;                                  // <- the refusal
}
```

And who publishes a synchronized body into `jit_cache` in the first place? The
background tiering worker, **on purpose**: `try_jit_compile_wrapped_entry`
passes `allow_synchronized_wrapped_entry = true` precisely because its consumer
is `execute_jit_call`, which wraps. That publication is correct. Serving it to
somebody else is not.

So the shape is: *a body legitimately compiled FOR the wrapped entry, handed
out by a fast path that never re-asked the question its own slow path exists to
ask.* A gate in front of a slow path guards nothing once the fast path can
answer.

`jit_invoke_dispatch`'s own `jit_cache` arm (`vm/src/jit/helpers.rs`) is worse
still: it never goes through `try_jit_compile_callee` at all, and it **caches**
what it takes in `DISPATCH_CACHE`. One unlocked serve makes every later call at
that site unlocked too.

## Why it surfaced now, and why that is not where the bug is

The vector passed on `origin/dev` @ `031fe394b` and fails on `77d8faf96`, ~4
hours of commits later. Nothing in that window touched a single `is_synchronized`
gate (`git log -S is_synchronized` over it is empty). What the window did land
is `ac67dafe8`, the RBC.6b lift: **OSR may now compile a method that has an
exception table.**

`RSyncMethodJit`'s thread body is a lambda whose loop sits after a
`try { latch.await(); } catch (InterruptedException) {}`. Before the lift that
exception table made the lambda OSR-ineligible, so the loop stayed interpreted,
so there was **no compiled caller**, so `DISPATCH_CACHE` never served that
`invokestatic` site and the interpreter's `dispatch_static` arm took the monitor
every time. After the lift:

```
[cratonvm-jitc] tiered-enqueue RSyncMethodJit.bumpStatic()V tier=C1 invoc_count=500 bg=true
[cratonvm-jitc] full-compile  RSyncMethodJit.bumpStatic()V entry=0x... len=422
[cratonvm-jitc] OSR-compile   RSyncMethodJit.lambda$main$0(...)V entry_pc=17
```

— compiled caller, published synchronized callee, raw CALL, no monitor.

The OSR lift is not the defect and should not be reverted. It made a
pre-existing hole reachable, which is the useful thing a lift does.

## Diagnosis

Five measurements, each one command:

1. `--nojit` — **PASS**, 2/2. JIT-shaped.
2. `--Xmx 8g` — fails identically. Not GC pressure.
3. `--XX:UseGc G1` — fails identically. Not the collector.
4. `CRATONVM_JIT_DENY` bisect: denying `RSyncMethodJit.bumpStatic` **or**
   `RSyncMethodJit.lambda` restores PASS. Both ends of one edge — a compiled
   caller reaching a compiled synchronized callee.
5. `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY=0` — **PASS**, 2/2, while
   `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` still fails. That names
   the statically-bound direct-entry door specifically, and is the one-command
   confirmation for anyone re-checking this.

`CRATONVM_JIT=sync-methods=1` also fails on the broken binary, which is what
says the flag was never the issue: it gates one door
(`try_jit_upgrade_with_gate`) and the background tiering door does not ask it.

## The fix

`CompiledMethod::requires_wrapped_entry` — one bool, stamped at publication
beside `owner_class_id`, from the `CachedBytecodeMethod::is_synchronized` every
publication site already holds. It travels with the body, which is what the
doors need: they hold a raw entry pointer and no method handle, so a name-based
predicate is not available to them.

Every unwrapped consumer now refuses a body carrying it:

* `try_jit_compile_callee`'s `jit_cache` fast path — the by-name entry point all
  the direct-call doors share;
* `jit_invoke_dispatch`'s own `jit_cache` arm, which fills `DISPATCH_CACHE`;
* both specialized `get(I)D` scalar routes in the same file. Not hypothetical:
  `java.util.Vector.get(int)` is `synchronized`, so that route was one `Vector`
  receiver away from the same defect.

The refusal falls through to the interpreter route, whose `dispatch_static` /
`dispatch_virtual` arms take the monitor — so the semantics are the ones the
method declares, not merely "safer".

The contract comment in `jit_bridge.rs` said "the three entries that would NOT
be wrapped". It now enumerates all of them and says which ask the artifact bit
rather than a compile-time predicate. The count being wrong is what let the
fast paths drift in: the enumeration said three while `jit_cache` answered for
two more.

`regression-suite/src/RSyncMethodJit.java`'s header claimed the default CORE run
exercised "the ORDINARY interpreted/synchronized path — NOT the compiled one it
was written for". That was false and, believed, would have retired the vector's
most valuable arm. Corrected in place, with the `CRATONVM_DBG_JITC` output that
disproves it.

## Cost

`probes`-style microbench, single-threaded (uncontended, so this is dispatch +
monitor cost, not contention), 2M ops, interleaved arms, best-of-5, ns/op:

| arm | static-sync | virtual-sync | plain-static (control) |
|---|---:|---:|---:|
| HotSpot 25.0.3+9 | 6.3 | 6.5 | 0.0 |
| pre-fix (**unlocked — wrong**) | 130–158 | 1309–1687 | 29–34 |
| pre-fix + `DISPATCH_CACHE_DIRECT_ENTRY=0` (correct route) | 1249–1340 | 1348–1443 | 28–31 |
| **fixed** | 1353–1452 | 1385–1479 | 29–31 |

The honest comparison is the third row against the fourth: **the fix costs what
taking the correct route costs and nothing beyond it.** The 130–158 ns figure is
the price of not locking. The `plain-static` control is unmoved across all four
arms, which is what says the refusal does not touch non-synchronized callees.

What the table also shows is a **pre-existing** ~200x gap to HotSpot for any
synchronized call out of compiled code — the virtual arm already had it before
this change. That is worth its own investigation and is not touched here.

## Reproduction (pre-fix)

```bash
javac -d /tmp/rs regression-suite/src/RSyncMethodJit.java
<cratonvm> --java-home <jdk25> -c /tmp/rs RSyncMethodJit
# -> AssertionError: static monitor lost updates: 239965 != 240000

CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY=0 <cratonvm> --java-home <jdk25> -c /tmp/rs RSyncMethodJit
# -> PASS   (names the door)
```

Or, both ways, through the suite — **both must pass**:

```bash
CV=<binary> JDK=<jdk25> ONLY=RSyncMethodJit bash regression-suite/run.sh
CRATONVM_JIT=sync-methods CV=<binary> JDK=<jdk25> ONLY=RSyncMethodJit bash regression-suite/run.sh
```

## Measured

* `RSyncMethodJit`: **5/5 PASS** fixed (default) and **2/2 PASS** with
  `CRATONVM_JIT=sync-methods`; the unfixed binary in the same worktree fails
  every run.
* `regression-suite/run.sh`, same worktree, same JDK: **61 pass → 62 pass**. The
  one remaining failure (`RImmutableFactoryTypes`) fails identically on the
  unfixed binary and is pre-existing on dev.
* The three source witnesses were each verified to fail when their own check is
  removed. The third one initially did **not**: it matched the identifier
  `requires_wrapped_entry` in the explanatory comment sitting above the filter,
  so it stayed green against a `.filter(|_c| true)`. Both text witnesses now
  match the code form (`if compiled.requires_wrapped_entry` /
  `!compiled.requires_wrapped_entry`). Worth recording as the thing that nearly
  shipped: a witness that reads a comment is a witness that cannot fail.

## Related

* `docs/known-issues/tomcat/webapp-deploy-annotation-scan-interpreted-226x.md` —
  why synchronized methods were admitted to compilation at all. Unaffected: the
  wrapped door (`execute_jit_call`) still uses these bodies; only the unwrapped
  doors stopped.
* `W7-38-jit-aastore-never-called-its-own-check.md` — the same family: a rule
  enforced on one path and not on the path that actually runs.
* `jit-multianewarray-allocated-every-level-with-classid-0-FIXED-20260817.md` —
  found the day before, and the reason this one was found: it was the unexplained
  red left over from validating that fix.
