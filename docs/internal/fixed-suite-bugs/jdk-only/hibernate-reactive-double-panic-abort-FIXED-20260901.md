# The `--jdk-only` Hibernate Reactive abort — FIXED. It was the panic hook, and the classes had already passed

Retires `known-issues/jdk-only/bug-jdk-only-hibernate-reactive-double-panic-abort-20260830.md`.

## Status

**FIXED 2026-09-01**, on branch `fix/jdk-only-huc-drift-and-hr-panic-20260901`,
branched from dev `222384268`. Root-caused to three named source lines.

The page was honest that §3 was "a correlated hypothesis, not a fix-ready root
cause". It was also wrong, and so was §2's account of the mechanism. Both are
recorded below, because the reason they were wrong is reusable.

## 0. The three things the page could not see

**The abort is not a Hibernate Reactive defect, a `--jdk-only` defect, or a
class-fabrication defect. It is the panic hook.**

| | the page | measured |
|---|---|---|
| which hook printed the message | "Rust's default panic hook" | CratonVM's own, `vm-cli/src/main.rs` |
| what `P1` was | an `.unwrap()` on a `--jdk-only` refusal `Err` (§3, hypothesis) | `LocalKey::with` on destroyed TLS, in `jni_detach_current_thread` |
| what the crashed classes had done | unknown — "CRASH" | **printed a passing `@@RESULT` first** |

The third one reframes the whole page. `BatchFetchTest`, run alone:

```text
@@RESULT org.hibernate.reactive.BatchFetchTest found=3 started=3 ok=3 failed=0 aborted=0 skipped=0 ms=12598
@@BATCHEND failed_classes=0
thread 'vert.x-worker-t' panicked at .../thread/current.rs:315:9
```

The tests ran, passed, and reported. The process then aborted on the way out.

## 1. Reproduced in isolation — first attempt

The page's §Reproduce said a single class was "expected to reproduce it, though
this has not yet been confirmed". It reproduces on the first try:

```bash
cd /data/cratonvm/apps/hibernate-reactive-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 TESTCONTAINERS_RYUK_DISABLED=true \
  cratonvm --java-home "$JDK" --Xmx 1500m --jdk-only @common.args \
  -Dcraton.batch=1 CratonRunner org.hibernate.reactive.BatchFetchTest
# rc=134
```

## 2. `P1` was invisible because the hook that was supposed to print it panicked

`vm-cli/src/main.rs` installs its own panic hook (the I1 "visibility-first"
hook). Rust's default hook was never in play — which matters, because the fix
is in code this repo owns. The hook's second statement was:

```rust
let thread = std::thread::current();
let thread_name = thread.name().unwrap_or("<unnamed>");
```

`std::thread::current()` does not return `None` after a thread's TLS has been
destroyed. It **panics**. A panic raised inside a panic hook is a
panic-while-panicking, and Rust cannot unwind twice: it prints
`thread panicked while processing panic. aborting.` and calls `abort()` —
before the hook has printed one byte about the panic it was called for.

So the 192 byte-identical `thread/current.rs:315:9` messages were not evidence
of one already-found bug, and they were not merely uninformative, as the page
said. They were the hook destroying its own input.

Three more copies of the same call sit in `vm/src/runtime/crash_handler.rs`, two
of them on **signal-handler** paths (`CrashInfo::from_signal`, and the Windows
vectored-exception handler). All four now go through one TLS-free helper,
`crash_handler::current_thread_name`.

### Why not `std::thread::try_current()`

Unstable on the 1.97.1 toolchain this tree builds with. The replacement reads
the OS thread name, which is TLS-free. Two caveats, measured rather than
assumed (glibc 2.39 / Linux 6.17, and Win11):

| | Linux | Windows |
|---|---|---|
| named thread | truncated to `TASK_COMM_LEN-1` = 15 bytes | exact |
| `main` | reports the process name | `main` |
| never-named thread | inherits the **parent's** `comm` | `None` |

The Linux truncation is why the log above says `vert.x-worker-t` and not
`vert.x-worker-thread-2`. Accepted: an aborting process prints no name at all.

## 3. `P1`, once it could be printed

With only the thread-name half fixed, the first panic prints, and
`RUST_BACKTRACE=full` names it outright:

```text
thread 'vert.x-worker-t' panicked at library/std/src/thread/local.rs:428:25:
cannot access a Thread Local Storage value during or after destruction: AccessError
   7: std::thread::local::panic_access_error
   8: jni_detach_current_thread
   9: <unknown>
  10: __GI___nptl_deallocate_tsd   at ./nptl/nptl_deallocate_tsd.c:73:29
  11: __GI___nptl_deallocate_tsd   at ./nptl/nptl_deallocate_tsd.c:22:1
  12: start_thread                 at ./nptl/pthread_create.c:455:3
  13: clone3
```

`DetachCurrentThread` is not only called by application code. A JNI library that
attaches host threads registers its per-thread cleanup with
`pthread_key_create`, and glibc runs those destructors in
`__nptl_deallocate_tsd` — which runs **after** `__call_tls_dtors`, i.e. after
every Rust `thread_local!` on that thread has been destroyed.

`vm/src/vm/realms/native_realm.rs` had already written this mechanism down, for
a different symptom: it is why a loaded JNI library is never `dlclose`d. The
same paragraph explains this abort, one door over.

`jni_detach_current_thread`'s first act was `is_foreign_attached()`, which is
`FOREIGN_THREAD_BOX.with(..)`. That is `P1`. It is also an `extern "C"` frame,
so the unwind was undefined behaviour, not merely a bad diagnostic.

## 4. The measurement that decided the shape of the fix

The TSD phase is all-or-nothing, and Rust's own TLS phase is not. Both were
measured rather than reasoned about (`/tmp/tls.rs`, `/tmp/tls2.rs`):

| caller | `try_with` on a key initialized earlier | `try_with` on a never-touched key |
|---|---|---|
| a Rust `thread_local!` destructor | `Ok` | `Ok` (it still initializes) |
| a `pthread` TSD destructor | `Err(AccessError)` | `Err(AccessError)` |

Two consequences:

1. A detach arriving in the TSD phase can reach **none** of the attachment
   state, so "there is nothing left to detach" is the only truthful answer —
   and `try_with` can give it without panicking.
2. A never-otherwise-touched, destructor-bearing key is a sound **liveness
   probe** for exactly the phase that produced this abort. That is what gates
   the hook's `tracing` mirror in §5.

Rust TLS destructors also run in reverse registration order, and a key
registered earlier is fully readable from a later-registered key's `Drop`
(`Ok(true)`, measured). That is what makes a future exit-guard placement
workable; see §7.

## 5. The hook aborted a second time, on its own tail

Fixing the thread name was not enough. The hook printed `P1` correctly and then
aborted anyway:

```text
thread 'vert.x-worker-t' panicked at .../thread/local.rs:428:25:
cannot access a Thread Local Storage value during or after destruction: AccessError
[cratonvm] jdk mode: real-jdk (java.home=/data/toolchain/jdk-25)
note: run with `RUST_BACKTRACE=1` ...
panicked at .../thread/local.rs:428:25:      <- same line again
thread panicked while processing panic. aborting.
```

The hook's last act is a `tracing::warn!` mirror "so log aggregators that key on
tracing still see the panic". `tracing`'s dispatcher and subscriber stack reach
several `thread_local!`s of their own. On a thread whose TLS is gone, that is a
second panic *inside the hook* — the same abort by a different door.

It is now gated on the §4 liveness probe, so every normal panic keeps its
tracing mirror and a teardown-phase panic prints one extra line saying the
mirror was skipped. The bootstrap-quiet `tracing::debug!` arm is gated the same
way.

## 6. The fix

Three changes, each on the line the measurement named:

| file | change |
|---|---|
| `vm/src/runtime/crash_handler.rs` | new TLS-free `current_thread_name()`; the file's own three `std::thread::current()` calls (panic hook, **signal handler**, Windows VEH) now use it |
| `vm-cli/src/main.rs` | the panic hook takes its thread name from that helper, and gates both `tracing` arms on a TLS liveness probe |
| `vm/src/native/jni.rs` | every thread-local access on the detach path — `is_foreign_attached`, `with_foreign_thread`, `detach_foreign_thread`, `clear_jni_context`, `clear_jni_thread`, `jni_detach_current_thread` — is `try_with` with a defined answer for "the thread is already gone" |

Regression test: `crash_handler::tests::current_thread_name_survives_tls_teardown`
touches its own `thread_local!` **before** `std::thread::current()` so that
reverse-registration order puts std's `CURRENT` handle first in the teardown and
the probe's `Drop` last — i.e. it runs in the exact destroyed-`CURRENT` state
that produced `thread/current.rs:315:9`. A regression re-panics from a TLS
destructor, which aborts the test process rather than failing quietly.

## 7. Measured — the whole suite, both binaries, at the same time

Two `cratonvm` binaries from the same tree, differing only by this fix, run
**concurrently** over the same 206-class `testlist.txt` under `--jdk-only`, one
shard each, 300 s per-class cap. Concurrently on purpose: this host carries
other sessions' work and load ran 20-135 across the run, so an A/B taken at two
different times on it is not an A/B.

| status | base | fixed |
|---|---|---|
| **CRASH** | **184** | **0** |
| PASS | 19 | **201** |
| FAIL | 1 | 3 |
| NOTESTS | 2 | 2 |

Per class, base → fixed:

```text
  CRASH   -> PASS      184
  PASS    -> PASS       17
  NOTESTS -> NOTESTS     2
  PASS    -> FAIL        2     <- 120 s vertx-junit5 method timeouts; see below
  FAIL    -> FAIL        1
```

And the page's own instrument, on the two raw logs:

```text
grep -c 'thread panicked while processing panic'
   base  192        <- the page measured 192 too, across 183 classes
   fixed   0
grep -c 'rc=134'
   base  184
   fixed   0
```

**Every one of the 184 crashes became a pass.** That is §0's point made
quantitative: the classes were already passing, and all the abort destroyed was
the runner's ability to see it. The page counted 183 of 206 and 192 panic
occurrences; this run's base arm counted 184 and 192. Same population.

### The two `PASS -> FAIL` rows are the host, and the A/B says so

Both are a 120 s **method** timeout inside the test framework, on the two most
thread-heavy classes in the suite, on a host whose load crossed 130:

```text
java.util.concurrent.TimeoutException: testIdentityGenerator(io.vertx.junit5.VertxTestContext)
    timed out after 120 seconds
java.util.concurrent.TimeoutException: testWorldRepository(io.vertx.junit5.VertxTestContext)
    timed out after 120 seconds
```

So they were re-run **interleaved** — base, fixed, base, fixed — four rounds
each, so neither binary got a quiet host the other did not:

| class | binary | aborted | failed | runs |
|---|---|---|---|---|
| `MultithreadedInsertionTest` | base | **2** | 2 | 4 |
| `MultithreadedInsertionTest` | fixed | **0** | 3 | 4 |
| `TechEmpowerTest` | base | **3** | 1 | 4 |
| `TechEmpowerTest` | fixed | **0** | 0 | 4 |

`MultithreadedInsertionTest` fails on the **base** binary too, 2 runs in 4 — it
is flaky under load on both, and its one-run `PASS` in the base suite arm was
the luck. `TechEmpowerTest` failed once on base and never on the fixed binary.

Aggregated over the 8 runs per binary: **aborts 5 → 0, failures 3 → 3.** The fix
removes the abort and does not touch the failure rate. Neither `PASS -> FAIL`
row is a regression.

The same table carries the §0 finding one more time, per run rather than per
suite: five of the eight base runs are `rc=134`, and in **four** of those five
the class had already reported `ok=1`.

## 8. What the page got wrong, and why it is worth writing down

**A byte-identical symptom is a claim about the instrument, not the defect.**
The page said this correctly — "the *visible* symptom is structurally incapable
of distinguishing between different underlying triggers" — and then reasoned
about the trigger anyway, from what happened to sit above it in the log. Those
`--jdk-only` refusal WARNs were the last thing printed for the same reason
anything is the last thing printed: the process died. §3's own caveat ("most
`--jdk-only` refusals of these same three classes do NOT lead to an abort") was
already the refutation.

**The fix for an unreadable instrument is to fix the instrument.** Repairing one
line in the hook — with no theory at all about what `P1` was — turned an
undiagnosable class of aborts into a printed panic with a backtrace naming the
function, and it took one build to do it. That is cheaper than any amount of
`CRATONVM_DBG_*` per-thread tracing, which is what §3 proposed next.

**Read the crashed run's own success markers.** `@@RESULT ... ok=3 failed=0` was
in every one of the 183 raw logs, immediately above the abort. The suite runner
classifies a class as `CRASH` when it cannot find that line in the *class's own*
stdout capture — and here the line was there, but the process died before the
capture was drained. "183 classes crash" and "183 classes pass and then the
process aborts on the way out" are very different bugs, and the page's own
evidence distinguished them.

**A mechanism already written down elsewhere in the tree.**
`vm/src/vm/realms/native_realm.rs` documents `pthread_key_create` cleanup
running in `__nptl_deallocate_tsd` "as the thread *finishes exiting*, which is
after `run()` has returned", and that it produced a crash "after `@@RESULT` had
already been printed, so it read as a mystery post-run crash rather than a
teardown bug". That is this defect's mechanism and this defect's symptom,
written down for a different door, weeks earlier.

## 9. Why only `--jdk-only` — it is JNA, and the answer was in the third frame

The page's mode-specificity claim survives a fresh control it did not have (§4
admitted the zero-match check was against an old residual-log corpus, not a
same-day run). `BatchFetchTest` alone, base binary, compatible mode: `rc=0`,
zero panics, `ok=3`. Under `--jdk-only`: `rc=134`.

Two things it is NOT. The loaded native-library set is **identical** — seven
objects in both modes (`libc`, `libcrypto`, `libextnet`, `libjava`, `libgcc_s`,
`libm`, `libssl`), so it is not "strict mode dlopens something compatible mode
does not". (An earlier pass here read `strace` output that counted the loader's
*failed* search-path probes as loads and briefly listed `libjvm.so` among them;
filtering `ENOENT` removes it. `libjvm.so` is never loaded — those were the
loader walking `/lib`, `/usr/lib`, the `glibc-hwcaps` variants and missing at
each.) And `tracing` cannot answer it: the detach path's own `debug!`/`trace!`
lines emit nothing from a TSD destructor in either mode.

Asked from the other end it falls out immediately. Interposing
`pthread_key_create` and printing, for every key that carries a destructor, the
destructor's owning object (`tsdspy.c`, `LD_PRELOAD`):

```text
--jdk-only    3 keys
  dtor=?@/data/bin/cratonvm-...            caller=?@/data/bin/cratonvm-...   (x2)
  dtor=?@/home/azureuser/.cache/JNA/temp/jna1788342378766891624.tmp
                                           caller=?@/lib/x86_64-linux-gnu/libc.so.6
compatible    2 keys
  dtor=?@/data/bin/cratonvm-...            caller=?@/data/bin/cratonvm-...   (x2)
```

**One extra key under `--jdk-only`, and its destructor lives in JNA.**
`jna-5.13.0.jar`'s `com/sun/jna/linux-x86-64/libjnidispatch.so` imports
`pthread_key_create` and exports `JNA_detach` beside
`Java_com_sun_jna_Native_setDetachState` — JNA's documented per-thread detach
state. The destructor calls `(*vm)->DetachCurrentThread(vm)`, which is invoke
table slot 5, which is `jni_detach_current_thread`.

So the whole chain, end to end:

1. under `--jdk-only`, JNA's native library is loaded (Testcontainers' Docker
   client reaches it); in compatible mode it is not;
2. JNA extracts `libjnidispatch.so` to `~/.cache/JNA/temp/jnaNNNN.tmp`, dlopens
   it, and **deletes the file**;
3. it registers a pthread TSD key whose destructor detaches the thread;
4. glibc runs that destructor in `__nptl_deallocate_tsd`, after
   `__call_tls_dtors`, so every Rust `thread_local!` is already gone;
5. `is_foreign_attached()` → `LocalKey::with` → `P1`.

Step 2 is also why the backtrace's frame 9 is `<unknown>` and always will be:
the mapping has no on-disk backing left to symbolicate. That frame is not
missing information, it is a deleted file.

**Nothing in that chain is mode-dependent except step 1.** The defect was never
a `--jdk-only` defect; strict mode only decides whether JNA is on the path. What
remains unanswered is one level further out — *why* the JNA-loading path runs
only in strict mode — and that is a question about Testcontainers' class graph,
not about this defect, which no longer crashes anything either way.

## 10. What is left open

**Nothing about this defect.** §9 names the registrant, the mechanism and the
mode difference, and the crash is gone in both modes.

One question sits one level further out and is deliberately not chased here:
*why* the JNA-loading path runs under `--jdk-only` and not in compatible mode.
That is a question about which classes Testcontainers' Docker client reaches
when CratonVM stops fabricating stand-ins — a class-graph question, not a
threading one. It changes nothing about the fix: every step of §9's chain except
that first one is mode-independent, and the code that aborted had no
mode-dependence at all.

Two smaller things this lane found and dealt with rather than left:

* `harness_exit_shim::unhandled_filter` was a **fourth** copy of
  `std::thread::current()` on a fault path, inside an `extern "system"` SEH
  filter where the unwind would be undefined behaviour. It is
  `#[cfg(all(test, windows))]`, so it was left alone until it could actually be
  compile-checked on Windows rather than changed blind.
* `layout_alias::observe`'s doc listed `declared == 0` among the cases it
  suppresses, where `classify` tests that case *first* on purpose. The contract
  described the recorder as blind to exactly the species the detector had been
  widened to catch.
