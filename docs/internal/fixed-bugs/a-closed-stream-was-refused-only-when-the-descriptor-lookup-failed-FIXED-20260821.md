# A closed stream was refused only when the descriptor lookup had already failed

**Status: FIXED 2026-08-21**, branch `fix/io-closed-stream-precedence-20260821`.
Found 2026-08-21 while merging dev into an unrelated netty TLS branch: three
`native-io` unit tests had been red on dev and the diagnosis pointed here.

## What it was

Ten bodies — five `fis_*`, five `fos_*` — asked whether the stream was closed
only from INSIDE the descriptor lookup's `None` arm:

```rust
let fd = match fis_get_fd(ctx, this) {
    Some(fd) => fd,
    // ... "Only a positively-marked close is refused"
    None if fis_is_closed(ctx, this) => return Err(io_stream_closed()),
    None => return Ok(Some(Value::Int(-1))),
};
```

**That is not what it did**, and the comment saying otherwise is what made it
survive three separate sweeps that touched these lines. `io_stream_is_closed`
has TWO independent grounds:

* the `FileDescriptor`'s `(fd < 0 && handle < 0)`, and
* a negative marker in instance slot 0;

while `f{i,o}s_get_fd` has FIVE places it will find a descriptor: the
`FileDescriptor` object's `fd`, then its `handle`, then a slot-0 reference's
`fd`/`handle`, then a legacy slot-0 `Int`, then `System.in`'s slot-1 `fd + 1`
encoding, then the process-stdin identity.

So a receiver marked closed on one ground while a descriptor was still
reachable by another took the `Some(fd)` arm and performed the I/O on a stream
it had just agreed was closed. The concrete shape the tests hit:
`native_fis_close` correctly writes the marker onto the `FileDescriptor`
(writing `Int(-1)` into slot 0 would coerce the reference field to `null` — see
the FIS-FIX note), leaving slot 0 at the untagged `Int(0)` an unwritten
reference slot reads back as, which the legacy arm accepts as **descriptor 0**.

That is the fabricated-success family the call sites themselves cite
(`G4-1-the-io-and-nio-fabricated-success-sweep`): a closed stream answering EOF
makes `while ((n = in.read()) != -1)` exit cleanly and the copy come out
silently truncated.

## The oracle, taken whole rather than row by row

`probes/ClosedStreamOracle.java`, Eclipse Adoptium 25.0.3+9-LTS, on already
`close()`d streams. The carve-outs are measured beside the rows they are
exceptions to, because they are what a hoisted check is most likely to break:

```text
fis.read()            THREW IOException: Stream Closed   fos.write(int)        THREW
fis.read(byte[4])     THREW                              fos.write(byte[4])    THREW
fis.read(b4, 0, 4)    THREW                              fos.write(b4, 0, 4)   THREW
fis.available()       THREW                              fos.flush()           void
fis.skip(0)           THREW                              fos.close() [double]  void
fis.skip(-5)          THREW                              fis.close() [double]  void
fis.skip(1)           THREW

fis.read(byte[0])     0        <- ZERO-LENGTH outranks the closed state,
fis.read(b4, 0, 0)    0           on BOTH sides
fos.write(byte[0])    void
fos.write(b4, 0, 0)   void

fis.read(b4, -1, 1)   THREW IndexOutOfBoundsException    <- BOUNDS outrank it
fis.read(b4, 0, 99)   THREW IndexOutOfBoundsException
fis.read(null, 0, 1)  THREW NullPointerException         <- and so does NULL
fos.write(null, 0, 1) THREW NullPointerException
```

**The precedence, in one line:** null, then bounds, then the zero-length
carve-out, then the closed check, then the descriptor lookup.

## The fix

`f{i,o}s_refuse_if_closed`, a `?`-returning helper called in sequence at each of
the ten sites — deliberately NOT folded back into `f{i,o}s_get_fd`, because the
lookup must not move above the three checks that outrank it. Each site's
existing comment travelled with the check rather than being left above the
benign arm it does not describe.

`close()` and `flush()` do not call it: a double close is `void` on both
streams, and so is `flush()` on a closed `FileOutputStream`.

## The tests, and the two that exist to stop the fix going too far

* `a_closed_stream_refuses_even_while_its_descriptor_is_reachable` — all ten
  operations. Its fixture **asserts** that slot 0 is the untagged default AND
  that `fis_get_fd` still answers `Some`, so the test cannot pass by the defect
  having become unreproducible.
* `the_carve_outs_survive_the_closed_check` — the zero-length pair on both
  sides, the double close, and flush-after-close.
* `an_open_stream_is_not_refused_by_the_hoisted_check` — the check now runs on
  every call rather than only when the lookup failed, so the thing to prove is
  that it does not begin refusing live streams.

**Non-vacuity checked** by neutering both helpers to `Ok(())`: the first test
and the three pre-existing reds FAIL, while the two twins pass on both arms —
which is exactly their job.

## What this deliberately leaves open

The fixture that bites is `slot 0 == Int(0)` — an unwritten reference slot —
being accepted by `f{i,o}s_get_fd`'s LEGACY arm as descriptor 0. Descriptor 0
is **stdin**. So the same arm says that a `FileInputStream` which reached these
natives without its `fd` field ever being written reads from the process's
standard input rather than failing.

The closed half of that is fixed here: such a stream, once marked closed, is now
refused before the lookup runs. The OPEN half is not, and it is a different
defect with a different fix — tightening the legacy arm to `v > 0`.

That looks safe on inspection and is **not** done here, for two reasons worth
stating rather than leaving as an omission:

* it has no failing test and no reproduction, only an inspection argument, and
  this branch's whole method was to measure first;
* the two other stdin routes (`System.in`'s slot-1 `fd + 1` encoding, where
  "0 means unset" is called out explicitly, and the `get_system_stream("in")`
  identity check) exist precisely BECAUSE 0 is ambiguous in a slot — which is
  evidence the tightening is right, and also evidence that the legacy arm is
  load-bearing for some caller that has not been identified.

Anyone taking it should start by asking which receivers actually reach
`f{i,o}s_get_fd` with slot 0 an `Int` at all, since the real-JDK layout puts a
`FileDescriptor` reference there.

## Measurements

```
cargo test -p cratonvm-native-io                  518 passed, 0 failed
regression-suite/run.sh                            63 passed, 1 failed
regression-suite/run.sh CRATONVM_ARGS=--jdk-only  104 passed, 0 failed
```

The one default-arm failure is `RImmutableFactoryTypes`, and it is **not this
change**: a control build with dev's `native-io/src` restored, everything else
identical, fails the same vector — 63/1 either way. It is a pre-existing dev
red about `Map.of(...) instanceof Collection`, with no `java.io` in it.

`--jdk-only` at 104/104 matters more than the count suggests: it is the arm the
`stub-ratchet` record names as the acceptance test, and several of these bodies
are the JDK 25 descriptors the SHIPPING modes reach.

### The broad workload: H2, 218 classes, both binaries

`java.io` is reached by everything, and this change can only turn a silent
success into a throw — so the risk it carries is a FALSE refusal on a live
stream, which no unit test can see. H2 is the file-I/O-heaviest suite in the
tree. One fork per class, `--category all --count 0`, both binaries, and the
binaries are ONE COMMIT apart: `fix` is the branch head, `base` is its own
merge-base, both built in the same worktree.

```
base  218 rows  176 PASS
fix   218 rows  178 PASS

diff (class + status):
  org.h2.test.db.TestTempTables                 HANG -> PASS
  org.h2.test.jdbc.TestConcurrentConnectionUsage  FAIL -> PASS
```

**No regression. And neither difference survives as an attributable
improvement** — both were re-run ABBA, five rounds:

| class | base | fix |
|---|---|---|
| `TestTempTables` | **9 / 9 PASS** | **10 / 10 PASS** |
| `TestConcurrentConnectionUsage` | 8 PASS, **1 FAIL** | 10 / 10 PASS |

* `TestTempTables` passes on BOTH arms every time, at 229–292 s on base against
  a **300 s cap**. The sweep's HANG was the cap. It is named in
  `fixed-suite-bugs/h2-suite-bugs/hangs-true-vs-perfcliff-RESOLVED-20260821.md`
  as a perf cliff that clears at a 5× cap, i.e. exactly the class whose wall
  sits on the boundary —
  the coin-toss shape a one-run-per-arm sweep cannot resolve.
* `TestConcurrentConnectionUsage` failed once in nine on base and never in ten
  on fix. One in nine is a flake; that split is nowhere near a result, and
  claiming the change fixed it would be inventing a win out of noise.

So the reading is the boring one, which is the right one: **218 classes, zero
regressions, and nothing else demonstrated.**

**A methodological note, because the first attempt at this was wrong.** Take one
reported `IDENTICAL` — and it was worthless. The runner's `JDK25` defaults to
`/home/victor/jdk25`, which does not exist on this host, so all 218 classes
"failed" in 7 seconds on BOTH arms and the diff was empty because both sides
were. That is the same trap recorded on the netty consolidated table the day
before: *two arms running nothing is not two arms agreeing.* The rerun sets
`JDK25`, was smoke-tested on three classes first, and refuses to diff at all if
either arm returns fewer than 200 rows.

The second attempt also needed correcting before it said anything: `results.tsv`
carries a per-run output path, so a raw `diff` reports every one of 218 rows as
changed. Compare class and status, not the file.



## Repro

```bash
javac -d /tmp/oracle probes/ClosedStreamOracle.java
java -cp /tmp/oracle ClosedStreamOracle          # the platform's answers

cargo test -p cratonvm-native-io --lib io_tests
JDK="<jdk25>" bash regression-suite/run.sh
JDK="<jdk25>" CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
```

## Related

- `stub-ratchet-compile-error-and-three-over-baseline-FIXED-20260821.md` — where
  the three `fis_*` reds were diagnosed and deliberately left, with the fix
  shape and the carve-out that had to survive it. Both are what this branch did.
