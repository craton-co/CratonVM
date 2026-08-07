# G1 parallel evacuation: a torn `Value` read panics a worker, and the panic becomes an unkillable hang

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | high — a data race in the G1 evacuation path, and it wedges `cargo test -p cratonvm-gc --lib` |
| **Reproduces** | `g1::tests::parallel_young_diamond_shared_children_dedup`, **11/20 standalone on pristine `dev`**, no instrumentation |
| **Discovered** | 2026-08-07, while hunting an unrelated one-in-twenty assertion flake |

## Reproduction

```bash
cargo test -p cratonvm-gc --lib --no-run
timeout 12 <test-binary> --exact "g1::tests::parallel_young_diamond_shared_children_dedup" --nocapture
```

`--nocapture` is not optional. libtest buffers a test's output and prints it
only when the test *finishes*; this test never finishes, so the panic that
causes the hang is invisible without it. That is why the failure looked for a
long time like a plain hang with no signature.

Standalone reproduces far more often than in-suite (~11/20 vs ~2/10), which fits
the mechanism: alone, `parallel_worker_count()` gets the whole machine, so more
workers race.

## What happens, in order

**1. Workers panic on a torn pointer.**

```text
thread '<unnamed>' panicked at types/src/value.rs:132:
ObjectRef pointer not 8-byte aligned: 0x7299462265db
thread '<unnamed>' panicked at types/src/value.rs:132:
ObjectRef pointer not 8-byte aligned: 0x72994622662b
thread '<unnamed>' panicked at types/src/value.rs:132:
ObjectRef pointer not 8-byte aligned: 0x7299462268d3
thread '<unnamed>' panicked at types/src/value.rs:132:
ObjectRef pointer not 8-byte aligned: 0x7299462265db
```

Those are not garbage — they are plausible heap addresses off by a few bytes.
That is a **torn read of a `Value`**: `seed_source_region` / `process_object` do

```rust
let value = std::ptr::read(slot_ptr as *const Value);
...
std::ptr::write(slot_ptr as *mut Value, nv);
```

and two workers are doing that to the *same* object's slots at the same time, so
one reads a half-updated pointer. The test name says exactly which case it is:
in a diamond (A→B, A→C, B→D, C→D) the shared child D must be evacuated **once**.
Both parents can reach D concurrently, and the `fresh` flag that is supposed to
deduplicate the copy is evidently not atomic against a second worker, so D gets
copied twice and its slots rewritten by two workers at once. Note the fourth
panic repeats the first address — two workers on the same object.

**2. The panicking worker leaks the termination counter.**

`SharedEvac::run_worker` pops an item and only decrements *after* processing:

```rust
Some(addr) => {
    self.process_object(...);          // <-- panics here
    if !children.is_empty() { self.outstanding.fetch_add(children.len(), AcqRel); ... }
    self.outstanding.fetch_sub(1, AcqRel);   // never reached
}
```

Unwinding out of `process_object` skips the `fetch_sub`, so each dead worker
leaks exactly one count. Measured, with the counter instrumented:

```text
[EVAC-SPIN] outstanding=4 (0x4) queue_len=0 wrapped=false
```

Four panics, four leaked. Not a `usize` wrap — a straight leak.

**3. Every surviving worker spins forever.**

```rust
loop {
    if self.outstanding.load(Acquire) == 0 { break; }   // never true again
    match self.queue.lock().pop() {
        Some(addr) => { ... }
        None => std::thread::yield_now(),               // <-- here, forever
    }
}
```

Confirmed by stack (gdb via `sudo`; `ptrace_scope` blocks a plain attach) —
every worker thread:

```text
#0  __GI_sched_yield
#1  cratonvm_gc::g1::SharedEvac::run_worker      at gc/src/g1.rs:833
#2  cratonvm_gc::g1::G1Collector::parallel_evacuate::{{closure}}
```

and the driver parked in `thread::scope` waiting for threads that will never
return:

```text
#1  std::sys::pal::unix::futex::futex_wait
#4  scope<...parallel_evacuate::{closure_env#0}...>
#5  parallel_evacuate                             at gc/src/g1.rs:3311
#6  G1Collector::young_collection_parallel        at gc/src/g1.rs:3543
#7  parallel_young_diamond_shared_children_dedup  at gc/src/g1.rs:12182
```

Because `scope` never returns, the workers' panics are never propagated, so the
process burns every core indefinitely instead of failing. Orphans were found on
the build host with **3h08m of CPU for a suite that finishes in 2.5s**.

## Two defects, and they should be fixed separately

**A — the race (the real bug).** Evacuating a shared child twice and letting two
workers rewrite one object's slots concurrently. The fix is to make the
forwarding installation atomic — a CAS on the header so exactly one worker wins
the copy and the loser adopts the winner's forwarding pointer — which is what
the `fresh` flag is already trying to express. This is a correctness change in
the evacuation path and wants its own design + validation pass; it is not a
tidy-up.

**B — the termination protocol is not panic-safe.** Independently of A, a worker
that dies in flight must not be able to convert a crash into an unkillable hang
that also hides the crash. Options: retire the in-flight count on unwind (a
guard whose `Drop` does the `fetch_sub`), and/or have the driver observe worker
death rather than waiting on a counter alone. Fixing B does not fix A — the test
would then FAIL (visibly, with the panic) about half the time instead of
hanging, which is strictly better but does turn the gc suite red until A lands.

## Why the suite looked green

`cargo test -p cratonvm-gc --lib` was run many times during this session and
reported `979 passed; 0 failed` in ~1.5s each time. The hang is intermittent and
load-dependent, and when it does fire the run never reports at all — so it shows
up as a *timeout* in whatever is driving the tests, not as a failure. It is easy
to read that as "the host is slow", which is exactly what happened here for
several rounds.
