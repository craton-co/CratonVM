# `thin_lock_inflates_on_contention` timed its contention instead of arranging it

**Status: FIXED 2026-08-18** on `fix/load-sensitive-unit-flakes-20260818`. This
retires the monitor half of
`docs/known-issues/unit-test-load-sensitive-flakes-20260815.md`. The x509 half
is **not** fixed and is re-filed, narrower, as
`docs/known-issues/x509-unknown-oid-test-load-sensitive-flake-20260818.md` —
see [The other half](#the-other-half).

## The bug

The test arranged its contention with two sleeps:

```rust
let h1 = spawn(move || {
    table1.enter(obj, ThreadId(1));
    barrier1.wait();
    sleep(Duration::from_millis(20));      // hold
    table1.exit(obj, ThreadId(1)).unwrap();
});
let h2 = spawn(move || {
    barrier2.wait();
    sleep(Duration::from_millis(2));       // "ensure thread 1 is still inside"
    table2.enter(obj, ThreadId(2));        // must be CONTENDED
    table2.exit(obj, ThreadId(2)).unwrap();
});
```

The barrier only lines the two threads up at the *start*. Everything after it
is an assumption about the scheduler: that thread 2's 2 ms nap lands inside
thread 1's 20 ms hold. A 10× margin is comfortable on an idle box and is not a
guarantee anywhere — a thread that loses its timeslice on a busy 8-core host can
be off-CPU for far longer than 18 ms.

When it doesn't hold, thread 1 has already released, thread 2's `enter` takes an
**uncontended** thin lock, nothing inflates, and the test fails on its own first
assertion. `MonitorTable` behaved correctly in every one of those runs.

## The failure text the original page never captured

The page's stated next step was to loop the suite under contention until one
failed, because *"guessing a mechanism from the code without that text is how
the wrong global gets fixed"*. Reasonable — but the text is obtainable without
waiting for the race, by causing the scheduling instead of hoping for it. Set
thread 1's hold to `0 ms`, i.e. exactly the observation "thread 1 finished
before thread 2 arrived":

```
thread 'threading::monitor::tests::thin_lock_inflates_on_contention' panicked at
vm\src\threading\monitor.rs:3574:9:
assertion `left == right` failed: contention must produce exactly one inflated monitor
  left: 0
 right: 1
```

`left: 0` — no monitor was ever inflated. That is the whole failure, and it
matches every property the page recorded: needs concurrency (two threads must
actually race), passes under `--test-threads=1` (an idle box keeps the margin),
passes alone, and is rare.

Reproducing a timing race is not the only way to identify one. When a test's
correctness rests on a stated timing margin, *forcing* the schedule it assumes
away is a controlled experiment and gives the same text in one run.

## The fix

Arrange the contention instead of timing it. Both sleeps are gone; the two
threads hand off explicitly:

* thread 2 does not attempt entry until thread 1 has published that it **holds**
  the thin lock, so it can never arrive early;
* thread 1 does not release until it observes the object **INFLATED**, so it can
  never leave early.

Deadlock-free by the documented shape of the path under test:
`enter_or_contend`'s `THIN_LOCKED(other)` arm inflates **first** and only then
blocks, so thread 1's wait is satisfied by thread 2 reaching the block — not by
thread 1 releasing. A 30-second deadline turns a regression that breaks that
ordering into a failure with a message rather than a hung suite, and the
assertion says which ordering it is about.

The test still asserts exactly what it did before: one inflated monitor in the
registry, and a mark word that stays `MARK_INFLATED`.

## Measured

| arm | before | after |
|---|---|---|
| thread 1's hold forced to 0 ms (the observed schedule) | **FAIL** `left: 0, right: 1` | — |
| thread 2 arrives 200 ms late (100× the old margin) | would fail | **pass** |
| full `-p cratonvm-vm --lib`, `--test-threads=96`, ×8 | — | **0 failures** |
| the test alone, ×400, box loaded | 0 failures (never reproduced this way) | pass |

The 200 ms perturbation is the load-bearing one: it is the same class of delay
that broke the old test, and the new one is indifferent to it because nothing in
it depends on a duration any more.

## The other half

The x509 test in the original page is **not** fixed here, and was not guessed
at. What this pass established about it:

* **Hypothesis 1 (a shared-name / SPKI collision between the two "Real RSA Root"
  tests) is disproved, twice over.** By construction: `validate_chain` is a
  function of the `&[Vec<u8>]` chain and a `&TrustManagerState` the test builds
  locally, and nothing in the trust path caches or indexes by subject name or
  public key across calls — the anchors map lives in that local state. And
  empirically: 640 executions of the two tests deliberately interleaved across
  16 threads in one process, zero failures.
* **No process-global state on the path.** `x509_manager`'s statics are the
  KM/TM registries the page already ruled out; `validate_chain` reads no env var
  or runtime flag; the crypto layer's statics are the JNI-facing key stores,
  which this path does not touch. `SecureRandom::new()` is a local RNG.
* **Still not reproduced.** ~15 further attempts here (full `--lib` at 64–96
  test threads, with a parallel `cargo build -j32`, and with the `vm` suite
  running concurrently — the page's "two cargo suites sharing the box"), on top
  of the page's 24, plus the 640-iteration concentrated stress.

That leaves no mechanism by which concurrency can change that test's result, and
no reproduction — which is why it stays open rather than being "fixed". The
successor page carries this forward so the next sighting starts from here.
