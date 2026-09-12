# Lane 4 — `java.io`, `java.nio`, `sun.nio`, and the FFM internals

**Scope: 1,110 §1.4 shadows over 131 classes, from 615 registration sites.**
Prefixes: `java/io/`, `java/nio/`, `sun/nio/`, `jdk/internal/foreign`.
The largest prefix lane, and the one with the highest blast radius.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. Method, preconditions and
landing protocol: [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

## 1. Shape of the lane

```text
  54  java/io/File                       21  java/nio/DirectByteBuffer
  42  sun/nio/ch/SocketChannelImpl       18  java/io/DataInputStream
  39  java/nio/file/Files                18  jdk/internal/foreign/layout/...
  38  java/nio/ByteBuffer                17  java/nio/CharBuffer
  34  sun/nio/ch/DatagramChannelImpl     17  jdk/internal/foreign/MemorySessionImpl
  29  java/io/PrintStream               239  jdk/internal/foreign*  (total)
  26  sun/nio/ch/ServerSocketChannelImpl
```

## 2. Two package verdicts already on record — respect them or beat them per-triple

`RETIRED_SHADOW_PREFIXES` carries notes you must read before proposing anything
under `sun/nio/`:

- **`sun/nio/fs/` is admitted** (2026-08-20). Deliberately the narrow prefix.
- **`sun/nio/ch/` scored 34/36 on the 2026-08-19 dial sweep and "nothing under
  it is retirable"** as a package. Phase 2 then narrowed that to exactly **one**
  triple, measured on its own, and the note in the source says explicitly that
  this narrows rather than overrules the package verdict.
- **`java/lang/ref/` is absent on purpose.**

The rule those notes encode: *a prefix admits a package to the binary search;
the table decides what is retired.* A package verdict is the right default.
Beat it one triple at a time with that triple's own measurement, or not at all.

## 3. Channels overlap lane T — coordinate before you start

> **WITHDRAWN 2026-09-10 — the channel rows are YOURS, and there is no hold.**
> `concrete_receiver.rs:185` is class-parameterised, which is what hides it from
> the source-scanning drift gate, but it is not CROSS-LANE: all 191 of its goal
> rows are under `sun/nio/`, so lane 0 §2's rule puts every one of them in this
> lane, and lane 0's 1,100 for LT excludes them (1,100 + 191 is not 1,100).
> Measured from `--dump-native-registry --explain-jdk-only`; see [the lane T record](../../internal/jdk-only/lane-t-the-throwable-family-retired-and-the-three-defects-the-arm-had-to-find-first-20260910.md) §0.
> Take them with the rest of your `sun/nio/ch` surface — and take §2's package
> verdict as the default while you do.

`native-io/src/concrete_receiver.rs:185` is a **cross-lane registrar**: 191 rows
over 21 classes, including `SocketChannelImpl`, `DatagramChannelImpl` and
`ServerSocketChannelImpl`, which are your three biggest `sun/nio/ch` classes.
Lane T owns it whole.

So roughly 100 of your channel rows are **not yours to retire individually**.
Check lane T's page for a hold on that call site, and take the channel classes'
remaining hand-written rows only.

## 4. The failure mode this lane owns: a silent wrong answer

Every other lane's worst case is a null or an exception. Yours is corrupted
data, and it will pass a happy-path probe:

- **`ByteBuffer`/`CharBuffer`/`DirectByteBuffer` (76 rows)** — position, limit,
  mark and capacity invariants. A retirement that changes when `position` is
  advanced produces correct-looking output with the wrong bytes. Probe
  `flip`/`rewind`/`compact`/`slice`/`duplicate` **state triples** after each
  operation, relative reads mixed with absolute reads, and the
  `BufferUnderflowException`/`IllegalArgumentException` boundaries — with
  messages, since the route decides the text.
- **`jdk/internal/foreign` (239 rows)** — `MemorySessionImpl` liveness and
  layout arithmetic. A wrong `byteOffset` is a memory-safety bug, not a diff.
  Probe closed-session access, confinement violations from another thread, and
  alignment failures, and require each to *throw the right type*.
- **A fix that turns a loud failure into a rare silent one is not a fix.** This
  lane has already produced one: every metric moved the right way *and* the
  store was corrupted. Prefer a probe that reads data back and compares content
  over one that checks a return code.

`native-builtins/src/panama.rs:2752` (21 rows, 3 classes) is cross-lane with
L2 — coordinate.

## 5. `PrintStream` (29 rows) — do this last, and say so in the commit

`System.out` and `System.err` are how **every lane** reads its probes. A
regression here reads as all nine lanes' probes failing simultaneously, and the
first instinct will be to blame the harness. Two rules:

- Retire `PrintStream` in its own wave, after the rest of `java/io`, and name
  the blast radius in the commit message.
- Its probe must write to a `ByteArrayOutputStream`-backed stream as well as to
  `System.out`, so a broken `System.out` cannot hide the evidence of itself.

Watch the encoding path: `sun.nio.cs.UTF_8.<clinit>` captures the
`JavaLangAccess` getter on the way past, and that interaction has already cost
one build. If your change moves when charsets initialise, re-read L2 §3.

## 6. `java/io/File` (54) and `java/nio/file/Files` (39) — the platform split

The largest single class and one of the most portable-looking. It is not
portable: separators, absolute-path rules, `getCanonicalPath` on a
non-existent path, and permission methods all differ by OS, and **the two shell
gates in this campaign are keyed `25/linux` and refuse on Windows**. Two
consequences:

- Your probe rows must not embed a platform's path syntax as an expected value.
  Compare cratonvm against HotSpot **on the same host**, which is what the A/B
  scripts do.
- A `sun/nio/fs/` retirement measured only on one OS is measured on one OS. Say
  which, in the commit.

`File`'s methods that answer about a missing file (`exists`, `length`,
`lastModified`, `canRead`) return sentinel values rather than throwing —
another place a wrong answer is silent.

## 7. The increment loop

1. Funnel from a dump: owns slot, kind `Bridge`, image `Code`, `invocations > 0`
   in **your** instrument's run.
2. Probe + HotSpot oracle, on this host. No build needed.
3. Fill `RETIRED_SHADOW_L4_TRIPLES`, sorted and unique.
4. Build token (L0 §5); one build per wave — this lane's waves should be large,
   since its probes are expensive to write and cheap to re-run.
5. `N refusals, 0 survivors`.
6. Probe-tree A/B, `--jdk-only` corpus, `SUITE=all` at `TIMEOUT=600`, `all`-arm
   count. **Never a timing claim** — `RMapGcStress` read 146 s then 334 s on the
   same binary.
7. Full gate set. Kind-map rows. Commit. Do not push.

## 8. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named — with
the `sun/nio/ch` package verdict either upheld or beaten per-triple with the
measurement recorded, and every buffer/FFM retirement backed by a probe that
reads data back rather than checking a return code.


---

## 9. Progress

### Wave 1 — 2026-09-11, 140 rows over 10 classes of `java/io/` and `java/nio/`

`RETIRED_SHADOW_L4_TRIPLES` in `native-api/src/retired_shadow.rs`, whose doc
comment carries the method and the arithmetic. §9.1 is the ledger §8 asks for;
§9.2, §9.3 and §9.4 are the three findings that change how the next wave should
be run, and they cost this wave five of its seven builds.

The classes: `java/io/File` (51), `java/io/DataInputStream` (18),
`java/io/DataOutputStream` (15), `java/io/ByteArrayOutputStream` (13),
`java/io/FilterOutputStream` (5), `java/nio/ByteBuffer` (34), and the four
`java/nio/ByteBufferAsCharBuffer{B,L,RB,RL}.order()` views.

### 9.1 The ledger — every bucket-A/B row in the prefix set, with a disposition

Over the **union of thirteen probe dumps**, not one run: §1's table is a
single-dump census and undercounts a class whose rows only one workload reaches.

| disposition | rows |
|---|---:|
| `RETIRED` — wave 1 | **140** |
| blocked: no probe in this tree invokes it | 479 |
| blocked: `sun/nio/ch/` package verdict, 2026-08-19 | 308 |
| blocked: outside a dial-floor family | 131 |
| blocked: lane T owns `concrete_receiver.rs:185` | 49 |
| blocked: `java/nio/file/Files`, armed 4, above the floor | 39 |
| deferred: `java/io/PrintStream`, its own wave (§5) | 30 |
| **backed out by a build** (four families, §9.2; CharBuffer's 17 taken as wave 4) | **34** |
| blocked: `Path` carrier is stamped with the interface | 11 |
| reviewed `Intrinsic` | 10 |
| **total** | **1 254** |

**"No probe invokes it" is the largest bucket and it is not a verdict about
those rows.** `invocations` is a lower bound — schema 5 carries an
`invocations_complete` bit to say so — and 479 rows being unmeasured by this
instrument is a statement about the instrument. Over half are one shape:
`jdk/internal/foreign/layout/ValueLayouts$Of*Impl`, eight classes at ~16 rows
each, which a single FFM layout probe would move into the funnel in one pass.
That is the cheapest next wave in the lane.

### 9.2 The dial is not a model of a retirement

The dial declines a native at DISPATCH, and the decline is **conditional**: when
the receiver's own class has no concrete body to yield to it answers no and the
native runs anyway — `declined_no_bytecode`, 4 156 of 180 268 on this wave's
scope. A retirement has no such fallback. **Every row the dial scored 0 on
*because it declined to decline* is unmeasured by it.**

So a per-family sweep at DIFF 0, and the same families re-armed together as one
scope at 0 diffs over 4 416 rows with 176 112 real yields, still lost two
families on the first build: `java/nio/file/spi/FileSystemProvider` (the VM's
provider is fabricated onto the ABSTRACT class, so `createLink` has no body, so
the dial ran the native — retired, the call reaches the abstract declaration and
throws) and `java/nio/CharBuffer` (`subSequence(1,3)` answered `cd` for `bc`;
the real bodies take their window from `position()`/`limit()`, which this VM's
carrier does not keep where the real accessors read them).

`java/nio/ByteBuffer`'s 34 rows — same accessors, same probe — moved nothing.
CharBuffer is a carrier defect, not a buffer-wide one. **Taken as wave 4 on
2026-09-12, and the diagnosis in that last sentence was wrong: the carrier is
fine and has been since 2026-08-06. See below.**

**Use the dial to BOUND a wave. Never to certify one.**

### 9.3 Screen against the CORPUS, not only the probe tree

The 13 probes scored 0 in **both modes** on the built binary. The `--jdk-only`
corpus went **132/132 → 129/132**, reproducibly, on that same binary:

```text
  RFileTimes       every timestamp reads 1970-01-01T00:00:00Z
  RJdkSecurity     a property-named truststore IGNORED: 122 anchors for 1,
                   and a certificate HotSpot REJECTS is ACCEPTED
  RSslLiveSession  fails at client.responseCode = 200
```

`RJdkSecurity` is the one to read twice. It is not a crash and not a diff in a
probe — it is a **silently widened trust set**, which is precisely the failure
mode §4 says this lane owns, and no happy-path probe would ever have seen it.

`RFileTimes` has one cause, and it is the SETTER: `Files.setLastModifiedTime`
reads `FileTime.toMillis()` off a fabricated carrier, gets 0, and stamps the
file at the epoch — so all four read-back rows follow from one write. Dial-armed
attribution names `java/nio/file/attribute/` for it and
`java/io/ByteArrayInputStream` for `RSslLiveSession`.

**And the screen that WOULD have caught all three is build-free:** arm the dial
on the whole wave scope and run `regression-suite/run.sh` with
`CRATONVM_ARGS=--jdk-only`, against its own unarmed run on the same binary.
~20 minutes, versus a ~26-minute build plus a ~40-minute verification. On the
trimmed scope it reads:

```text
  unarmed control                  132 passed, 0 failed
  armed on the wave-1 scope        132 passed, 0 failed
```

Phase 2 already had this as its fourth precondition. §7 step 6 lists the corpus
*after* the build; it belongs in step 2 as well.

### 9.4 `RJdkSecurity` was a carrier defect, and it is fixed

`RJdkSecurity` reproduced under **no** dial scope, single or combined — §9.2's
blind spot exactly. What found it was not the dial and not the 13 probes but an
eight-line probe, `apps/probes/L4AbsPath.java`, printing path SHAPES:

```text
  temp.getPath          abs=true len=32 slashes=2      (correct)
  temp.isAbsolute       false                          (HotSpot: true)
  temp.getAbsolutePath  abs=true len=45 slashes=5      (the cwd, prepended)
```

Every `java.io.File` this VM builds kept its path in **slot 0** and wrote
nothing else. Self-consistent for exactly as long as this VM's own natives are
the only readers — §1.4's whole story — and over the moment real `File` bytecode
runs: `UnixFileSystem.resolve` reads `prefixLength`, never written and therefore
`0`, and calls every path relative. `javax.net.ssl.trustStore` then named a path
that no longer resolved, the JDK fell back to `cacerts` without throwing, and
the vector's one expected trust anchor became 122.

Fixed in the commit before this wave: `native-api/src/file_layout.rs`, the
shared carrier rule, applied at all **six** producing call sites across
`native-io` and `native-builtins`. Additive — slot 0 keeps the String, so the
twenty-two `read_file_path` readers are untouched. With it, the corpus is back
at 132/132 and the six `prefixLength`-dependent `File` rows (`isAbsolute`,
`getAbsolutePath`, `getAbsoluteFile`, `getCanonicalPath`, `getCanonicalFile`,
`toURI`) are retired in this table rather than carved out of it.

**Two lessons, and the second is the expensive one.** A fabricated carrier is a
STAMP and CONTENTS, and this lane keeps finding the contents empty — `Path`,
`CharBuffer`, `FileTime`, `FileSystemProvider` and now `File`, five for five.
And the first fix for this one measured *inert*: it was written into
`native-io`'s three `File` constructors, which are themselves retired by this
very table and so never run. The `File` under test came from `createTempFile`'s
producer in `native-builtins`. **A carrier rule belongs in `native-api`, beside
`path_layout` and `appended_slots`, and every producer has to be found first.**

**The instrument that would have attributed it in one run now EXISTS**, built
2026-09-11 off the back of this wave: `CRATONVM_UNRETIRE_NATIVE_SHADOW` turns
named rows of the retirement tables back off at runtime, so the bisection that
cost this wave three builds is a sequence of runs.

```text
  CRATONVM_UNRETIRE_NATIVE_SHADOW=all                          is it the wave?
  CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/                     which package?
  CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File                 which class?
  CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File.isAbsolute()Z   which row?
```

It resolves each rule against the tables **at arm time and prints the row
count**, because the failure this instrument would otherwise invite is the one
that makes bisection worse than useless: a mistyped rule matches nothing, the
vector still fails, and the reader concludes the triple is exonerated. A rule
reaching zero rows is named on stderr before any dispatch.

See [`crate::unretire`] (`native-api/src/unretire.rs`). The eighteen triples
this section leaves un-attributed for `RJdkSecurity` are now bisectable in
eighteen runs by anyone who wants the answer the carrier fix made moot.

### 9.5 What the residual work before it was

Wave 1 rests on three defects fixed on 2026-09-10 (`6f507bb48..44f28fe64`),
because the dial could not read a clean floor until they were:

* `FileInputStream.skip` was not an `lseek`, and `isRegularFile0` — the
  predicate deciding whether `skip0` runs at all — answered **false for every
  file**, having read an instance native's `args[1]` as its first argument. That
  is why two earlier, correct `skip0` bodies measured inert and were reverted;
* a synthetic `java.nio.file.Path` wrote its string to slot 0 and its filesystem
  to slot 1 on a two-slot object, where `UnixPath` is
  `fs(0) path(1) stringValue(2) hash(3) offsets(4)`. Now resolved by name
  against the implementation class, once per process, in
  `native-api/src/path_layout.rs`;
* a `ByteBuffer` view's read-only flag was read from the wrong carrier.

### 9.6 What is left

* **`java/io/PrintStream` (30).** Measured clean in the wave-1 dial screen and
  deliberately not in the table; §5 requires its own wave and its own commit.
  Read §9.2 and §9.3 before trusting that screen — and screen it against the
  corpus, where `System.out` actually lives.
* **479 rows no probe in this tree reaches**, minus the 146 wave 2 moved out of
  that bucket in one pass -- see §9.7, which is what "the cheapest half" was
  worth.
* **The four backed-out families that are left (34).** This said FIVE, and
  said all five were the same thing — a fabricated carrier whose real fields
  this VM never writes. Wave 4 took `java/nio/CharBuffer` (17) and found that
  claim false for it: the carrier was repaired on 2026-08-06 and the row that
  actually broke was a METHOD CONTRACT, `toString(int, int)`, held to two
  different index conventions by two natives that only ever talked to each
  other. §9.15 is what that looks like and §9.4 is what a carrier defect looks
  like; **read a family's own measurement before assuming which of the two it
  is.**
* **`java/nio/file/Path` (11) and, behind it, `FileSystemProvider`.** One defect
  and a measured order: the Path carrier is stamped with the INTERFACE, so
  `toString()` lands on `Object.toString()` and no `instanceof UnixPath` in the
  JDK's own `java.nio.file` code can succeed. Minting a concrete provider while
  that is true dies in `UnixPath.toUnixPath`'s `instanceof` (`L4FilesSweep`
  0 → 172). **Path first**, and it needs `concrete_receiver::alloc_concrete`
  *and* `mirror_class_registrations` together, plus every
  `class_name == "java/nio/file/Path"` test in `vm/` and `native-collections`.
* **`sun/nio/ch/` (308).** Package verdict upheld, not beaten.
* **`jdk/internal/foreign`.** Wave 2 took the value layouts (§9.7). What is
  left under it is the group layouts (35) on one named carrier defect, and the
  segment/arena/session half (70) on a decision that is on record and says they
  must not be retired at all.

---

### Wave 2 -- 2026-09-11, 137 rows over the nine `ValueLayouts$Of*Impl` carriers

`RETIRED_SHADOW_L4_FFM_TRIPLES`, under a new NARROW prefix
`jdk/internal/foreign/layout/`. The wave is 137 of 146 candidate rows; the
table's doc comment carries the arithmetic and the carve-out.

**One probe emptied the bucket.** `apps/probes/L4FfmLayoutSweep.java`, 359 rows,
reaches **all 146** registrations over the nine classes -- the census taken from
its own run reports `invocations > 0` with the schema-5 `invocations_complete`
bit set on every one of them. §9.1 put 479 rows in "no probe in this tree
invokes it" and said over half were this shape; that was a statement about the
instrument, and one afternoon of probe-writing was the whole cost of disproving
it.

### 9.7 The carrier was the blocker again, and the dial could not see it

Armed on the wave's scope, the probe went from **100 differing lines to 358**.
That is not a verdict on the retirement -- it is the second instance of the
finding §9.4 recorded for `java.io.File`, one family over:

```text
  bool.toString    THREW NPE: Cannot invoke "Class.descriptorString()" because "this.carrier" is null
  bool.name        null                              (HotSpot: Optional.empty)
  bool.varHandle   THREW NPE: Cannot invoke "Optional.isPresent()" because "this.name" is null
```

`jdk.internal.foreign.layout.AbstractLayout` declares `name` as an
`Optional<String>` and this VM wrote a bare `String` reference into it;
`ValueLayouts$AbstractValueLayout` declares `carrier`, a `Class`, and nothing
wrote it at all -- while the native `carrier()` derived its answer from the
class NAME, so the METHOD was right and the FIELD behind it was null. Fixed in
the commit before the table, at every mint and clone site in
`native-builtins/src/phases_late/foreign_ffm.rs`, through one reader and one
writer that know the convention. **Two defects that were live in both modes
went with it**, neither of them reachable by a retirement:

| | before | after |
|---|---|---|
| `JAVA_INT.withName("k").equals(...)` | `NullPointerException` | `true` |
| `JAVA_INT.withOrder(BIG_ENDIAN).toString()` | `i4` | `I4` |
| `withName` on a struct/sequence | `Optional[Optional[s]]`, 0 members | the name, the members |
| probe rows differing, `--jdk-only` | 50 | **25** |

The second row is this lane's own failure mode in the one method a caller reads
to find out what it is holding: the case of the letter IS the byte order, and
`order()` beside it answered correctly the whole time.

### 9.8 The wave is a wash on the probe, and that is the result

One binary against itself, dial scoped to exactly the wave
(`CRATONVM_ENFORCE_NATIVE_SHADOW=jdk/internal/foreign/layout/ValueLayouts$`):

```text
  unarmed --jdk-only          25 rows differ
  armed on this wave          25 rows differ
```

The same count, a different 25. Nineteen rows the native answered wrongly become
right, because the real bodies throw the JDK's own text
(`Invalid alignment: 3`, `Bad layout path: ...`) and render its own string
(`a8:i4`). Nineteen become wrong, and **every one of them is `varHandle`** --
the real `AbstractValueLayout.varHandle()` reaches
`Utils.makeSegmentViewVarHandle` and ends in `NoClassDefFoundError:
java/lang/invoke/BoundMethodHandle`. So `varHandle` is carved out, the table is
137 rows rather than 146, and the arm is the floor minus nineteen: **six rows,
none of them on a class in the table.**

Read that as the lane's own §9.2 warning applied in the other direction: an
armed arm that goes RED is not a verdict either. The first armed run of this
scope read 358 and the answer was a carrier defect; the second read 25 and the
answer was one method.

### 9.9 Acceptance

Every number below is on `vm-l4ffm-w2` against `vm-l4ffm-ctl`, the binary built
from the exact commit this branch forks from (`aba314446`), so the two differ by
this wave and the carrier fix and nothing else. **No timing claim anywhere:**
`RMapGcStress` read 146 s then 334 s on one binary, and this lane does not make
that mistake twice.

```text
  corpus, CRATONVM_ARGS=--jdk-only   control  133 passed, 0 failed
                                     wave     133 passed, 0 failed
  corpus, SUITE=all                  wave     133 passed, 0 failed
  corpus, SUITE=core                 wave      93 passed, 0 failed
  vectors differing between the two --jdk-only arms:  none
```

**Re-taken on the MERGE**, not carried over: `origin/dev` moved 49 commits
under this wave, two of them other lanes' retirement tables. The merged binary
(`0b491dce8`) reproduces the branch numbers to the line -- `L4FfmLayoutSweep`
12 differing lines in `--jdk-only` and 48 in compatible, `FfmSegmentSweep` 40,
the other two probes 0 -- and the `--jdk-only` corpus is 133 passed / 0 failed
on it as well. The kind-map amendment and the probe A/B above were taken on the
pre-merge binary and are unchanged by it; the stub ratchet was RE-MEASURED,
because its three baselines moved twice while this wave was in flight.

**137 refusals, 0 survivors**, and the control says why it mattered. Same probe,
`--jdk-only --explain-jdk-only --jdk-only-report`:

| | control | wave |
|---|---|---|
| report entries over these 137 triples | 256 | 137 |
| outcome | 137 `native-won`, 119 `bytecode-won` | all `synthetic-native-registered` |
| survivors | 0 | **0** |

Every one of the 137 was a native winning over real bytecode that was there all
along, and none has a survivor to fall through to.

**Kind map.** 101 rows amended by hand in
`scripts/baselines/jdk-only-kind-map-25-linux.tsv`, `bridge 0 1` ->
`synthetic-stub 1 1`, filtered by the control census: a row was edited only
where the wave's census disagrees with the baseline AND the control's still
agrees with it AND the triple is in the table. Zero rows outside the table.
`--update-baseline` was refused, because it would have blessed the 804 rows the
gate fires on for `dev`'s own binary. After the amendment both binaries report
the same two numbers -- **804 changed kind, 39 lost `kind_stated`** -- which is
where the gate stood before this wave.

The table has 137 rows and only 101 appear in that file: `byteSize`,
`byteAlignment`, `byteOffset` and `toString` on the nine classes have no row in
the baseline at all, in EITHER arm, so they are absent rather than changed and
adding them would be a re-freeze wearing an amendment's clothes.

**Stub ratchet**, both columns printed, OFF taken from
`CRATONVM_UNRETIRE_NATIVE_SHADOW` so one binary answers both halves:

```text
  arm             OFF             ON              delta
  (default)   2728 / 13609    2865 / 13609        +137 / 0
  management  2755 / 13977    2892 / 13977        +137 / 0
  synthetic   2728 / 13644    2865 / 13644        +137 / 0
```

OFF reproduces every constant in the file -- all three stub baselines and all
three totals -- and the totals do not move in either half: case (b) in the
ratchet's own taxonomy, existing registrations relabelled rather than new fakes
registered.

**Re-measured on the merge with `origin/dev` at `e240573a8`, not carried over
from the branch.** The three baselines moved under this wave twice while it was
in flight (2609 -> 2728 and 2620 -> 2755) and the `+137` is identical each time
only because it was measured each time rather than subtracted.

**A collapse detector whose doc said a retirement could not move it.**
`STRICT_MIN_TOTAL_REGISTRATIONS` in `native-builtins/tests/stub_ratchet.rs` went
red on this wave, and it is the one gate here that genuinely moved because of
it. Its comment says of the 2026-08-11 logging retirement that it *"moved this
total by zero: a re-tag changes a registration's KIND, it does not remove the
registration"*. That is true in COMPATIBLE mode, which is what was measured, and
false in the mode this whole campaign is about: under `JdkOnly`,
`register_inner` REFUSES a `SyntheticStub`, so **every retired triple is one row
fewer in the strict registry.** The floor tracks a number nine lanes are
deliberately driving down. Eight waves had taken it from 11,192 to 10,998 — 98
above the floor, not the 300 the comment believes it left — and 137 more crossed
it.

On the branch, before the merge, this wave crossed it: 10,998 -> 10,861 against
a floor of 10,900. **On the merge it does not, and the number is deliberately
left alone.** A sibling lane hit the same wall the same day and lowered the
floor 10,900 -> 10,600 for its own wave, and 137 rows now read:

```text
             compatible   stubs   strict   dropped   refusals
   OFF         13609       2728   10878     2731      2749
   ON          13609       2865   10741     2868      2886
   delta           0       +137   -137      +137      +137
```

Exactly the table's row count in every column that moves, zero in the one that
must not, and a corpus that does not move at all.

**And then the gate stopped being a level.** While this wave was landing, the
stub-ratchet lane replaced `STRICT_MIN_TOTAL_REGISTRATIONS` with
`STRICT_UNEXPLAINED_DROP_MAX` -- *"bound the strict registry's SHORTFALL, not
its level: the number it guarded is designed to fall"* -- which is the same
conclusion from the other end, and a better instrument than the correction this
wave was going to leave behind. This wave is its confirmation:

```text
  shortfall = compatible - stubs - strict
  OFF   13609 - 2728 - 10878 = 3
  ON    13609 - 2865 - 10741 = 3
```

**Invariant across 137 retirements**, where the level moved by exactly 137.
A campaign-wide quantity wants a gate on what should NOT change, not on what
every lane is paid to reduce.

### 9.10 The residual, in the terms §9.5 uses

**Compatible mode keeps a second copy of the mint that this fix does not
reach.** `make_prepared_value_layout` in `vm/src/vm/vm_util.rs` seeds
`ValueLayout.JAVA_INT` and its fifteen siblings before `<clinit>`; it resolves
`byteSize`, `byteAlignment` and `name` by name and writes **three of the five**
real fields, leaving `carrier` and `order` null. `--jdk-only` drops that preseed
entirely and runs the real `<clinit>`, which is why the strict arm is at 25 rows
and the compatible one at 34. It does not touch this wave:
`NativeKind::allowed_in(Compatible)` is `true` for every kind, so a retired
triple still dispatches its native in compatible mode.

**The group layouts (35) are one carrier defect, not thirteen.**
`AbstractGroupLayout.elements` is declared `java.util.List<MemoryLayout>` and
this VM stores a java ARRAY there. Armed, `memberLayouts()` answers 0 where the
oracle answers 2, `byteOffset(groupElement("c"))` cannot resolve a member that
is plainly there, and every group `toString` dies in `NoSuchMethodError: 'int
java.lang.foreign.MemoryLayout.size()'`. Same shape as the defect this wave
fixed, same treatment -- a real `List`, written at the mint -- and it is the
cheapest next wave in the lane for the same reason this one was. **Taken as
wave 3 the following day; see below.**

**The segment, arena and session carriers (70) are not a wave.**
`docs/known-issues/jdk-only/the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`
decides that `cratonvm/internal/foreign/MemorySegmentImpl` is the VM's own
allocation shape and is laid out deliberately unlike
`AbstractMemorySegmentImpl`, whose `length`/`readOnly`/`scope` would alias the
carrier's `ptr`/`size`/`arena`. Retiring one runs a real body over those three
slots. Nothing to screen while that decision stands.

That decision's own §3 asked for three things from "the layout half", and the
first two are now done by a route it did not anticipate: the layout carriers are
minted on their REAL JDK classes rather than on a
`cratonvm/internal/foreign/LayoutImpl`, so `getClass().isInterface()` is false
for both of its check rows and `FfmSegmentSweep`'s other 23 are unchanged at 40
lines over 20 name pairs.

---

### Wave 3 -- 2026-09-12, 28 rows over the four GROUP carriers

`RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES`: `StructLayoutImpl`, `UnionLayoutImpl`,
`SequenceLayoutImpl`, `PaddingLayoutImpl`, under the prefix wave 2 admitted. No
new prefix, because the prefix was never the decision.

### 9.11 The carrier was the blocker for the third wave running

`jdk.internal.foreign.layout.AbstractGroupLayout` declares
`List<MemoryLayout> elements` and this VM stored a bare ARRAY in it; `kind` and
`minByteAlignment`, the other two fields it declares, were never written at all.
`memberLayouts()` is `return elements;` -- one `getfield` -- so the first real
body to touch a group got an array where the JDK's own code calls `List` methods:

```text
  struct.memberLayouts().size()     0        HotSpot: 2
  struct.byteOffset(groupElement)   cannot resolve a member plainly there
  struct.toString()                 NoSuchMethodError: MemoryLayout.size()
  struct.equals(struct)             NullPointerException
```

Third instance of one shape in three waves -- `java.io.File`'s `prefixLength`
(§9.4), `AbstractLayout`'s `Optional<String> name` (§9.7), now `elements` -- and
the same fix each time: **one reader, one writer, routed through every site that
mints or walks the carrier.** The pattern is worth naming, because the lane has
now paid for it three times: *a carrier minted on its REAL class must hold what
that class DECLARES, in the declared TYPE, at every field the real bodies read.*
Resolving the slot by name gets the index right and says nothing about the type.

Three decisions the measurement made rather than taste:

* **The member COUNT travels beside the array.** An `ArrayList` has capacity
  past its size, so reading `array_length` off `elementData` invents trailing
  null members -- a group that grows silently, which is §4's failure mode.
* **The reader asks `object_is_array`, not a field name.** A reference array's
  header class id is its COMPONENT's, so the payload cannot be asked its own
  name, and `AbstractGroupLayout` itself declares `elements` -- a name probe can
  answer yes for entirely the wrong reason.
* **The list is UNMODIFIABLE.** HotSpot throws `UnsupportedOperationException`
  from `memberLayouts().add(...)`, and since the accessor returns the field
  itself, an `ArrayList` hands a caller a mutable view of a layout's members.
  The writer mints `ImmutableCollections$ListN`, whose `size()` IS
  `elements.length` -- no second count to disagree with.

### 9.12 The two modes disagreeing is what found the last defect

```text
  structLayout(..).withName("st").memberLayouts().size()
    --jdk-only   2        compatible   0
```

`resolve_field_index` resolves a class GLOBALLY BY NAME and answers `None` for
one not yet loaded -- which under compatible mode `ImmutableCollections$ListN`
is not, because nothing has called `List.of` by then. The mint fell back to the
mutable shape, and `AbstractGroupLayout`'s constructor then `List.copyOf`'d it
into a `List12` on the next `withName`: a shape with `e0`/`e1` and **no backing
array at all**, which the reader cannot decode, so the group reported zero
members. Loading the class first fixes both modes.

**An A/B that runs only one mode would not have seen it**, and the lane page has
said to run both since wave 1 without saying why. This is why.

### 9.13 Acceptance

`apps/probes/L4FfmLayoutSweep.java`, now 390 rows, oracle stable over three
captures. Control is `vm-l4ffm-w3ctl`, built from `origin/dev` at `52113652d` in
a separate checkout so the wave tree was never disturbed.

```text
  L4FfmLayoutSweep, --jdk-only        rows differing
    control (origin/dev)                  13
    carrier fix, group table un-retired    3
    carrier fix + the table                2
```

The un-retired arm is the SAME BINARY with
`CRATONVM_UNRETIRE_NATIVE_SHADOW` naming the four classes; its arm report prints
`7 + 7 + 8 + 6 = 28 table row(s)`, which is the receipt that it armed this table
and not its neighbour. The retirement fixes one row -- `byteOffset` on a padding
layout, which now throws the JDK's own message -- and breaks none.

The two rows left are neither this wave's nor a group's, and both are
`varHandle`, carved out of this wave as of wave 2: `ADDRESS.varHandle().varType()`
answers `long` where the oracle says `MemorySegment`, and a union's `varHandle`
**accepts a misaligned access HotSpot refuses**. That second one is a MISSING
REFUSAL, which is §4's failure mode again, and it is now the only one of its kind
left in the FFM layout surface.

**28 refusals, 0 survivors.** On the control all 28 report `native-won`: a
native winning over real bytecode that was there the whole time.

The corpus does not move:

```text
  CRATONVM_ARGS=--jdk-only   control  133 passed, 0 failed
                             wave     133 passed, 0 failed
  SUITE=all                  wave     133 passed, 0 failed
  SUITE=core                 wave      93 passed, 0 failed
```

and the two `--jdk-only` arms differ on **no vector at all**, compared row by
row rather than by their totals.

**Stub ratchet, both columns printed, +32 against a 28-row table.** The unit
here is one REGISTRATION and the table's unit is one triple: `byteSize` and
`byteAlignment` on `StructLayoutImpl` and `UnionLayoutImpl` are each registered
twice, from two neighbouring loops in `foreign_ffm.rs`, so one shadows the other
and both are re-tagged. Same distinction wave 2's `51 table row(s)` against 71
moving registrations made.

```text
  arm             OFF             ON              delta
  (default)   2884 / 13617    2916 / 13617        +32 / 0
  management  2911 / 13985    2943 / 13985        +32 / 0
  synthetic   2884 / 13652    2916 / 13652        +32 / 0
```

Re-measured on the merge with `origin/dev`, not carried over: the three stub
baselines moved under this wave while it was in flight (2865 -> 2884,
2892 -> 2911), so the `+32` is identical each time only because it was measured
each time rather than added.

The OFF column names the four CLASSES, not the prefix: the prefix would have
un-retired wave 2's 137 value-layout rows as well, and **an OFF column that
undoes a neighbour's wave is not this wave's before-number.** That distinction
cost nothing to make and would have read as a 24-row improvement wrongly
credited here.

**Kind map: no amendment, and that is checkable.** Both binaries report the same
852 flips, and all 28 of this table's triples have ZERO rows in
`scripts/baselines/jdk-only-kind-map-25-linux.tsv` — the file carries wave 2's
101 `ValueLayouts` rows and none at all for the four group classes. So the gate
is red for `dev`'s own reasons, exactly as red as before, and this wave is not
among them.

Compatible mode is 28 rows against strict's 2, and every one of the 26 is
downstream of the preseed §9.10 names: comparing two structs compares their
MEMBERS, and the members are the preseeded `ValueLayout` constants whose
`carrier` `make_prepared_value_layout` never writes. Strict drops that preseed
and runs the real `<clinit>`, which is the whole of the gap.

---

### Wave 4 -- 2026-09-12, 17 rows over `java/nio/CharBuffer`

`RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES`. No new prefix: `java/nio/` was admitted
by wave 1, which retires this class's 34-row `java/nio/ByteBuffer` twin under
it. The prefix was never the decision.

### 9.14 The family §9.2 backed out, and what re-measuring it found

§9.2 lost this family on wave 1's first build: armed, `subSequence(1, 3)`
answered `cd` where the oracle says `bc`, and the lane page wrote that down as a
carrier whose `position`/`limit` the real accessors could not read. **The
carrier is fine.** `native-builtins/src/phases_late/charset_buffers.rs` mints
`java/nio/HeapCharBuffer` -- the real CONCRETE class -- at every producer, and
has since `4ba4f312b` (2026-08-06, *"CharBuffer.wrap stamped the abstract class,
so subSequence checked nothing"*), five weeks before §9.2 was written.

So the first thing wave 4 measured was the family as it stands, against
`apps/probes/L4CharBufferSweep.java` and HotSpot 25 on linux/x86_64:

```text
  L4CharBufferSweep, 261 rows                       rows differing
    control (origin/dev bdb02d94e), --jdk-only            0
    control, compatible                                   0
    control, --jdk-only, dial armed on the family         0
```

Three arms clean, the dial reporting `reached=1913 yielded=1913 leaked=0`. On
that evidence the wave is a pure §1.4 shadow removal: seventeen natives in
front of real bodies that answer identically.

**It is not, and the dial is why.** §9.2's own rule -- *use the dial to BOUND a
wave, never to certify one* -- earns its keep here in the direction that is
hardest to see: the dial's decline is conditional, `subSequence` and
`toString(int, int)` are ABSTRACT on `java/nio/CharBuffer` and so can never be
declined, and those two are exactly where the defect lives. A table has no such
fallback. Retired, the same probe moves **twelve rows**.

### 9.15 `toString(int, int)` had two conventions, and owned both ends of each

The twelve are one defect, bisected to one row in one pass with
`CRATONVM_UNRETIRE_NATIVE_SHADOW` -- seventeen runs, sixteen of them still at
12 diffs and `toString()Ljava/lang/String;` alone at 0.

The JDK's `CharBuffer.toString()` is one line, `toString(position(), limit())`,
and every real `toString(int, int)` under it takes ABSOLUTE buffer indices:
`HeapCharBuffer` is `new String(hb, start + offset, end - start)`,
`StringCharBuffer` is `str.subSequence(start + offset, end + offset)`. This VM
had TWO conventions for that one method, picked by the receiver's class name:
absolute for a `StringCharBuffer`, and **relative to the position** for
everything else -- with its own `toString()` passing `0, limit - position` to
the second one to compensate.

```text
  b = allocate(6); put("abcdef"); clear(); position(2); limit(5)

    HotSpot                b.toString()  ->  toString(2, 5)  ->  "cde"
    retired, pre-fix       b.toString()  ->  toString(2, 5)  ->  hb[2+2 .. 2+5]  ->  "ef"
```

Twelve rows, every one of them off by exactly `position`: `subSequence`
followed by `toString`, a windowed `toString`, the reflective route, a
read-only window, a `slice`'s window, a `wrap(char[], int, int)`'s window, and a
lone surrogate whose window moved off it.

**This is the lane's oldest lesson arriving in a new shape.** Three waves have
now paid for *a carrier minted on its real class must hold what that class
declares, in the declared type*. The general form is about agreement, not about
fields: **a native that owns both ends of a convention agrees with itself
whatever the convention is, and only real bytecode is a second opinion.** Here
the convention was an argument's meaning rather than a field's type, and the
retirement is what put a real body on one end of it.

The receiver-class split went with the fix. It was added in August so that the
reflective route -- which resolves `toString()` against the DECLARING class --
and a bytecode `invokevirtual` would agree on a `StringCharBuffer`; with one
convention there is nothing left for them to disagree about. `ts.wrapSR
.reflective` and `ts.heap.reflective` are the two probe rows that hold both
routes to it.

### 9.16 Acceptance

`apps/probes/L4CharBufferSweep.java`, 261 rows, oracle stable over three
captures. Four arms, three binaries, all built from this worktree:

```text
  L4CharBufferSweep, --jdk-only, 261 rows            rows differing
    A  control (origin/dev bdb02d94e)                      0
    B  the table, WITHOUT the fix                         12
    C  the fix + the table                                 0
    D  the fix, table un-retired (SAME binary as C)        0
```

Arm D is C with `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/CharBuffer`, whose arm
report prints `17 table row(s)` -- the receipt that it armed this table and not
a neighbour under the same prefix. Compatible mode is 0 on the control and 0 on
the wave, which is what a mode-blind re-tag should do: `SyntheticStub` is
allowed in compatible mode, so the native still wins there and nothing moves.

**The funnel: 17 of 17, none cold.** Every row is invoked by the probe under
`--nojit CRATONVM_DISABLE_INTRINSICS=1`, which is what makes the census's
`invocations_complete: true` mean what it says. `session()` and `checkSession()`
are package-private `java.nio.Buffer` internals no probe can call by name; they
are reached 5 and 149 times as callees, which is how the JDK reaches them too.

`hasArray()Z` was the one row the funnel nearly lost, and for a reason worth
recording: called through a lambda it answered correctly with `invocations: 0`
-- the answer came from somewhere the registry never saw -- and called plainly
off a local the same triple reads 5. The probe now asks in both shapes
(`direct()`), because a funnel that cannot show a row was invoked must not take
that row.

**Eighteen rows clear the §1.4 bucket test and seventeen are in the table.**
`charAt(I)C` is the eighteenth and is carved out by the re-tag's own condition:
`NativeMethodRegistry::register` re-tags a retired triple only when the
effective category is `Bridge`, and `charAt` is registered as an `intrinsic`. A
row for it would be inert by construction, which is a different reason from the
FFM waves' `varHandle` carve-out and is asserted separately.
