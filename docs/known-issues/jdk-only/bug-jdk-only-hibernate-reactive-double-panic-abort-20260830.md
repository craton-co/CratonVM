# `--jdk-only` aborts 183 of 206 Hibernate Reactive classes on one panic-during-panic site

**Status: OPEN, not yet root-caused to a single source line.** Mechanism and
scope are exhaustively measured; the specific `unwrap`/panic-producing call
inside the first (swallowed) panic has not yet been located.

## 0. The numbers

Full Hibernate Reactive suite (`testlist.txt`, 206 classes), one shard, real
JDK 25, `--jdk-only`, run `hr-jdkonly-fullsuite-20260830`:

```text
CRASH    183   (88.8%)
PASS      20
FAIL       1
NOTESTS    2
```

Every one of the 183 `CRASH` rows is `process-died rc=134` (`134 = 128+SIGABRT`).
Not a mix of causes — the same `rc` and the same signal, every time.

The identical suite under **compatible mode** (no `--jdk-only`) does not exhibit
this at all: `grep -rl 'thread panicked while processing panic'` and
`grep -rl 'thread::current'` across this project's existing non-jdk-only
residual logs (`/data/hr-nonpassed-zgc-rerun-20260827`, `/data/hr-fail-g1.txt`,
`/data/hr-fail-zgc.txt`, `/data/hr-fail-generational.txt`) return **zero**
matches. This is a `--jdk-only`-specific failure mode, not a latent Hibernate
Reactive / CratonVM defect that strict mode merely exposes more often.

Netty's own `--jdk-only` full-suite run, taken the same day
(`netty-jdkonly-fullsuite-20260830`, 733 classes, 70 FAIL / 20 HANG / 22
ABORTED / 581 PASS), has **0** occurrences of `SIGABRT` in its raw log. Netty's
`ABORTED` rows are a benign JUnit assumption-skip classification (each still
carries a large `ok` count alongside a handful of `aborted`), not a process
crash, and are unrelated to this defect.

## 1. The one panic site, confirmed across every occurrence

`/data/hr-jdkonly-fullsuite-20260830/run-20260830-064325-passed/on-real/shard-0/raw.log`:

```text
grep -c 'thread panicked while processing panic' raw.log   -> 192
grep 'panicked at' raw.log | sed 's/.*panicked at //' | sort -u
  -> /rustc/8bab26f4f68e0e26f0bb7960be334d5b520ea452/library/std/src/thread/current.rs:315:9:
     (exactly one distinct value)
```

192 occurrences across 183 crashed classes (some classes' worker pool aborts
more than one thread before the process dies), all reporting the identical
panic site:

```text
panicked at /rustc/.../library/std/src/thread/current.rs:315:9:
use of std::thread::current() is not possible after the thread's local data has been destroyed
thread panicked while processing panic. aborting.

#
# A fatal error has been detected by the CratonVM Runtime Environment:
#  SIGABRT at pc=0x72340de9ec0c, ...
#  jdk mode: real-jdk
```

## 2. Why the log never shows the actual trigger

This is Rust's **panic-while-processing-a-panic** abort, not a single panic.
Some first panic `P1` fires on a worker thread; Rust's default panic hook runs
to format and print `P1`'s "thread '<name>' panicked at ..." message, and that
formatting itself calls `std::thread::current()` to get the thread's name. If
the thread's TLS has already been torn down (i.e. `P1` fires during that
thread's own exit/cleanup path), `std::thread::current()` panics — call it
`P2` — while the runtime is still inside the handler for `P1`. Rust cannot
safely unwind twice, so it calls `abort()` immediately.

The consequence for diagnosis: **`P1`'s own message is never printed.** The
hook panics before it gets to print anything, so every raw.log occurrence
shows only `P2`'s fixed message (`thread/current.rs:315:9`), regardless of
what `P1` actually was. This is why all 192 occurrences look byte-identical —
it is not evidence of one bug already found, it is evidence that the *visible*
symptom is structurally incapable of distinguishing between different
underlying triggers.

## 3. What immediately precedes it (correlated, not yet proven causal)

The lines directly before each crash are a burst of `--jdk-only`
"refusing to fabricate a compatibility stand-in" WARNs against
**CratonVM-internal** classes — not Hibernate/Vert.x application classes:

```text
class=cratonvm/internal/SystemLogger        requested_by=native-builtins/src/lib.rs:28925
class=cratonvm/stream/LazyOp                requested_by=native-collections/src/lib.rs:26755
class=cratonvm/internal/foreign/MemorySegmentImpl  requested_by=native-builtins/src/panama.rs:192
```

followed immediately by the double-panic abort. These are exactly the kind of
internal support classes `--jdk-only`'s design doc calls out as refused rather
than silently fabricated (see the mode banner: "Wave-1 enforcement covers
class fabrication and synthetic-stub registration; remaining violations are
recorded and counted"). The working hypothesis — **not yet confirmed** — is
that one of these refusals turns what used to be a silently-successful
fabricated-stand-in path into a `Result::Err` that some worker-thread-local
cleanup code `.unwrap()`s, and that unwrap is `P1`; it fires late enough in
that thread's lifecycle that its TLS is already gone by the time the panic
hook tries to run.

This has not been isolated to a specific call site. The refusal WARNs
themselves are rate-limited and repeat across hundreds of classes without a
crash — most `--jdk-only` refusals of these same three classes do NOT lead to
an abort — so the correlation alone does not identify which specific refusal,
or which specific caller of it, is `P1`. `CRATONVM_DBG_*`-level per-thread
tracing across a repro would be needed to catch `P1` itself before the hook
destroys it.

## 4. What is NOT claimed

* This is not shown to be Hibernate-Reactive-specific in mechanism — only in
  observed rate. The trigger classes (`SystemLogger`, `LazyOp`,
  `MemorySegmentImpl`) are CratonVM-internal, not Hibernate/Vert.x classes, so
  any sufficiently async/multi-threaded workload under `--jdk-only` is a
  plausible candidate; Hibernate Reactive's heavy use of Vert.x's own worker
  pools plus per-test JVM-level connection teardown likely just hits the
  thread-exit-during-panic window far more often than Netty's or Spring's
  workloads did.
* The exact `.unwrap()`/`.expect()` site producing `P1` has not been located —
  §3 is a correlated hypothesis, not a fix-ready root cause.
* Not compared against the `--real-jdk` (non-`--jdk-only`) mode's crash rate on
  this same suite in this run — the historical zero-match check in §0 is
  against this project's existing residual-log corpus, not a fresh same-day
  control run.

## Reproduce

```bash
CV_BIN=<release cratonvm> EXTRA_VM_ARGS='--jdk-only' \
  <hibernate-reactive-suite-runner script> --list testlist.txt --shards 1 \
  --out <outdir>
grep -c 'thread panicked while processing panic' <outdir>/.../raw.log
```

Any single crashed class (e.g. `org.hibernate.reactive.BatchFetchTest`) run
alone under `--jdk-only` is expected to reproduce it, though this has not yet
been confirmed in isolation — all 183 crashes so far are from the full-suite
run.
