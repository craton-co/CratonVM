# W7-60 — the test harness could not see failures, and now cannot fail to

Status: **the filter is measured, the population is closed, the instrument is
mutation-checked, and the three named vectors are repaired.** One new finding
arrived inside the repair (§5).

> **2026-08-12 — §7.3, the last recorded-not-fixed row, is CLOSED.**
> `RPriorityQueueGc` and `RTreeRangeGc` publish check counts and their rows are
> **deleted** from `regression-suite/harness-uncounted.txt`; that file now holds
> exactly one entry, `RClassUnloadSweep`. Deleting rather than annotating is
> forced by the ratchet's reverse direction, which is the half that keeps the
> baseline from decaying. Detail in §7.3 itself; the two things worth carrying
> forward are:
>
> * **The deferred "what is the unit" decision needed no run.** The unit is one
>   `check()` call, matching every other vector. The count is a property of the
>   SOURCE — how many assertion sites executed — not of the heap; `--nojit` and
>   `--Xmx 64m` decide whether those assertions FAIL, not how many of them run.
>   The old rows' premise that a count "measured outside those conditions means
>   nothing" conflated the two.
> * **One of the two needed a genuinely uncounted twin, and that is the finding.**
>   `RPriorityQueueGc`'s concurrent drain loop runs three assertions per element
>   drained, and how many elements survive the workers' interleaved `poll()`s is
>   scheduling-dependent — the vector's own comment already says so about the
>   neighbouring `CK` line. Folding those into a count that is *diffed against
>   HotSpot* would have made a CORRECT VM go red at random: a new instrument
>   defect installed while closing an instrument defect, the §5 reflex again.
>   They go through `checkDyn()`, identical failure behaviour, not counted.
>
> **A premise handed to that lane was also false and is recorded so it is not
> re-tried:** `RMapResizeGc` and `RMapGcStress` were reported as uncounted and
> unbaselined, i.e. as two live `HARNESS ERROR` rows on every run. They are
> neither. Both print `CK n=…` **and** `PASS <Class> (N checks)`
> (`RMapResizeGc.java:108`, `RMapGcStress.java:233`), so G2 and G3 are both
> satisfied, and adding them to `harness-uncounted.txt` would have fired the
> reverse ratchet on every run — turning a green pair red in the name of fixing
> them. Re-measured by reading all 72 sources: the only vectors without a count
> were the two closed here plus `RClassUnloadSweep`, exactly as §1 recorded.

Predecessors: `W7-51-vacuous-sweep-round-2.md` (which found this, repaired three
vectors, and left the harness itself and three named vectors open),
`W6-5-vacuous-tests.md` (round 1, the two shapes).

---

## 0. The headline

`regression-suite/run.sh` reduced every vector's output to its `PASS` and `CK`
lines before diffing CratonVM against HotSpot:

```sh
extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }
```

The filter is right — CratonVM interleaves timestamped WARN/tracing output with
the vector's own, and a raw diff would be red on every class for reasons that
have nothing to do with the VM. What was missing is that **nothing checked that
anything meaningful survived it.** Three scheduled vectors printed their entire
evidence on other prefixes, so what reached the diff was the constant
`PASS <Class>`, and a constant always matches itself.

W7-51 measured that side by side: `RDataInputFastPull` with a one-line defect
injected — a typed read dropping the high byte of `readShort()`, exactly the
partial fast pull the vector exists to catch — exited **rc=0 with output
byte-identical to a clean run**. Scheduled, executed, incapable of failing.

| | this round |
| --- | --- |
| vectors executed on HotSpot and re-filtered through `extract()` | **70 / 70** |
| vectors whose evidence `extract()` discards, **after** W7-51's three repairs | **0** |
| vectors whose extracted output is a bare constant | **0** |
| vectors that publish no count of the assertions they ran | **5** → **3** → **1** (2026-08-12, §7.3) |
| guards added | 4, each **mutation-checked** |
| vectors repaired here | 5 (`RNioNoFollow`, `RCrypto`, `RJdkX509Intercept`, `RFileTimes`, and `RNioNoFollow`'s uncovered line-separator bug) |
| mutants run | **11**, each side by side against the pre-repair source |

**No VM code is touched.** Compatible mode is untouched, no `CRATONVM_*` flag is
introduced, and no vector was weakened or deleted.

---

## 1. The true count — measured, not sampled

W7-51 named three vectors and did not ask how many more there were. The question
has an exact answer, and getting it needs no CratonVM build, because **HotSpot
prints exactly what the vector prints and nothing else.** The set of lines
`extract()` drops from the ORACLE's output is therefore precisely the evidence
the harness is blind to. The same subtraction against CratonVM's output would be
swamped by the VM's own tracing and could never be made fatal — which is the
reason this measurement had not been made.

All 70 scheduled vectors (42 `CORE_CLASSES` + 28 `JDKONLY_CLASSES`) were
compiled and run on Temurin 25.0.3+9 with the suite's real module path and
class-path service resources, and each one's output was subtracted through
`extract()`.

**Result: after W7-51's three vector repairs are applied, ZERO scheduled vectors
print evidence the harness discards.** The only dropped lines anywhere in the 70
are eight `WARNING:` lines — the four-line `System.loadLibrary` restricted-method
block, in `RJdkJni` and `RJdkFailure` — which are JDK stderr, not vector
evidence, and are correctly filtered.

So the population W7-51 opened is exactly the three it repaired. That is the
answer to "how many others", and it is a measurement rather than a hope; it is
also why the guard below is armed as a hard error rather than a baseline.

### 1.1 The adjacent population the same census exposed

In the census as taken, ten vectors — `RCollections`, `RStrings`, `RNumbers`,
`RSerial`, `RCrypto`, `RExceptions`, `RReflect`, `ROptionalClassForName`,
`RPrivateLambdaOwner`, `RDirectBufferElem` — reduced to a **single line** with no
`CK` line at all. (`RCrypto` is one of the ten and is repaired in §4.1, for a
different reason: its round trips, not its filtering. The other nine are left
alone, and the paragraph below is why.) They
look like the defect and are not, and the distinction is worth stating because
getting it wrong would have produced ten pointless "repairs":

* the three W7-51 found had **no local assertions**. They printed values and
  relied on the cross-VM diff, and the diff had been deleted from under them, so
  nothing was left.
* these ten assert locally, through a `check()` that throws, so a wrong answer
  exits non-zero and `run.sh`'s rc check catches it. What they lose is only the
  *second* instrument.

Their `PASS` lines carry a check counter (`PASS RCollections (53 checks)`), and
that counter is not decoration: it is data-dependent on how many assertions
actually executed, so it still discriminates the one failure mode the rc check
cannot see — **a vector that silently ran FEWER assertions than the oracle.**

Which is the shape `RNioNoFollow` had, and is why the third guard exists.

---

## 2. What the guard catches — `regression-suite/harness-guard.sh`

Repairing three vectors fixes three vectors. It does not stop the fourth. Four
guards now run per vector on every `run.sh` invocation, and `extract()` moves
into this file so it is defined **once**: the filter the suite diffs through and
the filter the guards reason about drifting apart would be the same defect one
level up.

| | fires when | today |
| --- | --- | --- |
| **G1** DISCARDED EVIDENCE | the ORACLE printed a line `extract()` deletes and that is not JDK stderr noise | 0 offenders |
| **G2** CONSTANT EXTRACT | what survives carries no observable at all — no `CK` line, no check count — so the diff compares a constant against itself. Empty extract is the degenerate case | 0 |
| **G3** NO CHECK COUNT | the vector publishes no count of the assertions it executed | 3, all baselined → **1** (2026-08-12) |
| **G4** SICK ORACLE | the HotSpot run supplying ground truth did not itself succeed | 0 |

**G4 is a defect in its own right, found while building the guard.** `run.sh`
did `hskey=$(timeout "$TIMEOUT" "$HS" ... | extract)` — the oracle's exit code
was **discarded**. A HotSpot run that crashed or hit the 120 s timeout silently
became a TRUNCATED ground truth. It failed safe in the common case (a truncated
oracle disagrees with a healthy CratonVM, so the diff goes red) but not in the
one that matters: an oracle that hung *after* printing all its `CK` lines
produces an `hskey` identical to a healthy one, and the timeout is invisible.
The raw output and the rc are both kept now.

**G3 is ratcheted, in both directions**, against `regression-suite/harness-uncounted.txt`.
A vector that stops publishing a count is a regression; a vector that *starts*
publishing one is **also** an error, so a repair is not finished until its row is
deleted. A baseline that only ever records "known bad" decays into a list nobody
re-checks; this one cannot, because clearing an entry is what makes the run green
again — the ratchet shape `L6-unadjudicated-bridge-ratchet-DONE-20260805.md`
established.

The three baselined entries each carry their reason: `RClassUnloadSweep` has zero
assertions by design (whether a weak reference has been cleared is a GC-policy
outcome, not a language guarantee, so its one observable is deliberately
diff-only); ~~`RPriorityQueueGc` and `RTreeRangeGc` have no counter at all, and
adding one to them is a real repair that belongs to a lane which can run them
under the `--nojit --Xmx 64m` reproduction conditions `class_cv_args()` supplies,
since a count measured outside those conditions means nothing.~~
**Both closed 2026-08-12 — see §7.3 and the box at the top. That clause was
wrong about *why* it was hard: the count is a source property, and the flags
decide whether the assertions fail, not how many run.** One entry remains.

### 2.1 The guard is mutation-checked, which is the only reason to believe it

A guard that has never been shown to fire is the same species of defect as the
one it guards against. `regression-suite/harness-selfcheck.sh` runs the four
guards against **HotSpot alone** — no CratonVM binary, so it is reproducible on
any lane and cheap enough for CI — and takes `MUTATE=<Class>:<sed-expr>`.

It **refuses a `MUTATE` expression that left the source byte-identical.** A
no-op mutant produces a green run that looks like evidence and is not, which is
the failure mode this whole record is about; the refusal is itself listed below
as M6.

| mutant | models | fired |
| --- | --- | --- |
| M0 | `RJitGc` unmutated — the positive control | *sound*, as it must be |
| M1 | `RJitGc` evidence moved off the `CK` prefix | **G1**, naming the dropped line |
| M2 | `RCollections` check counter removed | **G2** (+G3) |
| M3 | `RChmKeySetView` stops publishing its count | **G3** (+G1) |
| M4 | `RJitGc` oracle dies in `<clinit>` | **G4** (+G2, G3) |
| M5 | `RJitGc` listed in `harness-uncounted.txt` while it does count | **G3, reverse direction** |
| M6 | a `sed` expression matching nothing | **refused before compiling** |

M0 is not padding. Without it, a guard that flagged everything would satisfy
every other row in this table.

The whole of `run.sh` was then driven end to end over all 70 vectors with a
HotSpot-backed stand-in launcher, which exercises the guards on the real control
flow rather than in the self-check harness only.

---

## 3. `RNioNoFollow` — it asserted nothing on the platform the suite is run from

`Files.createSymbolicLink` needs a privilege Windows does not grant an
unprivileged process. On failure the vector printed
`CK RNioNoFollow symlinks=unavailable`, printed `PASS`, and **returned, skipping
all 27 checks**. Both VMs printed the same bail-out line, so the cross-VM diff
agreed, the exit code was 0, and the gate was green. `run.sh`'s own header says
the suite is usually run from Git Bash on Windows, so **on the primary platform
this vector was inert for its entire scheduled life.** Its comment described that
as the design: *"both VMs print it, so the cross-VM diff still matches"*.

The guard was over-broad twice over. Six checks need no symlink whatever —
`NOFOLLOW_LINKS` on a regular file (write, read, channel), `APPEND`, the
`CREATE_NEW` refusal, the shorter-write truncation, the 4 MiB round trip and
`Files.write(Iterable)` — and they travel the **same option scanner the defect
lived in**: it read `APPEND` and `CREATE_NEW` and nothing else, which is exactly
why `NOFOLLOW_LINKS` went unseen. They were discarded with the rest.

Repaired by scoping, not by loosening: the symlink arms move to `symlinkArms()`,
the rest to `plainFileArms()`, and the count of assertions that **actually ran**
goes on the `PASS` line. That count is the instrument — a VM that bails where
the oracle does not now reports a different number and the diff goes red.

**Measured side by side on Windows**, two mutants each modelling a defect this
vector exists to catch:

| mutant | pre-repair | post-repair |
| --- | --- | --- |
| `Files.write` forgot `TRUNCATE_EXISTING` | **rc=0**, filtered output byte-identical to clean | rc=1 |
| the option scanner ignores `CREATE_NEW` | **rc=0**, filtered output byte-identical to clean | rc=1 |

0 checks ran before on this platform; 10 run now.

---

## 4. `RCrypto` and `RJdkX509Intercept` — round trips and a missing negative

Both were specified in W7-51 §2.5 with mutation evidence and neither repair was
reached. Both are re-measured here **with the record's own mutants**, side by
side against the pre-repair source.

### 4.1 `RCrypto`

Four of seven checks were round trips whose "expected" value was produced by the
same implementation under test. A round trip cannot catch a wrong algorithm, a
missing authentication tag, or a verifier that answers `true`. The principle was
already written down in this repository — `RChaCha20Cipher.java:34`, *"a
round-trip test cannot catch any of that"* — and implemented correctly in
`RJdkSecurity.java`. `RCrypto` had no such arm.

Three shapes were added, each killing a different fake:

* an **AES-256-GCM known answer** on the fixed key/IV, measured on Temurin
  25.0.3+9, plus a tag-length check;
* **AES-256-GCM out of the GCM specification, test case 16** — key, IV,
  plaintext, AAD and expected output all published values, so unlike every other
  assertion in the file it cannot be satisfied by an implementation agreeing
  with itself. It matches SunJCE byte for byte, and that agreement between an
  independent published vector and the JDK is what makes the *measured* known
  answer above trustworthy. It also exercises AAD, which the fixed-key arm does
  not, and a 60-byte plaintext that crosses the block boundary four times;
* **refusals.** Six that GCM must make (flipped ciphertext byte, flipped tag
  byte, wrong key, wrong IV, mismatched AAD) and three RSA ones (OAEP under the
  wrong private key, OAEP on a corrupted ciphertext, PKCS1 under the wrong
  private key — the last is exactly where the constant-time-unpad regression
  this vector was written for lived); and three signature negatives: a different
  message, a different public key, a corrupted signature.

7 → 27 checks, 0 → 10 `CK` lines.

**Mutant: the record's provider, verbatim** — installed at position 1, AES-GCM
the identity function with no tag, `engineVerify` returning `true`
unconditionally.

| | exit | filtered output |
| --- | --- | --- |
| pre-repair | **rc=0** | `PASS RCrypto (7 checks)` — byte-identical to the clean run |
| post-repair | rc=1 | fails at `AES-256-GCM known answer:`, whose reported value is `4145532d47434d...` — the plaintext |

And with **only** the always-true `engineVerify` registered, the real cipher left
in place, so the signature half is measured on its own:

| | exit | |
| --- | --- | --- |
| pre-repair | **rc=0** | byte-identical |
| post-repair | rc=1 | `a signature must NOT verify against a different message` |

### 4.2 `RJdkX509Intercept`

The negative control was written in the comment — *"and must NOT verify against
a different one"* — and never in the code. Two are added, and they fail for
different reasons on purpose: **the first proves the KEY is consulted, the second
proves the SIGNATURE BYTES are.** A `verify()` comparing only key identity would
pass the second; one ignoring the key entirely would pass the first.

* the wrong key, derived from the certificate's **own** SPKI with one bit
  flipped inside the modulus — a well-formed RSA-2048 key that is not this one,
  with no key generation, so the vector stays deterministic and gains no
  dependency on a working `KeyPairGenerator` under `--jdk-only`;
* a tampered certificate: one bit flipped in the signature `BIT STRING`, the
  last element of the DER, so it still parses and only its signature is wrong.

22 → 26 checks. **Mutant: the record's delegating `X509Certificate` whose
`verify()` is a no-op** — a VM that accepts any certificate under any key.

| | exit | filtered output |
| --- | --- | --- |
| pre-repair | **rc=0** | `CK RJdkX509Intercept verify=ok` — byte-identical to the clean run |
| post-repair | rc=1 | `a certificate must NOT verify against a different public key` |

---

## 5. Two things found inside the repairs

Both are the reason mutation is run on the repair and not only on the defect.

**A latent vector bug the scoping uncovered.** `Files.write(Path, Iterable)`
terminates each element with `System.lineSeparator()`, not `'\n'`, so
`RNioNoFollow`'s `equals("alpha\nbeta\n")` is false on Windows. That line had
**never once executed there**, because the bail-out returned before reaching it.
It is a defect in the vector, not in any VM. Fixed against the separator — which
is *also* published as bytes on a `CK` line, so a VM whose `Files.write` and
whose `System.lineSeparator()` are wrong in the same direction still disagrees
with the oracle rather than agreeing with itself.

**A vacuous assertion written while repairing vacuous assertions.** `RCrypto`'s
corrupted-signature arm was first written as a body that throws `AssertionError`
on success, run through the file's `refused()` helper. `AssertionError` is an
`Error`, not an `Exception`, so it escaped the helper entirely and the `check`
that named the arm was **unreachable code that read like coverage** — the exact
defect this record is about, one level down. Caught by running the mutant, which
failed with the wrong message. It is now a captured token.

W7-51 §3 records the same reflex — a `12 > 0` constant assertion written and
deleted — and W6-5 §3.4 records it once before that. **This is the fourth
occurrence across four lanes, every one by an author actively working on this
defect class.** Reading the diff does not catch it. Running a mutant and
checking *which* assertion failed does.

---

## 6. What the orchestrator should expect

**A lower pass count that represents better measurement.** Concretely:

1. **`RNioNoFollow` on Windows now executes 10 assertions where it executed
   zero.** Any of them can now be red. That is the coverage arriving, not
   breakage.
2. **`RCrypto` runs 27 checks instead of 7**, including nine refusals and three
   signature negatives, and now publishes ten `CK` lines the cross-VM diff
   compares. A CratonVM whose AES-GCM omits the tag, whose GCM ignores AAD,
   whose RSA unpadder accepts the wrong key, or whose `Signature.verify` is
   permissive will go red here for the first time. Its AES-GCM known answers are
   the likeliest first red.
3. **`RJdkX509Intercept` runs 26 instead of 22**, and now needs
   `KeyFactory.getInstance("RSA")` with `X509EncodedKeySpec` to work under
   `--jdk-only`. If that path is unregistered in strict mode, this vector goes
   red for a reason that is a genuine finding about strict mode.
4. **`RForeignLayoutCollections`, `RDataInputFastPull`, `RClassUnloadSweep`**
   carry W7-51's repairs and put 43, 23 and 1 observables into the diff where
   they previously put none. Any of the three may be red on the first CratonVM
   run; each such red is the defect the vector was written for, finally visible.
5. **`RFileTimes`** publishes `CK RFileTimes checks=40` where it published the
   unanchored `CK checks 40`, and its `PASS` line gained a count. Both are
   *diffed* lines, so this changes the expected output — it is not a behaviour
   change, but a stale expectation elsewhere would show up as a red.
6. **The harness guards are FATAL, not advisory.** A vector added in another
   lane that prints evidence on a non-`PASS`/`CK` prefix, or that publishes no
   observable, now fails `run.sh`. That is deliberate: a warning inside a green
   build is how the previous version of this defect survived long enough to be
   measured. They report under a separate `HARNESS:` heading and separate
   counters, because "the VM answered wrongly" and "the instrument cannot see
   the answer" are different findings and must not be summed.
7. **A new CI step**, `Regression-suite harness self-check (HotSpot only)`, in
   the `jdk-only` job. It needs no CratonVM binary, so it still measures the
   harness when the build breaks. Green over all 70 vectors when added.
8. **ADDED 2026-08-12.** `RPriorityQueueGc` and `RTreeRangeGc` now print
   `CK <Class> checks=N` and `PASS <Class> (N checks)` where they printed a bare
   `PASS <Class>`. Two new diffed lines each, and `harness-uncounted.txt` loses
   two rows — leaving it at one. Nothing about either vector's behaviour changed;
   if either is red on the first run it is for the reason `class_cv_args()`
   supplies its flags, which is what those gates are for.

Everything above was measured on HotSpot 25.0.3+9 on Windows. **No CratonVM run
was possible in this lane**, so no claim here is a claim about CratonVM's
behaviour, and none is presented as one.

---

## 7. What would falsify this lane

1. **The self-check is red on the first CI build.** It was green over all 70 at
   this commit, so a red means a vector landed between the measurement and the
   run — which is the step working.
2. **The three repaired vectors are red under CratonVM.** Expected, and
   enumerated in §6. Each red is a finding.
3. ~~**G3's baseline is wrong about `RPriorityQueueGc` / `RTreeRangeGc`.** Their
   rows say a count needs a decision about what the unit is, measured under
   their reproduction flags. If someone can make that decision cheaply, the rows
   should go.~~ **This falsifier FIRED, 2026-08-12, and the rows are gone.** The
   decision was cheap and the rows' stated reason for deferring it was wrong.
   What replaces it as a falsifier is narrower: **the two new counts must be
   constants on a healthy run.** `RTreeRangeGc`'s every loop is over a
   fixed-size collection, and `RPriorityQueueGc`'s scheduling-dependent drain is
   excluded through `checkDyn()` — if either number moves between two green runs
   of the SAME VM, the exclusion is incomplete and the count must come back out
   (or the row go back in) rather than be widened. Both vectors also gain a
   `PASS <Class> (N checks)` shape where they printed a bare `PASS <Class>`, so
   their expected output changes: that is a diffed line, not a behaviour change.
4. **`extract()` might be the wrong filter, not merely an unguarded one.** This
   lane deliberately did not widen it: widening admits CratonVM's tracing into
   the diff and turns every vector red. The claim defended here is narrower and
   testable — *nothing meaningful is outside the filter* — and it is now checked
   on every run instead of assumed.

---

## 8. The one-line lesson

W7-51's lesson was that a vacuous test is found by breaking the code it claims to
cover. The lesson here is the next one: **the same question has to be asked of
the instrument, and it has a cheaper answer than anyone expected.** The oracle's
output is exactly the vector's output, so subtracting the filter from it names
every blind vector in one pass, with no VM build, in under a minute — and the
subtraction had never been performed, in a suite that had been read carefully
many times by people who wrote precise headers about what it measured.

---

## 9. Verified in the tree, and the harness this record did not reach — 2026-08-12 (doc-only lane)

No cargo, no Rust, no run. This section is a **verification** of what this record
claims about the tree, and a **finding** about the neighbouring harness.

### 9.1 Everything this record claims about the tree is there

Checked by symbol, not by line number:

* `extract()` is defined **exactly once**, at `regression-suite/harness-guard.sh:53`.
  `run.sh` does not redefine it; it sources the guard file at `run.sh:236` with a
  hard `exit 3` if the source fails. The "two filters drifting apart would be the
  same defect one level up" argument holds in the tree, not just on paper.
* The guards are invoked on the real control flow — `harness_guard_oracle` at
  `run.sh:496`, `harness_guard_extract` at `:507` — and they are **fatal, not
  advisory**: `guarded=1` feeds `hbad`/`hfailed`, which is folded into
  `total_failed` at `run.sh:613` and reported under its own `HARNESS:` heading at
  `:609`. The separate-counter claim of §6.6 is real; so is the exit-status claim.
* G2/G3 deliberately run even with no HotSpot present (`run.sh:502-507` and its
  comment), which is the run where they matter most.
* `harness-uncounted.txt` holds **exactly one** live entry, `RClassUnloadSweep`,
  with the two cleared rows recorded as a comment block rather than as rows —
  the reverse-ratchet direction, applied to itself.
* CI wiring: `harness-selfcheck.sh` at `.github/workflows/ci.yml:1332`,
  `STRICT_COVERAGE: 1` at `:1350`, `run.sh` at `:1354`, and W7-51's
  `CRATONVM_REQUIRE_E2E: 1` producer at `:211`.

### 9.2 The blind spot the same subtraction has not been pointed at

This record closed the question *"what does the instrument delete"* for the 72
vectors `run.sh` schedules. The adjacent corpus is untouched and much larger:

| | files | scheduled |
| --- | --- | --- |
| `regression-suite/src/*.java` | 72 | 70, and four guards watch them |
| `probes/*.java` | **449** | **3** |

The string `probes` appears **zero** times in `run.sh` at any `SUITE=` value. The
sole scheduled consumer is `scripts/jdk-only-strict-probes.sh`
(`ci.yml:315`, `:1404`), whose `PROBE_LIST` default is
`JdkOnlyCensusLoadProbe JdkOnlyBreadthProbe JdkOnlyPlatformProbe` plus the agent.
**This is the standing reason a finding whose only evidence is a probe cannot be
discharged by a suite run, however green** — it is the live situation for
`W7-33-differential-dead-sections.md` and `W7-36-differential-view-families.md`,
whose entire evidence base is `probes/ShadowDifferentialProbe.java`, a file named
by no `.sh` and no `.yml` in the tree.

That is not a call to schedule 449 probes. It is the reason to say, per record,
whether a claim has a scheduled witness — and this record's numbers, uniquely in
the set, do.

### 9.3 G4's defect, still live and still scheduled, in the harness next door

**This is the finding of this section.** G4 exists because `run.sh` discarded the
oracle's exit code, so a control that never produced evidence was scored as
ground truth. `scripts/jdk-only-strict-probes.sh` — which CI runs on the whole
OS × JDK matrix — has the same defect in a second form, and its own comments
argue that the defect is correct behaviour:

```
scripts/jdk-only-strict-probes.sh:249-251
    echo "WARNING: the agent jar did not build; the agent section will report absent"
    echo "         in EVERY arm, so the arms still agree and the gate stays honest."
```

```
scripts/jdk-only-strict-probes.sh:277, :280
      echo "         The jni section will report lib=absent in EVERY arm."
    echo "WARNING: no C compiler ($CC); the jni section reports lib=absent in every arm."
```

The gate is a cross-arm **agreement** ratchet. When a fixture fails to build,
every arm reports `absent`, every arm agrees, no section diverges, and the script
reaches `RESULT: PASS` with `exit 0`. The whole agent section and the whole JNI
section — the two that cover the JNI boundary and instrumentation under
`--jdk-only`, i.e. the surface least covered anywhere else — are silently
switched off, and the only trace is a `WARNING:` inside a green build.

**Three arms that all failed to build agree with each other**, and agreement
between three broken instruments is this record's entire subject. It is exactly
the shape §6.6 rejects — *"a warning inside a green build is how the previous
version of this defect survived long enough to be measured"* — and exactly the
shape G4 was written to make fatal. The script already has the right vocabulary
for it: its self-test failure path prints `RESULT: REFUSED` and exits 2, on the
principle that a gate which cannot adjudicate must say so rather than pass.

W7-51 §3 has carried this as an open residual since it was found. It is harder to
close now than when it was merely unnoticed, because the source comments assert
that the behaviour is honest, so a fixer has to disagree with the file first.
**NOMINATION below**, with exact text; a doc-only lane cannot make it.

#### NOMINATION — `scripts/jdk-only-strict-probes.sh`: a fixture that did not build must REFUSE, not PASS

Shape deliberately mirrors `harness-uncounted.txt`: degradation is allowed, but
only when it is **declared**, so the default run cannot be quietly hollowed out.
Three edits, anchored on literal text.

**(1)** After the `SRCS=""` initialisation block, add the flag. Anchor —
`scripts/jdk-only-strict-probes.sh`, immediately before the agent fixture
comment:

```sh
# ------------------------------------------------------- fixture: the agent jar
```

becomes

```sh
# Set by any fixture that failed to build. A missing fixture makes its section
# report `absent` in EVERY arm, so the arms AGREE and the agreement ratchet sees
# no divergence — the section is switched off and the gate still says PASS. That
# is the G4 defect of W7-60-harness-extract-blindness.md: a control that produced
# no evidence being scored as ground truth. Declared degradation is fine;
# silent degradation is not.
DEGRADED_FIXTURES=""

# ------------------------------------------------------- fixture: the agent jar
```

**(2)** Mark each degraded fixture. Three sites, each gaining one line.

```sh
    echo "WARNING: the agent jar did not build; the agent section will report absent"
    echo "         in EVERY arm, so the arms still agree and the gate stays honest."
```

becomes

```sh
    echo "WARNING: the agent jar did not build; the agent section will report absent"
    echo "         in EVERY arm. The arms then agree because NEITHER measured"
    echo "         anything, which is not honesty — see the DEGRADED_FIXTURES note."
    DEGRADED_FIXTURES="$DEGRADED_FIXTURES agent-jar"
```

```sh
      echo "WARNING: the JNI fixture did not build (see $OUT/logs/jni-build.log)."
      echo "         The jni section will report lib=absent in EVERY arm."
```

becomes

```sh
      echo "WARNING: the JNI fixture did not build (see $OUT/logs/jni-build.log)."
      echo "         The jni section will report lib=absent in EVERY arm."
      DEGRADED_FIXTURES="$DEGRADED_FIXTURES jni-lib"
```

```sh
    echo "WARNING: no C compiler ($CC); the jni section reports lib=absent in every arm."
```

becomes

```sh
    echo "WARNING: no C compiler ($CC); the jni section reports lib=absent in every arm."
    DEGRADED_FIXTURES="$DEGRADED_FIXTURES jni-lib-no-cc"
```

**(3)** Refuse at the verdict. Anchor — the final three lines of the file:

```sh
echo "RESULT: PASS -- every arm completed and no section diverged that the"
echo "        baseline does not already carry."
exit 0
```

becomes

```sh
if [ -n "$DEGRADED_FIXTURES" ] && [ "${ALLOW_DEGRADED_FIXTURES:-0}" != "1" ]; then
  echo ""
  echo "DEGRADED FIXTURES:$DEGRADED_FIXTURES"
  echo "  Each of these switches a whole SECTION off in every arm at once. The"
  echo "  arms then agree, no section diverges, and the ratchet cannot fire --"
  echo "  so a PASS here would mean 'nothing was measured', not 'nothing broke'."
  echo "  Fix the fixture, or declare the degradation with"
  echo "  ALLOW_DEGRADED_FIXTURES=1 so the run says out loud what it did not cover."
  echo "RESULT: REFUSED -- a fixture did not build, so its section is absent in"
  echo "        every arm and the agreement between them is vacuous."
  exit 2
fi

echo "RESULT: PASS -- every arm completed and no section diverged that the"
echo "        baseline does not already carry."
exit 0
```

Two notes for whoever lands it. **A permanently-red job is a job nobody reads** —
W7-51 §1.2's rule — so if the CI matrix has a runner with no C compiler, that
runner's step sets `ALLOW_DEGRADED_FIXTURES=1` and the WARNING then stands as a
declared gap rather than a hidden one. And this changes an **exit code**, so it
must be run once on each matrix OS before landing: the failure mode of this fix
is a REFUSED on a platform where the fixture never built and nobody knew.

### 9.4 The instrument this record's `--jdk-only` neighbours should be using

Adjacent, and worth recording here because this record is the one about
instruments: for `--jdk-only` questions there is now a complete per-run census —
`cratonvm --jdk-only --explain-jdk-only --jdk-only-report r.json -cp <cp> <Main>`
emits `compatibility-class-requested`, `native-shadows-bytecode` and
`synthetic-native-registered` rows, each with class, method, descriptor and the
requester's `file:line`. Three cautions, all of the same species this record is
about:

* **the flags are silently ignored if placed after the main class** — no file, no
  warning, exit 0. A missing report is indistinguishable from a clean one, which
  is a G4-shaped hazard in the census itself;
* **`--explain-jdk-only` reports boot-time refusals and only those.** Every
  runtime refusal is silent, so a clean startup banner licenses nothing;
* **the census over-reports and a probe under-reports.** A requested-and-refused
  row is not a failure — the caller recovers onto real bytecode. Only the
  intersection of the two is the blocking set.
