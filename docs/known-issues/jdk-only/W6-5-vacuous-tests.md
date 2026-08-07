# W6-5 — six tests that passed without testing anything, and what else is shaped like them

Status: **four fixed in this lane** (`RChannelInterrupt`, `RSocketChannelInterrupt`,
`vm/tests/fjp_recursive.rs`, `vm/tests/rfjp1_recursive.rs`), all verified against
HotSpot 25.0.3 and mutation-checked. **Two further findings reported, not fixed**
— they are outside this lane's files. Wave 6, lane W6-5.

Predecessors: `L10-rjdkprocess-vector-overassertion.md` (the opposite failure —
an over-asserting vector), `W3-4-forkjointask-status-flags-and-the-eager-default.md`
(which is where the vacuous FJP tests were first written down),
`vm/tests/probe_compile_guard.rs` (the existing source-level guard for one of
these two shapes), `vm/tests/common/mod.rs`.

---

## 0. The two shapes, both greppable

| shape | how it reads | how it fails |
| --- | --- | --- |
| **A — input missing &rarr; silent pass** | `if !src.exists() { return; }` inside a `#[test]`, or an `eprintln!("...skipping")` followed by `return` | cargo prints `ok`. The only tell is the **duration**: a test that must boot a VM finishing in `0.00 s`. |
| **B — assert on presence, not behaviour** | `check(field != null, ...)` where the bug being guarded is a *behaviour* | the test is green before AND after the fix for the defect it names. |

Both are self-concealing: nothing in a green run distinguishes them from
coverage. Every one of the six was found **by accident**, by someone reading the
source for an unrelated reason.

---

## 1. Shape B — the two channel-interrupt vectors

### 1.1 What they asserted

`regression-suite/src/RSocketChannelInterrupt.java` asserted only that
`AbstractInterruptibleChannel.interruptor` and `closeLock` were non-null on three
channels. Its own header said why, and said what that costs:

> "CratonVM services these channel operations natively — `begin()` may never run,
> so a behavioural probe cannot tell a seeded channel from an unseeded one …
> **a blocked read is reported as OK**."

That last clause is the whole defect. The class never parked a reader and never
closed or interrupted one from another thread, so it could not observe a blocked
reader at all — and therefore reported one as fine.

`regression-suite/src/RChannelInterrupt.java` was less bad than the sweep brief
suggested: it *did* assert a real behaviour (`FileChannel.write` on an
already-interrupted thread must raise `ClosedByInterruptException`, channel
closed, interrupt status preserved). What it never did was **park** anything.
An already-set interrupt flag is caught by `begin()` before the operation ever
blocks, so that vector exercises the entry check and nothing about wake-up.

### 1.2 What they missed

A wave-2 lane fixed a real defect in which a thread blocked in
`Socket.getInputStream().read()` / `SocketChannel.read()` was never woken by
another thread's `close()` — the reader stayed parked. Both vectors were green
**before and after** that fix.

`Socket.getInputStream().read()` and `SocketChannel.read()` are **separate
implementations behind separate close registries** (`native-io/src/net.rs` vs
`native-io/src/socket_channel.rs`), so covering one says nothing about the other.
Both are now covered, one per vector.

### 1.3 What they assert now

`RChannelInterrupt` (9 checks, was 5) — keeps the FileChannel half verbatim and
adds the `java.net` stream half:

* a thread parked in `Socket.getInputStream().read()` is woken by another
  thread's `Socket.close()`, within a bounded join, with a `SocketException`;
* the reader must have spent at least 250 ms inside `read()` — otherwise it never
  parked and the scenario proved nothing.

`RSocketChannelInterrupt` (28 checks, was 10) — keeps all ten field checks and
adds four behavioural scenarios:

| scenario | specified outcome |
| --- | --- |
| blocking `SocketChannel.read()` + `close()` from another thread | `AsynchronousCloseException`, channel closed |
| blocking `SocketChannel.read()` + `Thread.interrupt()` | `ClosedByInterruptException`, channel closed, interrupt status preserved |
| `read()` on a channel closed **before** the call | plain `ClosedChannelException` |
| blocking `ServerSocketChannel.accept()` + `close()` | `AsynchronousCloseException` |

The third row is a **negative control** and is the load-bearing one. Without it,
a VM that threw `AsynchronousCloseException` from every read on a closed channel
would satisfy rows 1 and 4 without ever waking anything — the new assertions
would be as vacuous as the old ones, just harder to spot.
`ClosedByInterruptException extends AsynchronousCloseException extends
ClosedChannelException`, so classification goes through a single `nameOf()`
ordered most-derived-first; a `catch` chain in the wrong order reports every one
of them as the base class and silently stops discriminating.

### 1.4 Why this is not the `RJdkProcess` mistake again

`L10-rjdkprocess-vector-overassertion.md` records a vector that over-asserted:
three process-table snapshots against a child chosen to die immediately, i.e.
three coin flips. Three things keep this lane away from that:

* **Every outcome was measured on the oracle before being asserted.** A scratch
  reconnaissance probe enumerated what HotSpot 25.0.3 actually throws in each of
  six shapes; only the observed outcomes were written into an assertion.
* **The races are ours to control, and they run one way.** The closer sleeps
  400 ms *after* the reader signals it is entering the call. A slower machine
  makes the observed park time longer, never shorter. The 250 ms proof threshold
  is a floor the test creates itself, not a wall-clock guess about the host
  (cf. "fixed wall-clock test bounds are latent CI flakes" — the flaky shape is
  an *upper* bound on something the host controls; this is a *lower* bound on
  something the test controls).
* **Every wait is bounded.** A defect produces a named `AssertionError` in ~16 s,
  never an `rc=124` at the suite's 120 s per-class timeout — which would be
  indistinguishable from a VM hang. Helper threads are daemons, so a reader that
  genuinely never wakes cannot keep the VM alive past the failure.

Verified 5/5 clean on HotSpot at the default `--release`, and again at
`--release 17` (the suite's `RELEASES` mode compiles at 17/21/25).

Mutation-checked: with the `close()` call suppressed — simulating exactly the
defect — both vectors fail with the named assertion, `rc=1`, in 16 s and 15 s.

---

## 2. Shape A — the two Rust ForkJoin tests

### 2.1 What was wrong

`vm/tests/fjp_recursive.rs` and `vm/tests/rfjp1_recursive.rs` both referenced
`apps/fjp_probe/FjpProbe.java`, **which did not exist in the tree**.
`ensure_probe_compiled` hit `if !src.exists() { return false; }`;
`fjp_probe_classes` returned `None`. Both printed "skipping" and passed in
0.00 s. Worse, `rfjp1_recursive` never compiled the fixture at all — it only
checked for leftover `.class` files, so it could pass only by accident even when
the source *was* present.

Consequence, recorded independently in
`W3-4-forkjointask-status-flags-and-the-eager-default.md`: the justification for
lazy ForkJoin fork ("eager fork overflowed the host stack on deeply-recursive
`RecursiveTask` probes") **had no live guard anywhere in this repo**. That is
part of why flipping the default on 2026-08-07 needed a bespoke A/B.

### 2.2 Choice: restore the probe (option i)

A probe *can* be written safely, so option (ii) — fail loudly on absence — was
not the whole answer. It is, however, half of it: the fixture is restored **and**
its absence is now a panic.

`apps/fjp_probe/FjpProbe.java` — divide-and-conquer sum of
`long[0..1_000_000)`, threshold 1000. These are the **calibrated** constants the
historical findings were recorded against: 1000 leaves, recursion depth exactly
10. It pins three separate things:

* **RFJP.1 (JIT).** `right.compute()` and `left.join()` both unbox a `Long`
  returned from a recursive call — the `Long.valueOf` boxing-miscompile site
  that made the probe print `sum = 0`.
* **WP4.3 (interpreter).** A `long[]`, not an `int[]`, because `lastore` once
  lost the value tag and zeroed the array.
* **Lazy fork (threading).** `left.fork()` is a real `fork()`. Under an
  always-eager mode this is where the host-stack recursion begins. The shipped
  default is `FjtForkMode::CountedCompleterEager`, which a `RecursiveTask`
  receiver does **not** match, so the default takes the lazy branch here and this
  probe is unaffected by that flip — which is exactly what makes it the right
  shape for a guard. The eager arm is now one command:
  `CRATONVM_FJP_EAGER_FORK=all cratonvm --java-home <jdk> -c <classes> FjpProbe`.

The probe **self-checks its own recursion depth** and prints `depth = 10`; both
harnesses assert that line. Without it, a mistyped threshold would turn the
probe into a single non-recursive leaf that still prints the right sum — the
same vacuity, one level down.

On failure the probe prints `FAIL ...` and exits 1, so a harness asserting
`status.success()` is meaningful.

### 2.3 What changed in the harnesses

* **A missing fixture is a PANIC, not a skip.** A checked-in fixture that has
  vanished is a broken repository, not an absent toolchain. Evaluated *first*, so
  it is reported even when stale class files are lying around.
* `rfjp1_recursive` now compiles the fixture itself instead of hoping someone
  left class files behind.
* Class output moved to `target/fjp-probe-classes/<test-binary>/`, one directory
  per harness, so the two cannot race each other's `javac`, and a source edit
  triggers a recompile (mtime check) instead of reusing a stale class.
* `javac` is resolved from `CRATONVM_TEST_JDK` / `JAVA_HOME` before falling back
  to `PATH`, so the probe is compiled by the toolchain the run uses.
* Both harnesses honour `CRATONVM_REQUIRE_E2E`: a javac that cannot be launched,
  an unsupported `--release`, a missing binary and a missing JDK all become
  panics under it. Only those four still skip.
* `rfjp1_recursive` gained a bounded 120 s wait (it used a plain `.output()`,
  which a starving pool or a stack overflow could wedge forever).
* Both keep the `"failed to compile"` marker `probe_compile_guard.rs` requires.
  `rfjp1_recursive` now launches `javac`, so it newly falls under that guard.

### 2.4 The durability problem — `apps/` is gitignored

`.gitignore:12` is `apps/`. **That is the mechanism by which this fixture
disappeared**, and putting it back in the same place re-arms it: the file needs
`git add -f`, and once added, ordinary `git add -A` will still never notice a
change to it.

The lane's file allocation was `apps/fjp_probe/**`, so the fixture is there. Both
harnesses' `probe_source()` also accepts **`probes/FjpProbe.java`** — `probes/`
is tracked, with only `probes/*.class` ignored — so moving the file there needs
no code change and is the durable home. **Recommended.**

> **UPDATED 2026-08-07.** `probes/FjpProbe.java` still does not exist; the
> fixture is still at `apps/fjp_probe/FjpProbe.java`, which **is** tracked
> (force-added), so this section's premise — "it vanishes on the next clone" —
> no longer applies to FJP. It does still apply, unfixed, to
> `probes/BdProbe.java`, `regression-suite/src/RJdkPhaser.java` and
> `regression-suite/src/RJdkFieldModule.java`, all three of which `git status`
> reports as `??`. **A tracked *directory* is not a tracked *file*** — the
> recommendation above is only half the work, and the missing half is the
> `git add -f`.

---

## 3. Other tests shaped the same way — inventory

### 3.1 CONFIRMED, high value: two documented "permanent gates" that no longer run

`regression-suite/src/RPriorityQueueGc.java` and
`regression-suite/src/RTreeRangeGc.java` are each described in a FIXED-bug
document as *the permanent regression gate* for a heap-corruption defect:

Both are internal records, cited by their path relative to the internal tree's
own root (they are not published, so a `docs/`-rooted path would be a link no
public reader can follow):

* `fixed-suite-bugs/h2-suite-bugs/bug-h2-priorityblockingqueue-stale-objectref-classcastexception-FIXED.md:119`
* `fixed-suite-bugs/treemap-treeset-range-snapshot-stale-objectref-FIXED.md:102`

The fact this section rests on does not depend on reaching either page: **both
vectors pass on a broken VM without their extra argument**, for the two reasons
spelled out under point 2 below (on the default heap no collection happens
during the walk; with a live JIT frame the young generation falls back to a
non-moving sweep under which a stale reference still resolves).

Two things have since broken, and they compound:

1. **Neither class is in `CORE_CLASSES`** in `regression-suite/run.sh`, so a
   plain `bash regression-suite/run.sh` never runs either one. They are the only
   two vectors in `src/` that are in neither `CORE_CLASSES` nor
   `JDKONLY_CLASSES` other than `RConcurrent`, whose exclusion *is* documented
   in run.sh.
2. ~~**The `cv_extra_args` hook they depend on is gone from `run.sh`.**~~
   **UPDATED 2026-08-07: partly fixed, and the residue is worth knowing.**
   `run.sh` now carries `cv_extra_args` / `class_cv_args` (CratonVM-only
   arguments, deliberately withheld from the HotSpot oracle — *"the oracle has
   to stay the plain reference run"*) and an explicit `UNREGISTERED_CLASSES`
   list naming `RConcurrent`, `RPriorityQueueGc` and `RTreeRangeGc`, each with
   its reason — and `class_cv_args` supplies exactly the arguments this section
   asked for: `RPriorityQueueGc` → `--nojit --Xmx 64m`, `RTreeRangeGc` →
   `--Xmx 64m` (deliberately without `--nojit`, so the compiling configuration
   stays under test). `run.sh`'s own comment now carries this section's warning
   verbatim: *"With `--nojit` alone the vector PASSES ON A BROKEN VM."*
   **Point 1 still stands**, and it is the whole residual: both classes remain
   in `UNREGISTERED_CLASSES`, so neither runs at all until a lane that can build
   and run the VM verifies them — which `run.sh` also says in place.
   Both
   documents and both vectors' own headers say the suite runs them with
   `--nojit --Xmx 64m` and `--Xmx 64m` respectively. `run.sh` today has only
   `class_args()`, which handles `RJdkModule` and nothing else — and `class_args`
   output is passed to **HotSpot as well**, so it cannot carry CratonVM-only
   spellings like `--nojit`.

The compounding is what matters. Both documents state that **without those args
the vector passes on a broken VM**:

> "with a live JIT frame on the stack the young generation falls back to a
> non-moving sweep … under which a stale reference still resolves and the defect
> hides entirely"

> "On the default heap no collection happens during the walk at all and the class
> passes on a broken VM."

So the verification command both documents give —
`ONLY=RPriorityQueueGc bash regression-suite/run.sh` — is **itself vacuous
today**. Re-adding the classes to `CORE_CLASSES` without restoring the hook would
produce two more green-forever vectors, which is worse than the current state
because it would look like coverage.

`run.sh` is not this lane's file. Proposed patch, text anchors, in
[§4](#4-proposed-patch-to-runsh-not-this-lanes-file).

### 3.2 CONFIRMED, largest by count: `apps/` is gitignored, and ELEVEN more probe fixtures are gone

The FJP fixture is not a one-off. Every path literal of the form
`.join("apps").join(<probe>)` in `vm/tests/*.rs` was resolved against the
filesystem. Twelve were referenced; **eleven do not exist** (the twelfth is the
one this lane restored):

| missing fixture dir | referencing test file | `#[test]` fns in file |
| --- | --- | --- |
| `apps/atomic_probe` | `vm/tests/wave4_a_atomic.rs` | 5 |
| `apps/bytebuddy_probe` | `vm/tests/wave2_bytebuddy.rs` | 4 |
| `apps/chm_basic` | `vm/tests/wave2_chm.rs` | 1 |
| `apps/console_probe` | `vm/tests/wave3_console_module.rs` | 2 |
| `apps/h2` | `vm/tests/wave2_h2_connection.rs` | 1 |
| `apps/lm_subclass` | `vm/tests/block_2b_logmanager_factory.rs` | 2 |
| `apps/methodhandles_probe` | `vm/tests/wave2_c_methodhandles.rs` | 1 |
| `apps/proxy_probe` | `vm/tests/wp2_5_proxy.rs` | 2 of 21 |
| `apps/scanner_probe` | `vm/tests/wave3_scanner.rs` | (self-generates its source — likely OK) |
| `apps/selector_probe` | `vm/tests/wave3_c_selector.rs` | 1 |
| `apps/xml_probe` | `vm/tests/wave1_d_xml_stax.rs` | (self-generates its source — likely OK) |

The two that write their own probe source are probably fine. The rest take the
identical shape the FJP tests took:

```rust
let (stdout, stderr, rc) = match run_mh_probe(Duration::from_secs(120)) {
    Some(o) => o,
    None => {
        eprintln!("[wave2-c] skipping (binary or probe class unavailable)");
        return;
    }
};
```

Verified in detail, `vm/tests/wp2_5_proxy.rs:145` —
`proxy_probe_compiled_class_files_exist` is unconditionally vacuous, with *two*
nested escape hatches before any assertion:

```rust
if !probe_dir.exists() { return; }            // Fixture not staged.
if !main_cls.exists() { return; }             // "a builder pre-step compiles it"
```

Neither directory nor class has existed in this tree. The test cannot fail.

**Root cause is one line**: `.gitignore:12` is `apps/`. Probe fixtures placed
there are invisible to `git add -A`, survive only in whoever's working tree
created them, and vanish on the next clone — taking their tests' ability to fail
with them, silently. That is a repository-layout defect, not eleven independent
test bugs, and re-creating each fixture under `apps/` would re-arm it. The
durable home for a tracked probe fixture is `probes/`, which is tracked with only
`probes/*.class` ignored.

None of these skips route through `common::require_*`, so
`CRATONVM_REQUIRE_E2E=1` does **not** catch them either — see §3.3.

### 3.3 The structural gap behind shape A

`vm/tests/common/mod.rs` promotes a missing **binary** or a missing **JDK** to a
panic under `CRATONVM_REQUIRE_E2E=1`. It does **not** cover a missing **fixture**
— which is the exact hole the two FJP tests fell through. `probe_compile_guard.rs`
covers the adjacent case (javac ran and rejected the source) but not this one.

A third `common::require_fixture(Option<PathBuf>)` would close it, and a
source-level guard in the shape of `probe_compile_guard.rs` — "every test that
gates on a fixture path must panic, not return, when it is absent" — would keep
it closed. ~~Not done here~~: `vm/tests/common/mod.rs` and `probe_compile_guard.rs`
are not this lane's files, and the fix in §2.3 is local to the two tests.

> **UPDATED 2026-08-07 — `common::require_fixture` now exists in
> `vm/tests/common/mod.rs`, and it changes nothing about CI.** The helper only
> panics when `CRATONVM_REQUIRE_E2E` is set, and **no file under
> `.github/workflows/` sets it** — CI runs plain `cargo test --workspace`.
> `vm/tests/common/mod.rs` states the opposite in two comments (*"CI sets it to
> assert that a green run was a real one"*); that claim is false and cannot be
> corrected from `docs/`. Until a workflow exports it, every
> `require_binary` / `require_jdk` / `require_fixture` skip is still a silent
> green in CI, and §3.3's structural gap is open for the reason it was filed,
> not for the reason it names.
>
> The live instance: `vm/tests/rbigdec1_arithmetic.rs` claims *"the gate is now
> live in CI"*. It is not — the test opens with `None => return`, the fixture
> `probes/BdProbe.java` is **untracked** (`?? probes/BdProbe.java`), and the
> alternative path `apps/bigdecimal_probe/BdProbe.java` does not exist. The
> adjacent claim in the same file that *"`probes/` IS tracked"* confuses the
> tracked directory with the untracked file — which is exactly the confusion
> §2.3 above warns about, arriving from the other side.

### 3.4 Java vectors — swept, clean

All 55 `regression-suite/src/*.java` were checked for both shapes:

* **Presence-only assertions**: no vector's assertions are majority
  `!= null`-shaped. `RSocketChannelInterrupt` was the worst at 3/10 and is now
  3/28.
* **Tolerate-and-continue** (`catch (Throwable t) {}` around the subject): one
  hit, `RJdkNio.java:74`, and it is explicitly cleanup, not an assertion.
* **Conditional checks** are partly self-policing here: every vector prints
  `CK <Class> checks=N`, and `run.sh` diffs that line against HotSpot, so a check
  that runs on one VM and not the other shows up as an output difference. Note
  this only works because the count is printed — a vector that stopped printing
  it would lose the property silently.

---

## 4. Proposed patch to `run.sh` (not this lane's file)

**Unverified — I could not run the suite.** Confirm with
`ONLY="RPriorityQueueGc RTreeRangeGc" bash regression-suite/run.sh` before
landing; both documents claim PASS on the fixed binary, so red here means the
underlying fixes regressed, which is the point of a gate.

**(a) schedule the two vectors.** In the `CORE_CLASSES=` assignment, replace

```
RFieldSiteCache RMethodSiteCache RDataInputFastPull"
```

with

```
RFieldSiteCache RMethodSiteCache RDataInputFastPull RPriorityQueueGc RTreeRangeGc"
```

**(b) restore the CratonVM-only per-class args.** After the closing brace of
`class_args()`, i.e. replace

```sh
    *) : ;;
  esac
}

# Can $1 (a javac) actually target `--release $2`? Probed with a throwaway
```

with

```sh
    *) : ;;
  esac
}

# Launcher arguments a vector needs on the CRATONVM side ONLY. Kept separate
# from class_args() because these are CratonVM spellings: HotSpot is the oracle
# and must stay an unmodified reference run, so it never receives them.
#
# Both of these gates are INERT without their argument. See the internal records
# fixed-suite-bugs/h2-suite-bugs/bug-h2-priorityblockingqueue-stale-objectref-classcastexception-FIXED.md
# and fixed-suite-bugs/treemap-treeset-range-snapshot-stale-objectref-FIXED.md:
# on the default heap no collection happens during the walk, and with a live JIT
# frame on the stack the young generation falls back to a non-moving sweep under
# which a stale reference still resolves. Either way the vector passes on a
# broken VM. Do not drop these again.
cv_extra_args() {
  case "$1" in
    RPriorityQueueGc) printf '%s' "--nojit --Xmx 64m" ;;
    RTreeRangeGc)     printf '%s' "--Xmx 64m" ;;
    *) : ;;
  esac
}

# Can $1 (a javac) actually target `--release $2`? Probed with a throwaway
```

**(c) pass them.** Replace

```sh
    extra=$(class_args "$c")
```

with

```sh
    extra=$(class_args "$c")
    cvextra=$(cv_extra_args "$c")
```

and replace

```sh
    cvout=$(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV" --java-home "$JDK" ${CRATONVM_ARGS:-} $extra -cp "$BUILD" "$c" 2>&1)
```

with

```sh
    cvout=$(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV" --java-home "$JDK" ${CRATONVM_ARGS:-} $extra $cvextra -cp "$BUILD" "$c" 2>&1)
```

`$cvextra` is unquoted on purpose — it is a flag word list, matching how `$extra`
and `$CRATONVM_ARGS` are already consumed on that line.

---

## 5. What would falsify this lane

The channel work rests on one claim: **that the four new scenarios are
deterministic on the oracle and not coin flips.** The falsifying observation is a
single HotSpot run of either vector reporting an outcome other than the one
asserted — most plausibly a plain `ClosedChannelException` where
`AsynchronousCloseException` is demanded, which would mean the closing thread won
the race to close before the reader entered `read()`, i.e. that the 400 ms margin
is not enough on some host. 5/5 clean here, and the margin is ~4 orders of
magnitude above the work it covers, but it is a margin, not a proof.

The FJP work rests on a claim I could not test: **that `FjpProbe` passes under
CratonVM.** It is verified only on HotSpot. If CratonVM fails it, that is a real
finding, not a bad test — but it will surface as a newly-red test rather than a
newly-green one, and the orchestrator should expect that possibility.
