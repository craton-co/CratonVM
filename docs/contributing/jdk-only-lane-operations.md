# Operating a `--jdk-only` lane

| | |
|---|---|
| **Status** | Permanent. This is process, not a work item; it does not retire when a campaign does. |
| **Normative source** | [`../feature-designs/jdk-only-mode.md`](../feature-designs/jdk-only-mode.md) |
| **Companions** | [`../jdk-only-migration.md`](../jdk-only-migration.md) · [`../jdk-only-native-review.md`](../jdk-only-native-review.md) · [`../known-issues/jdk-only/INDEX.md`](../known-issues/jdk-only/INDEX.md) · [`jdk-only-lanes/`](../known-issues/jdk-only-lanes/lane-0-integration-and-gates.md) |

**Why this page exists separately from any handoff.** The eight-lane campaign of
2026-08-28/29 ran from `HANDOFF-20260828-SCOPE.md`, whose §3 and §5 were the
operating rules every lane worked from. Those rules outlived the campaign, and
leaving them in a handoff meant the handoff could not retire without taking them
out of circulation. Rehoming them here was the last of that page's three stated
retirement blockers; **it retired on 2026-09-01** to
`HANDOFF-20260828-SCOPE.md` in the internal tree, as a campaign record.
Everything below is durable.

Each rule here was learned by getting it wrong once. The cost is recorded with
the rule, because that is the part that makes it stick.

**Running more than one lane at a time.** The rules below are per-lane and stay
the same however many lanes run. What a parallel campaign needs *in addition* --
who owns which class prefixes, who may edit the shared gate cells, the build
queue, and the merge order -- is in
[`jdk-only-lanes/lane-0-integration-and-gates.md`](../known-issues/jdk-only-lanes/lane-0-integration-and-gates.md),
which is the ownership authority for the nine-lane split of 2026-09-10. Read it
before starting a lane; read this page for how to work inside one.

---

## 1. The method

Five families were done this way, mechanically: 993 probed rows, 48 defects, 48
fixed. Seven more followed it for another 56.

1. **Take your families' `native-won` triples from the report.** Do not pick
   methods by hand, and do not probe what the report says already loses —
   a `bytecode-won` row is a native that was registered and lost the dispatch
   anyway, so there is nothing there to retire.
2. **Write ONE differential probe per family group**, asking the CONTRACT EDGES.
3. **Run it in BOTH modes** against a real HotSpot of the same version as the
   image, as oracle.
4. **Check `owns_slot` BEFORE editing** (§3 below).
5. **Fix, rebuild, RE-RUN THE PROBE.** Never conclude from reading.
6. Record what you found AND what passed.

### Aim at edges, not the happy path

**Of the first 48 defects, not one was a wrong answer to an ordinary call, in
any of five independent families.** Nulls, bounds, refusal types, constructor
validation, naming special cases, callback boundaries. The seven later batches
came out the same way: `java.security` had 1313 of 1333 rows already correct and
all twenty differences on paths a caller only reaches by getting something wrong.

Two consequences:

* it predicts where your rows will yield;
* **a probe that exercises only the happy path will report your family clean
  when it is not.** That is how a shim's middle stays correct while its
  perimeter rots unnoticed.

Concretely: every `ByteArrayOutputStream` bounds row already passed *including*
`off + len` overflowing to a negative int — the case a check written
`off + len > b.length` gets wrong while looking right. The bounds logic was
correct; it never ran, because a null buffer returned before it.

### Probe hygiene, each learned by getting it wrong

* **stdout only** (`2>/dev/null`). `2>&1` puts VM tracing in the diff; the
  asymmetric version of that mistake invented four phantom differences.
* **Print no value the two VMs may choose independently** — identity hashes,
  addresses, thread names, timings, iteration order of a hash container, a
  resolver's answer, a virtual-thread handoff count. Ask the SHAPE instead:
  that `Object.toString()` starts with the class name and an `@`, that it is
  stable across calls, that two objects render differently.
* **Do not assert an unspecified identity.** `getSuppressed() != getSuppressed()`
  is `false` on HotSpot only because it returns a shared empty constant. One such
  row in a per-item helper became **280 differing rows** across 42 classes and
  read as a systemic VM defect.
* **Check the ROW COUNT before reading the diff.** A run that died partway
  produces a short file whose missing tail `diff` reports as ordinary `<` lines.
  That hid a whole probe section once, 128 of 160 rows in another the same day,
  and a `ReferenceQueue.remove(-1)` that waited forever — which announced itself
  only as 54 rows written against 60.
* **The runner reports LINES, not rows.** Probes print two trailer lines
  (`rows N`, `DONE <name>`), so `hs=1258` is 1256 rows. Take the count from the
  probe's own `rows N` line.
* **A crash early in a probe masks every later defect.** Fixing it usually
  uncovers more work rather than finishing it.
* **Never run a probe battery through an interactive remote channel.** A probe
  that hangs holds the channel for the length of its timeout. `nohup` into a
  log, poll the log, and cap every run with `timeout`.

---

## 2. Worktrees, branches, and the shared files

Work in **your own worktree on your own branch**. Land by pushing to `dev`.

`native-collections/src/lib.rs` is ~74 500 lines and hosts many families;
`native-builtins/src/lib.rs` is ~47 700. Several lanes will touch them at once.
This is survivable — different functions are different hunks and git merges them
— but only if you **merge `origin/dev` often**, not once at the end. On a busy
day `dev` moves every ten to twenty minutes.

**Re-check `dev` immediately before the final build, not only at the start.**
The useful check is not "am I behind" but "did anyone touch MY files":

```bash
git diff --stat HEAD...origin/dev -- <every file your branch touched>
```

Empty output means your last full verification still covers your changes and the
merge is bookkeeping — merge, re-check your invariants by grep, run the gate the
merge could plausibly break, and push. Non-empty means rebuild and re-verify.

---

## 3. Before you edit

### `owns_slot` — skipping this costs a build

A method can be registered by several files and **only one wins**.

```bash
cratonvm --java-home "$JDK" --dump-native-registry <shaped-path>/reg.json \
         -cp probes/out YourProbe
# then read, for your (class, name, descriptor):  owns_slot, invocations, registered_by
```

`owns_slot: false` means **your edit is inert**. `invocations: 0` on the winner
means your probe never reached it and the diff proves nothing either way.

A build was lost to this on `Arrays.fill` — the `phases_early.rs` registration
was fixed and `native-collections` owned the slot. The same dump showed the
*winning* `copyOf` already carried the guards being written while its losing twin
still had the broken body: **a duplicate pair can sit half-fixed indefinitely**,
and nothing fails until registration order changes.

**The tell, if you have already built:** a fix that lands for one descriptor and
not its neighbour, or for one member of a uniform family and not the rest. That
is the signature of a second registrar — or, when it is 1 of 10 rather than 1 of
3, of an edit sitting in a fallback arm below the branch point. Count what moved
as a fraction of what should have.

### The VM sometimes lies about identity

`List.of(..).getClass().getName()` reports
`java.util.ImmutableCollections$List12`. The receiver's real class is
`cratonvm/internal/Unmodifiable*` — this VM funnels every unmodifiable view
through synthetic classes and fakes `getClass()`. **A guard written against the
name your probe prints can never fire.** Nothing you can READ will reveal it.
Re-measure, never re-read.

### Field slots are not declaration order you can guess

`jdk.internal.misc.Signal`'s registrar wrote `name` to slot 0 and `number` to
slot 1, with a comment saying so. `javap -p` declares `int number` first. One
line, twelve wrong probe rows, and `toString()` printing `SIGnull` for every
signal. **Resolve by NAME**, with a slot fallback only where a mock or synthetic
receiver resolves no names — and verify the by-name write took, rather than
assuming it did.

---

## 4. Instrument traps

* a dump/report flag placed **after** the main class is silently ignored — no
  file, no warning, exit 0;
* on a Windows host the report path must be **Windows-shaped**, or the VM prints
  `os error 3`, continues, and the file never appears;
* the report is **not written when the program calls `System.exit`**;
* `tools/flag-census/render-inventory.sh` **REFUSES** to run when
  `flag-surface.txt` disagrees with `INVENTORY`. **Never chain it with
  `render-tokens.sh`** — its refusal scrolls past, one doc regenerates, the other
  silently does not, and the gate stays red for a reason nothing states. Run it
  alone and read its three lines;
* `regression-suite/run.sh` DELETES its per-vector `--jdk-only-report` files
  after printing the census. Pass `KEEP_JDK_ONLY_REPORTS=<dir>` when you need
  them — the census is their summary, not a substitute, and adjudication reads
  the files.

---

## 5. Landing protocol

```bash
# 1. gate — this is the whole set; do not shorten it
cargo test -p cratonvm-types
# Name NOTHING by hand here. `--tests` is the authority and the directory GROWS.
cargo test -p cratonvm-native-builtins --tests
cargo test -p cratonvm-native-builtins --features management --tests
cargo test -p cratonvm-native-builtins --features synthetic-jdk --tests
# plus --lib for any other crate you changed

# 2. the three arms, on a RELEASE build of the MERGED tree
CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
SUITE=all                bash regression-suite/run.sh
SUITE=core               bash regression-suite/run.sh

# 3. push, in its OWN command, keyed on the gate RESULT
git push origin HEAD:dev
```

**Do not chain the push behind the gates.** A red `doc_citation_paths` reached
`dev` because the conditional was keyed on `behind=0` instead of on the test
result.

**Why the gate list names no targets.** It used to name seven `--test` targets.
`native-builtins/tests/` holds **ten**, and the three omitted were
`lock_discipline_ratchet`, `eintr_ratchet` and `aes_gcm_kat`. The first is not a
rounding error: it holds this crate to a raw-lock-construction baseline
**because this crate re-enters the VM** — a native callback calls back into
Java, which takes the heap and class-manager locks — so a `Mutex` here with no
`LockLevel` is a deadlock the order checker cannot see. It caught exactly that
on a commit whose other nine gates were green. *(A lane following the old
seven-name list ran 7 of 10 for a whole campaign before noticing. If you copied
a gate script from a previous lane, check it against `ls native-builtins/tests/`
before you trust it.)*

**An unknown `--test` name exits 101, the same code a panicking test gives.**
Seven "failing ratchets" once turned out to be seven stale names, with cargo's
tail-truncated output listing the targets that DO exist — which reads as a list
of failures. If a sweep of unrelated guards goes red identically, suspect the
invocation before the tree, and read the FIRST line of the output.

`--features synthetic-jdk` is in the set because a `#[cfg(feature =
"synthetic-jdk")]` module that nothing compiles rots silently. Measured
2026-08-29: 4176 tests on default, 4208 under `management`, 4352 under
`synthetic-jdk` — the third arm alone compiles 176 tests nothing else does.
`--tests` subsumes `--lib` for that crate.

---

## 6. Telling your red from theirs

`dev`'s tip is frequently red, and expecting to clear something that is not
yours on most merges is the normal case rather than a bad day. What matters is
attributing it in seconds instead of bisecting it.

**The general move: find the smallest input the failing check reads, and compare
THAT with `origin/dev`.** Two worked examples:

* a `properties_sidetable` source-witness guard read
  `include_str!("properties_sidetable.rs")` and nothing else. The file was
  byte-identical to `origin/dev`'s, which is a complete proof that no merge
  caused it — one `git diff`, no build;
* a registrar-drift baseline named one stale pair from a commit already on
  `dev`, and `git log` found the commit that collapsed it.

A red you can attribute in one `git diff` is a red you do not have to bisect.

**When the check's input is the whole registry, substitute instead of
comparing.** A stub-ratchet count is a property of every registrar at once, so
no single file is identical-or-not in a way that settles it. What settles it in
one build is checking out `origin/dev`'s version of *every file your branch
touched*, running the gate, and putting yours back:

```bash
for f in <your changed files>; do cp $f /tmp/$(basename $f).mine; git checkout origin/dev -- $f; done
cargo test -p cratonvm-native-builtins --features synthetic-jdk --test stub_ratchet
# ...then copy the .mine files back
```

That is a controlled experiment rather than an argument, and it is much cheaper
than a second worktree. It was worth doing: a `synthetic-jdk` stub ratchet red
survived the substitution unchanged at 1591 against a baseline of 1582, which
proved it was **not mine** — and the two intuitions that pointed at my own
change (a new keystore SPI class, seventeen new registrations) were both wrong.
All seventeen classified as `Bridge`, not `SyntheticStub`.

**"Not mine" is where substitution stops, and I read it as "therefore `dev`'s"
twice in a report.** It was neither. That arm had **no baseline of its own**:
`BASELINE_SYNTHETIC_STUBS` branched on `feature = "management"` and nothing
else, so a third configuration — which compiles registrars the other two do not
— was being adjudicated against the first one's number, from the day the arm
entered §5. Nine rows of "drift" that nobody had introduced.

**So when a gate is red in one arm only, check whether that arm has a baseline
before you look for a culprit.** A `cfg` with two branches and three callers is
a silent mis-scoring, and it had also defeated the label meant to catch it —
the failure line printed `no-management` and named the DEFAULT constant as the
one to paste into, so re-freezing from that run would have admitted the whole
gap to the default arm silently. Fixed 2026-08-30 by
`BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK` plus three-way `cfg` on the baseline,
the label, and the constant name.

**Run the feature arms, not just the default one.** That red is reachable only
under `--features synthetic-jdk`; the default and `management` arms were green
on the same tree, because the third arm compiles registrars the other two do
not.

**Read a gate run correctly before attributing it at all.** `cargo test` stops
at the first failing test BINARY, so without `--no-fail-fast` a run showing one
failure is showing you where it stopped, not what is broken. And do not pipe it
through `head`: `cratonvm-vm` emits 40+ `test result:` lines and a cap hides the
tail. Grep for failure lines only, so an empty result is the green.

**A control is only a control for what it can run.** A test that needs the
release binary SKIPS when there is none and reports `ok`. A pristine control
worktree has no binary, so targets that fail on a built tree "pass" there — and
a clean control run is then evidence about the build, not about the code.

**A red that moves with load is not automatically a flake.** A TLS vector looked
exactly like one — green, red, green across three solo runs, and failing on the
HOTSPOT side while CratonVM passed its own checks. It was the vector's own
client loop discarding the reply it asserts on, because `unwrap()` consumes one
TLS record per call and three arrive in a single read under load. **Read the
page the vector is documented on before writing a diagnosis of it.**

The known-red list of that campaign is dated material and retired with it, in
`HANDOFF-20260828-SCOPE.md` §5. **Re-derive rather than trust a list
older than a day** — that is why it is not on this page.

---

## 7. What "done" means for a lane

Not "my probe is green". A lane is done when:

1. every `native-won` triple in its families is covered by a differential probe;
2. the probe is 0-diff in both modes, **or** each residual row has a record
   naming the measurement and why it was not fixed;
3. the record says what PASSED as well as what failed — that is what tells the
   next person where the work is not;
4. gates and the three arms are green on a merged tree, and it is pushed.

**A 0-diff probe is a precondition for retiring a shadow, never a justification
on its own.** A native may exist because the bytecode path was measured slower,
or because it was measured WRONG once and the native is the fix — `StrictMath`'s
69 rows adjudicated KEEP for exactly that reason, and the 0-diff was evidence
the family *works*. Read the registrar's history and count invocations **in the
same run as the probe** before proposing a retirement.

### Retiring a shadow: the four preconditions, and why the corpus is not one

Two drivers implement this, so the method is runnable rather than described:
[`../../scripts/jdk-only-phase2-sweep.sh`](../../scripts/jdk-only-phase2-sweep.sh)
arms one receiver at a time and produces CANDIDATES with the three verdicts, and
[`../../scripts/jdk-only-phase2-battery.sh`](../../scripts/jdk-only-phase2-battery.sh)
runs the whole probe tree in three columns and reports the signed distance from
HotSpot. Both print the vacuity checks beside every row, because that is the
half a green forgets to mention.

Phase 2 armed all 270 classes of the shadow surface one at a time and the dial
called 236 of them retire-safe. **Arming those 236 together fails 54 of 118
corpus vectors and breaks 35 of 78 probe families.** So a sweep produces
candidates, never verdicts. Each candidate earns its row against all four of:

1. **The dial was ASKED** — `enforcement_dial.reached > 0` for that scope in
   `--jdk-only-report`, not a passing vector. 146 of those 236 fail this: the
   smoke set never dispatched anything on the class, so arming it changed
   nothing and read as the best possible result.
2. **The WHOLE probe tree, armed on that class alone, gets no worse anywhere** —
   not just the family's own probe, which is the narrowest instrument in the
   building. `ConcurrentHashMap`'s own probe is 0-diff over 39 357 yields;
   `MapViewsShadowSweep` dies at row 261 of 302, because `java.util.Properties`
   delegates to an internal `ConcurrentHashMap` and every `Properties` view
   empties out. **A retirement's blast radius is its class's USERS.**
3. **The image target carries `Code`** to yield to — `image_declaring_method`,
   declared or inherited and not abstract. Without it the retirement trades a
   shadow for an `UnsatisfiedLinkError`. That field is `null` unless you pass
   **`--explain-jdk-only`** alongside `--dump-native-registry`, and a filter
   over the null reads as "no candidate anywhere has bytecode".
4. **A dispatch observed by the instrument that produced the improvement**,
   per TRIPLE, read as `invocations > 0` in that instrument's own run.

**Read the direction, not the movement.** Armed-vs-unarmed says a row moved and
cannot say which way. Use `d(hs,armed) − d(hs,base)`: negative means the
retirement moves the VM toward the oracle — `ArrayDeque` armed starts throwing
`ConcurrentModificationException`, which is what HotSpot does and what the
native never did. Only positive is a reason not to retire.

**And the dial is not the retirement.** It DECLINES at dispatch and it arms a
PREFIX; `retired_shadow.rs` re-tags at REGISTRATION and is per-TRIPLE. Treating
a dial result as the table's result cost a full build: `sun/nio/ch/FileChannelImpl`
armed took a probe from 4 diffs to 0, so the one triple on it the CORPUS had
dispatched was retired — and nothing moved, because the probe exercises
`truncate(J)` (`invocations: 2`) and never touches `open` (`invocations: 0`).
Convert a dial result into a table entry only via a registry dump **from a run
of the very probe whose improvement you are citing**, then rebuild and
re-measure on two binaries.

### What skipping the two-binary step actually cost, 2026-09-10

The paragraph above already said "rebuild and re-measure on two binaries", and
lane L0 shipped a 54-triple table without doing it. The `--jdk-only` arm came
back **97 of 132** where the four previous binaries had scored 132/0. Two
withdrawal rounds later the table is **29**. Nothing reached `dev`, and the only
instrument that caught it was the arm.

Three things that round taught, none of which is on this page yet.

**1. Read the dial's `leaked` counter before citing a dial arm at all.** The
armed run prints it:

```text
[DIAL_DOOR_CENSUS] armed=true reached=3680 yielded=3593 leaked=87
```

87 dispatches reached the dial and were **not** yielded. A leaked row reports
the NATIVE's answer while reading, in a three-arm diff, as "the bytecode is fine
here". `Class.isArray` was one: the armed arm printed `true/false`, matching
HotSpot exactly, and the real retirement answers `false/false`. **`leaked > 0`
means some rows in that arm are not evidence, and the arm does not say which.**

**2. An agreement at the value a blanket yield returns anyway is not
evidence.** This trap fired three times in one session and it is the single
biggest source of wrong table entries:

```text
Module.getName        probed on an UNNAMED module -> null   yield returns null
Module.getDescriptor  probed on an UNNAMED module -> null   yield returns null
Module.canRead        probed only in its TRUE direction     yield returns true
Module.getClassLoader probed as "java.base's is null"       yield returns null
```

All four read as clean agreements. A retirement answering `null`/`true` for
everything satisfies every one of them, and `Module.getClassLoader` does
exactly that — it answers `null` for a PLATFORM-loaded module, which is what
`RLoaderIdentity` asserts and what cost five vectors in the second round.
**Before banking an agreement, ask what the yield returns for the whole family
and whether this row's correct answer differs from it.** If it does not, the row
is untested; write the discriminating row (a NAMED module, the FALSE direction,
a non-null loader) or hold the triple.

**3. An invocation that is not the harness's own is not a control.** Run
directly with `-cp regression-suite/build`, four of the five second-round
failures failed on the CONTROL binary too and read as "not mine" — they need a
`--module-path` that `run.sh` supplies. Re-run through the harness with
`ONLY="..."`, the control scored 5/0 and all five were mine. Always A/B with
`ONLY=` through `run.sh`, never with a hand-written classpath.

**And re-freeze a withdrawal from a forced failure.** `stub_ratchet`'s
assertion is `<=`, so removing entries passes silently: the constants sat 16
above the tree with every arm green. Set each baseline to `1`, run, and read the
failure's own paste-ready line. Arithmetic on the old constant is not a
measurement.

### When the arm fails and nothing says WHICH row: the census attributes it

Added 2026-09-11 after lane L0 used it to go from an empty table back to 19
retirements in an afternoon. The situation it is for: `OK -> BAD == 0` holds on
your probe, the corpus arm still loses vectors, and a bisection over the table
is one build per hypothesis at ~65 minutes each.

The per-vector census answers a **weaker** question for free, and the weaker
question is usually enough — *which of my table's triples does this failing
vector dispatch at all?* A registration a vector never consults cannot be the
row that broke it.

```bash
env CRATONVM_ARGS=--jdk-only CV=<CONTROL binary, table EMPTY>     ONLY="RFailing1 RFailing2 RFailing3"     KEEP_JDK_ONLY_REPORTS=$SP/rep bash regression-suite/run.sh
```

Each kept `<Vector>.json` holds one-line-JSON `native-shadows-bytecode` rows
with `class`/`method`/`descriptor`. Intersect with your table: the **union over
the failing vectors** is what you withdraw, and the complement is untouched by
every vector that failed.

Four things that make it sound rather than suggestive, and each is a way to get
it wrong:

* **Run it on the CONTROL binary.** With the table in, your retired rows are
  refused and never appear in the census at all — you would measure an empty
  intersection and conclude everything is safe.
* **Check the run's own `saturation:` line.** `none` means the counts are
  totals. Anything else makes them floors, and a floor cannot support "this
  vector never dispatches that triple".
* **Close over the whole corpus, not just the vectors you ran.** Pair the
  attribution with an arm that already PASSED with those rows in the table.
  L0's round-1 arm scored 127 passed / 5 failed with the 19 among its 38 rows,
  so those 127 are measured rather than assumed; 127 exonerated + 5 attributed
  away = all 132.
* **It over-collects on purpose.** It names every row that *could* be
  responsible, never the one that *is*. Withdraw them "as touched, not as
  convicted", and pin them out with a test so a later wave has to do the
  bisection instead of re-adding one by hand.

And it does not replace the arm: "not dispatched in these five" is not "not
dispatched anywhere". The wave still gets the three arms on its own binary.

### After the build: prove the retirement is not INERT before reading a probe

The four preconditions above decide whether to retire. This is the first thing
to check once you have, and it is not any of them: **a refusal is a retirement
only when nothing already owns the triple.**

`NativeMethodRegistry::register_inner` refuses a `SyntheticStub` under
`--jdk-only` without inserting it, which is what lets the real bytecode run. But
`JdkOnlyViolation::SyntheticNativeRegistered` carries a `survivor`, and when it
is non-null an EARLIER registration of the same triple is still in the slot and
still serving — so strict mode runs that older native instead of the bytecode
the policy asked for, every probe reads exactly as it did before, and the wave
is a no-op that looks like a clean result. Re-registration is common: 53 of the
185 triples in the 2026-09-09 CHM/`Properties` wave are registered more than
once.

```text
python - <<'PY' report.json      # --jdk-only-report from any run of your probe
import json, sys
pre = ("java/util/concurrent/ConcurrentHashMap", "java/util/Properties")
v = json.load(open(sys.argv[1], encoding="utf-8"))["violations"]
ours = [r for r in v if r["kind"] == "synthetic-native-registered"
        and r["class"].startswith(pre)]
print(len(ours), "refusals,", sum(1 for r in ours if r["survivor"]), "with a survivor")
PY
```

Zero survivors is the answer you need. Anything else means the table entry is
inert for that triple and the registration that supersedes it has to be found
and dealt with first.

### A probe whose noise floor exceeds the effect cannot score a retirement

The 2026-09-09 wave moved two probes of 115. One was
`SystemRuntimeObjectSweep`, +4, and it was a real defect. The other was
`VtHandoffProbe`, −4, and it was NOTHING: its rows are thread counts (`polls
that received a value |96|` against `|76|`, `threads joined |510|` against
`|512|`) and both arms are wrong against HotSpot in the same way on every run.

A negative delta is the shape of the result you want, which is exactly why it is
the one to distrust. Before recording an improvement, read the ROWS that moved
and ask whether the probe could have produced that delta with no change at all —
`measure a flaky vector's noise floor before explaining it` applies to the good
news too.

**Two named offenders, with the evidence, so the next lane does not re-derive
it.** `VtHandoffProbe` and `JdkOnlyPlatformProbe` both count virtual-thread
handoffs, and both counts are nondeterministic on this VM. Six successive
whole-tree A/Bs across the 2026-09-09/10 waves scored `JdkOnlyPlatformProbe` at
delta `0, -2, 0, 0, +2, +2` — it oscillates in BOTH directions — and the row is
one line of one probe.

**The proof that no binary attribution is possible.** `cratonvm-p8.exe`
differed from HotSpot on this row when it was the TRIAL arm of one A/B, and was
byte-identical to HotSpot when the very next A/B used it as the CONTROL. One
binary, one probe, opposite verdicts. That is the noise floor, measured, and it
is wider than any delta this campaign has claimed from this probe.

**And it is two fields, not one** — a correction to the earlier note here,
which named `handoffs` alone:

```text
HotSpot   ... handoffs=64  allJoined=true  ...
observed  ... handoffs=50 / 60 / 63 / 64   allJoined=true / false
```

They drift independently, so a run can match on `handoffs` and differ on
`allJoined` — which is exactly what the 2026-09-10 `+2` was. A lane that
chases only the handoff count is looking at the wrong half of the line about as
often as the right one. `diff` scores the whole line either way, so the delta
is `2` whichever field moved, and the delta alone cannot tell you which.

A delta from either probe is a coin flip until someone fixes both counts.
Neither is a reason to hold a retirement, and neither is a win to claim.
