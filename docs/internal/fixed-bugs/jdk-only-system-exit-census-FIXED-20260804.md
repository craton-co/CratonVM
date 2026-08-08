# `System.exit(N)` bypassed the JDK-only census — CLOSED 2026-08-04

**Status:** FIXED.

## What changed

`vm-cli`'s pre-exit hook now writes the census. The obstacle was never access —
the hook already resolves the live VM — it was **locking**: all three writers
take the class-manager lock, and a blocking acquire from an arbitrary Java
thread is both a deadlock risk and a lock-order violation, which would turn a
lost file into a hung process. That is strictly worse than the bug.

So:

* each of the three writers gained a `_with(cm: Option<&ClassManager>)` form;
* `SharedVm::try_write_jdk_only_dumps_for_exit` takes **one** non-blocking
  `OrderedPlRwLock::try_read_untracked` and passes the result to all three, so
  the artefacts describe the same instant. The `_untracked` spelling is new and
  documented as sound *only because it cannot block*: the tracked `try_read`
  panics on a descending-order acquisition, which is right for ordinary code
  and wrong for a terminal path whose job is to write a file and let the
  process die. There is no retry loop and no timeout, deliberately;
* on failure each artefact is written in a labelled `"partial": true` form.

**The partial forms are shaped so they cannot read as green**, which the record
called the single worst outcome available. The class-origin census omits
`counts` entirely rather than zero-filling it; the report omits the four class
buckets rather than writing `"compatibility_classes": 0`; the native census
writes `real_declaring_method: null` rather than `{"loaded": false, …}` — that
last is a *measurement* saying the run never touched the class, and it would be
a lie.

Both writers share one `WRITTEN` latch, so a `System.exit` racing a normal
shutdown cannot interleave. The launcher's `--dump-*` paths are published to a
`OnceLock` right after `Args::parse_from`, because the hook is a bare `fn(i32)`
installed before parsing; that is launcher state (one process, one command
line), not the per-VM state contract §2 is about, and it sits alongside the
equally process-global `PRE_EXIT_HOOK` it cooperates with.

## Verification

A `--jdk-only --jdk-only-report … --dump-class-origins …` run of a program
whose `main` ends in `System.exit(7)`, against a real JDK 21 image:

```
PROBE: about to exit
[cratonvm] System.exit(7) called — process terminating
[cratonvm] wrote JDK-only census on System.exit
```

Both files present, both complete (no `"partial"` key — the lock was free), and
the report's violation list matches the same program returning normally.
`Compatible` mode adds exactly zero work: the three-`is_none()` guard returns
before touching a lock or the filesystem.

`Runtime.exit(int)` shares `invoke_pre_exit_hook` with `System.exit(int)`, so
it is covered by construction.

## Not verified by a run, and worth knowing

The **partial** path itself. Reaching it needs the class-manager lock held at
the instant of `System.exit`, which is a race, not something a probe can
schedule. The degraded renderers are exercised as pure functions instead; what
a run has not shown is a real thread losing that race. The failure mode if the
degraded path is wrong is a mislabelled file, not a hang — the try-lock is
what rules the hang out, and that is structural.

---

*The original filing follows unchanged.*

---


**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31 from the wave-1
re-land. Not a correctness bug: nothing runs wrong, and `Compatible` mode is
unaffected. It is an **evidence** bug, and it is selective in the worst possible
way — the strict runs that fail early are exactly the runs that leave no
artefact behind.

## What is wrong

`vm-cli`'s `write_jdk_only_dumps` writes the three census artefacts
(`--dump-class-origins`, `--dump-native-registry`, `--jdk-only-report`). It is
called through `finish_jdk_only` from **four** sites in `run()`, and the choice
of four is deliberate — its doc comment says so:

> Called from every exit path that has a live VM, including the failing ones:
> `difftest` categorises a *failing* strict run from these files, so a run that
> dies on the violation it was launched to find must still leave the census
> behind. Writes at most once per process (first caller wins — the failure path
> is the informative one).

The four:

1. **Main-class load failure** — `if let Err(e) = vm.load_class(&class_name)`.
   Its own comment: *"this is the likeliest strict-mode failure — the main class
   (or something it needs) was refused rather than fabricated. The census must
   survive it, or `difftest` cannot categorise the failure it was launched to
   produce."*
2. **`premain` abort** — a `-javaagent:` agent threw a fatal `Error`.
3. **The panicked `catch_unwind` branch** — `main()` panicked.
4. **Normal shutdown** — written unconditionally, like the missing-natives dump.

**None of the four is reached when Java calls `System.exit(N)`.**
`native_system_exit` (`native-builtins/src/lang_system.rs` ~1215) and
`native_runtime_exit` (~1493) end in `std::process::exit(code)`. That does not
unwind: no `Drop`, no `catch_unwind`, no return to `run()`. The process is gone
before any of the four call sites can execute.

So a `--jdk-only --jdk-only-report report.json` run of an application that
detects a problem and exits — a Spring Boot failure analyzer, a CLI tool's
argument-parse error path, Cassandra NodeTool's airline NPE catch, anything
that ends in `System.exit(1)` — produces **no report at all**. The operator sees
a bare exit code and is told nothing about the violations that led to it, even
though the VM recorded every one of them.

## Why it is worse than an ordinary gap

The gap is correlated with failure. A strict run that boots cleanly, does its
work and returns from `main` writes a complete census that mostly says
"everything was fine". A strict run that hits a refusal, surfaces it as a Java
exception, and gets `System.exit(1)`d by the application's own error handler
writes nothing. The census is systematically missing for the population it
exists to describe.

It also interacts with the other observability gaps. `--trace-jdk-only` drains
its append-only logs at `Vm::new` and at shutdown (see
[the observability record](jdk-only-observability-surface-FIXED-20260804.md)),
and the shutdown drain is inside `finish_jdk_only` — so a `System.exit` run also
loses every class-origin violation recorded after boot, not just the files.

## A pre-exit hook already exists — and cannot safely do this job

This is not a case of "there is nowhere to put the call". `native_system_exit`
and `native_runtime_exit` both call `invoke_pre_exit_hook(code)` immediately
before `std::process::exit`, and `vm-cli`'s `run()` installs a closure there at
the top of the function (`lang_system::set_pre_exit_hook(...)`). The hook is
already doing real work on that path: `cleanup_staged_archive_copies()`, the
JFR `dumponexit` recording dump, `CRATONVM_DBG_EXIT`'s dispatch-trace ring, and
`maybe_dump_jit_method_stats()`. It even resolves the live VM the same way a
census would have to, via `cratonvm_vm::native::jni::process_vm()`.

The obstacle is not access. It is **locking**.

The hook runs on whichever Java thread called `System.exit`, at an arbitrary
point in that thread's execution, with whatever locks that thread already holds.
`write_jdk_only_dumps` calls three writers that all take the class-manager lock:
`dump_class_origins_json` and `dump_jdk_only_report_json` need it for the origin
census and the violation list, and `dump_native_census_json` takes it for the
whole `real_declaring_method` loop. That lock is an `OrderedPlRwLock`
(`vm/src/vm/realms/class_realm.rs`: `pub class_manager: OrderedPlRwLock<ClassManager>`),
i.e. it participates in the workspace's lock-order enforcement
(`types/src/lock_order.rs`). Acquiring it from an arbitrary Java thread that may
already hold a lower-ranked lock is both a deadlock risk and a lock-order
violation — and a deadlock *in the exit path* hangs the process instead of
losing a file, which is strictly worse than the bug being fixed.

That is why the re-land wired the census into the four unwinding paths and left
the fifth alone. The reasoning is right; the hole is still a hole.

## What specifically must change

There is no free option here. In rough order of preference:

1. **Snapshot early, serialise late.** Keep a cheap, lock-free-to-read snapshot
   of what the artefacts need — the violation vectors are already append-only,
   and the census rows are derivable from the registry, which is `&`-accessible.
   The class-origin dump is the hard one, because it walks the class store. A
   pre-exit path that writes the *report* and the *native census* but skips the
   class-origin dump is strictly better than writing nothing, and should say so
   in the file (a `"partial": true` key, not a silently short census).
2. **Try-lock with a bounded timeout.** `write_jdk_only_dumps` from the hook,
   but with `try_read` and a hard bail. A census that is sometimes missing is
   what we have today; a census that is *usually* present and never hangs is a
   clear improvement. This must be try-lock, never a blocking acquire.
3. **Intercept above the native.** `System.exit` runs Java shutdown hooks in a
   real JVM; if CratonVM ever routes exit through a proper shutdown sequence on
   a known thread with known locks, the census belongs there and the whole
   problem dissolves. `SHUTDOWN_HOOKS` already exists in `lang_system.rs`, so
   this may be less far away than it looks.

Whatever is chosen, `write_jdk_only_dumps`'s existing `WRITTEN` `AtomicBool`
already makes "first caller wins" safe, so a fifth caller cannot double-write or
race the shutdown one.

## How to verify a fix

* A `--jdk-only --jdk-only-report r.json --dump-class-origins o.json` run of a
  program whose `main` ends in `System.exit(1)` must leave both files behind,
  and the report's violation list must match what the same program produces when
  its `main` returns normally instead.
* `Runtime.exit(int)` must behave identically to `System.exit(int)` — they are
  separate natives (`native_runtime_exit` / `native_system_exit`) and both call
  the hook, so both must be covered by the same test.
* **Deadlock guard, and this is the one that matters:** a program that calls
  `System.exit` from a thread holding a class-manager read lock (any thread
  inside a class-loading callback) must exit, not hang. Run it under the
  standard watchdog; a timeout here is the failure mode, not a flake.
* `Compatible` mode must be unchanged: with no `--jdk-only*` flag,
  `write_jdk_only_dumps` returns immediately on its three-`is_none()` guard, so
  a correct fix adds exactly zero work to a default run. Verify that, do not
  assume it.

## Blast radius if done wrong

* **A blocking lock acquire in the exit path turns a lost file into a hung
  process.** This is the whole reason the gap exists; reintroducing it as the
  "fix" is the single worst outcome available.
* Writing a *partial* census without labelling it partial is worse than writing
  none: a zero-row class-origin dump reads as "this run fabricated nothing",
  which is the exact false-green that contract §11's acceptance criteria are
  supposed to be immune to.
* Adding the census to the pre-exit hook without respecting `WRITTEN` would let
  a `System.exit` racing a normal shutdown produce two interleaved writes to the
  same path.

## Related

* [The observability surface](jdk-only-observability-surface-FIXED-20260804.md)
  — the instruments this record is about delivering. In particular,
  `--trace-jdk-only`'s shutdown drain is lost on the same path.
