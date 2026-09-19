# Lane 4 — `java.io`, `java.nio`, `sun.nio`, and the FFM internals

**Scope: 1,110 §1.4 shadows over 131 classes, from 615 registration sites.**
Prefixes: `java/io/`, `java/nio/`, `sun/nio/`, `jdk/internal/foreign`.
The largest prefix lane, and the one with the highest blast radius.

Read [`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md)
§2b first — ownership, shared cells, the build queue and the merge protocol,
rehomed there on 2026-09-12 from the lane-0 page. Method, preconditions and
landing protocol: the same page, §1, §7 and §5.

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

> **Taken as wave 6 on 2026-09-12, with `java/io/PrintWriter`'s seven.**
> Both rules below were followed and both are recorded in §9.21–§9.25.
> The count: the class carries **31 registrations** — 29 bucket-A rows
> (retired), one `Intrinsic` (`charset()`, where a table row would be
> inert), and one declared by **no** supported image
> (`write(String,int,int)`, where a table row is a `NoSuchMethodError`).
> §1 said 29 and §9.1 said 30; they were counting different two of
> those three things.

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

`RETIRED_SHADOW_L4_TRIPLES` in `../../../native-api/src/retired_shadow.rs`, whose doc
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
| ~~deferred: `java/io/PrintStream`, its own wave (§5)~~ **RETIRED — wave 6**, 29 of the class's 31 registrations (§9.21) | **29** |
| **backed out by a build** (§9.2; CharBuffer's 17 taken as wave 4, 27 more as wave 5 — all that is left is `FileSystemProvider`'s 4 and the three `ByteArrayInputStream` observers of §9.19) | **7** |
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
on the whole wave scope and run `../../../regression-suite/run.sh` with
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
eight-line probe, `../../../apps/probes/L4AbsPath.java`, printing path SHAPES:

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

Fixed in the commit before this wave: `../../../native-api/src/file_layout.rs`, the
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

See [`crate::unretire`] (`../../../native-api/src/unretire.rs`). The eighteen triples
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
  `../../../native-api/src/path_layout.rs`;
* a `ByteBuffer` view's read-only flag was read from the wrong carrier.

### 9.6 What is left

* ~~**`java/io/PrintStream` (30).**~~ **Taken as wave 6 on 2026-09-12**, with
  `java/io/PrintWriter`'s seven rows beside it — one registrar,
  `register_printstream_fallback_natives`, holds both. The advice above was
  right and was followed: the wave-1 dial screen was not trusted, and the
  corpus was screened. What this bullet could not say is that the class had
  **already been adjudicated**, triple by triple, on 2026-08-11, in
  `../../jdk-only/W7-22-shadow-retirement-logging-and-time.md` —
  a document this page never cites. That write-up blocked `PrintStream` on
  five named things and cleared `PrintWriter` outright, and it was never
  landed because its author was on Windows, where the `<jdk>/<os>`-keyed gates
  exit 2. **Before measuring a family from scratch, grep the whole
  `` tree for its class name**; §9.21 is what that
  grep was worth here.
* **479 rows no probe in this tree reaches**, minus the 146 wave 2 moved out of
  that bucket in one pass -- see §9.7, which is what "the cheapest half" was
  worth.
* ~~**The four backed-out families that are left (34).**~~ **Taken as wave 5
  on 2026-09-12, except the provider.** This said FIVE, and said all five were
  the same thing — a fabricated carrier whose real fields this VM never writes.
  That claim is now false for four of the five. Wave 4 took
  `java/nio/CharBuffer` (17) and found a METHOD CONTRACT, `toString(int, int)`,
  held to two different index conventions by two natives that only ever talked
  to each other. Wave 5 took the other three (27 rows) and found: a group whose
  blocker had been FIXED two days before it was written down (§9.17), a READER
  that assumed a unit (§9.18), and a COUPLING that is not a defect at all
  (§9.19). §9.15 is what a contract defect looks like and §9.4 is what a
  carrier defect looks like; **read a family's own measurement before assuming
  which of the two it is — and re-read it after anything nearby is fixed.**
* **`java/nio/file/Path` (11) and, behind it, `FileSystemProvider` — which is
  now the ONLY backed-out family left, and is **nine** bucket-A/B rows rather
  than the four §9.2 counted (the population grew; re-census before quoting a
  family's size).** One defect
  and a measured order: the Path carrier is stamped with the INTERFACE, so
  `toString()` lands on `Object.toString()` and no `instanceof UnixPath` in the
  JDK's own `java.nio.file` code can succeed. Minting a concrete provider while
  that is true dies in `UnixPath.toUnixPath`'s `instanceof` (`L4FilesSweep`
  0 → 172). **Path first**, and it needs `concrete_receiver::alloc_concrete`
  *and* `mirror_class_registrations` together, plus every
  `class_name == "java/nio/file/Path"` test in `../../../vm` and `native-collections`.
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

**One probe emptied the bucket.** `../../../apps/probes/L4FfmLayoutSweep.java`, 359 rows,
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
`../../../native-builtins/src/phases_late/foreign_ffm.rs`, through one reader and one
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
`../../../scripts/baselines/jdk-only-kind-map-25-linux.tsv`, `bridge 0 1` ->
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
`STRICT_MIN_TOTAL_REGISTRATIONS` in `../../../native-builtins/tests/stub_ratchet.rs` went
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
reach.** `make_prepared_value_layout` in `../../../vm/src/vm/vm_util.rs` seeds
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
`the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`
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

`../../../apps/probes/L4FfmLayoutSweep.java`, now 390 rows, oracle stable over three
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
`../../../scripts/baselines/jdk-only-kind-map-25-linux.tsv` — the file carries wave 2's
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
carrier is fine.** `../../../native-builtins/src/phases_late/charset_buffers.rs` mints
`java/nio/HeapCharBuffer` -- the real CONCRETE class -- at every producer, and
has since `4ba4f312b` (2026-08-06, *"CharBuffer.wrap stamped the abstract class,
so subSequence checked nothing"*), five weeks before §9.2 was written.

So the first thing wave 4 measured was the family as it stands, against
`../../../apps/probes/L4CharBufferSweep.java` and HotSpot 25 on linux/x86_64:

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

`../../../apps/probes/L4CharBufferSweep.java`, 261 rows, oracle stable over three
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


---

### Wave 5 -- 2026-09-12, 27 rows over the three remaining backed-out families

`RETIRED_SHADOW_L4_BACKED_OUT_TRIPLES`, eight classes of `java/io/`,
`java/nio/channels/` and `java/nio/file/attribute/`. No new prefix: wave 1
admitted both of the packages these sit under, so the table is the whole of the
decision. Probe: `../../../apps/probes/L4W5Sweep.java`, 183 rows.

The wave lands **with a fix**, not as a pure shadow removal: `RFileTimes` and
`RSslLiveSession` -- the two vectors §9.2 named -- both failed on the first
build of the table, and only one of the two was a defect.

### 9.17 A family can be blocked on a defect that is already fixed

The file-handle group (9) -- `java/io/FileOutputStream`, `FileCleanable`,
`FileDescriptor` and the abstract-receiver `java/nio/channels/FileChannel` --
went out of wave 1 as an **un-attributed GROUP**, because `RJdkSecurity`
reproduced under no dial scope at all and nothing narrower could be said.

§9.4 then found that vector's cause and fixed it: every `java.io.File` this VM
built kept its path in slot 0 and wrote nothing else, so `prefixLength` read 0
and `UnixFileSystem.resolve` called every path relative. **Nobody re-tried the
group.** It is green here in every arm -- probe, corpus, both modes -- and its
entire blocker was a defect in a different class that had been closed for two
days when the family was written down as blocked.

That is a third way for a recorded cause to be wrong, beside §9.15's (the
mechanism was misdiagnosed) and §9.2's (the carrier had been repaired): **the
cause was right, and then someone fixed it somewhere else.** A backed-out list
is a snapshot of a tree, and this lane has now been wrong about one in three
entries on it. Re-measure a blocked family whenever anything it touches moves.

### 9.18 `filetime_read_millis` assumed a unit, and the setter stamped 1970

`RFileTimes` failed on `java/nio/file/attribute/` alone, reproduced a row at a
time. §9.2's attribution was right; its MECHANISM -- "reads `FileTime.toMillis()`
off a fabricated carrier" -- was not. The carrier is fine. The READER is not:

```rust
pub(crate) fn filetime_read_millis(ctx: &dyn NativeContext, ft: ObjectRef) -> i64 {
    if let Value::Long(v) = ctx.get_field_by_name(ft, "value") { return v; }   // millis?
    ...
}
```

`FileTime` stores a PAIR, and `value` alone is not a time. `FileTime.from(Instant)`
compiles to `new FileTime(0L, null, instant)` -- identically on 17, 21 and 25,
`javap -c` -- so `value` is a literal `lconst_0` and the time lives in
`instant`. The reader answered **0** for every `FileTime` real bytecode had
built, and `BasicFileAttributeView.setTimes` stamped the file at the epoch:
`plain.readAttributes.lastModified 1970-01-01T00:00:00Z`, four rows of it.

It was self-consistent for exactly as long as this VM was the only PRODUCER --
`filetime_alloc` converts to millis before storing and writes
`unit = MILLISECONDS`, so the writer and the reader agreed on a convention that
appears nowhere in the class. **This is §9.15's finding in a second place, and
the generalisation holds: a native that owns both ends of a convention agrees
with itself whatever the convention is; only real bytecode is a second opinion,
and a retirement is how you ask for one.**

Fixed rather than carved out. The unit is honoured, `unit == null` falls back to
the `instant` fields, and both legacy shapes still read. It cannot ask
`TimeUnit.toMillis` through the VM -- `filetime_read_millis` IS the body
registered for `FileTime.toMillis()`, so dispatch would re-enter it in
compatible mode -- so the constant is identified by its `Enum.name` rather than
by an ordinal slot read, which is a defect this same file has already shipped
once (`posix_file_permission_stub_clinit`).

### 9.19 A row can be correct, and still not retirable

`RSslLiveSession` failed on `java/io/ByteArrayInputStream` alone, and **nothing
about those rows is wrong**. They answer as the real bodies do. Three of them --
`read()I`, `read([BII)I` and `close()V` -- are the only three sites in the VM
that dispatch `BaisEvent`, which is how this VM makes HotSpot's keep-alive
DRAIN INSTANT observable: at body EOF and at close the `https` carrier is
recycled, and every connection-level accessor goes back to throwing
`IllegalStateException: connection not yet open`.

`drainTrap` drains with the no-arg read and then closes. Retiring both rows
removed both observation points, the connection was never recycled, and
`getCipherSuite()` answered where HotSpot throws. **Un-retiring EITHER row alone
clears the vector**, which is why a single-row bisect reports two causes for one
failure and why the conjunction has to be read rather than the first PASS.

Carved out, six of nine retired. `read([BII)I` is carved out too, on no vector's
evidence: it carries the same observer for a caller that drains with the
three-argument read, and shipping it because nothing went red is how the same
defect arrives with nothing to find it.

**This is a new kind of blocker for this lane** -- not a carrier, not a
contract. A general-purpose class's native carries a side effect that a
different subsystem depends on, and the retirement machinery cannot see it: the
census says `bridge`, the image says `has_code`, the probe agrees row for row,
and the only instrument that knows is a corpus vector about TLS sessions.

**Nomination.** Move the observation to a stream the HTTP layer owns. HotSpot's
`getInputStream()` does not hand back a `ByteArrayInputStream` either -- it
returns a `HttpInputStream`/`KeepAliveStream` wrapper -- so a VM-owned wrapper
would be both more faithful and un-retirable by anyone else's wave. Until then
these three rows stay, and this section is why.

### 9.20 Acceptance

**Four arms, and the wave MOVES the probe toward HotSpot.** Diffed against a
HotSpot 25 oracle rather than against the control alone:

```text
  L4W5Sweep, --jdk-only, 183 rows            rows differing from HotSpot
    A  control (origin/dev f99c2e748)                 8
    B  this wave                                      4
    C  this wave, compatible mode      0 differing FROM THE CONTROL
    D  this wave + UNRETIRE, same binary   0 differing FROM THE CONTROL
```

Arm D prints `27 table row(s)` over its eight rules at arm time -- the receipt
that B's movement is this table's and not a neighbour's under a prefix that
admits the whole of `java/io/`. The four rows it moves are four silent wrong
answers:

```text
  FileTime.from(..., NANOSECONDS).toString()    .123Z        -> .123456789Z
  FileTime.from(..., NANOSECONDS).to(NANOS)     ...123000000 -> ...123456789
  PosixFilePermissions.fromString("rwx")        a Set        -> IAE
  FileChannel.isOpen() after the stream closed  true         -> false
```

The last is §4's species. The receiver is a REAL `sun.nio.ch.FileChannelImpl`
in all three arms: a native registered on `java/nio/channels/FileChannel` won
the door for it and answered from this VM's fd table rather than from the
`closed` field real `close()` had just set.

**The corpus found both defects and the probe tree found neither.** The probe
was clean on the first build of this table; `RFileTimes` and `RSslLiveSession`
were not. Attribution cost six runs to a family and nine more to a row, with
`CRATONVM_UNRETIRE_NATIVE_SHADOW` and no rebuild -- the instrument §9.4
nominated, doing the job §9.4 wanted it for.

```text
  CRATONVM_ARGS=--jdk-only   control  136 passed, 0 failed
                             wave     136 passed, 0 failed
  SUITE=all                  wave     136 passed, 0 failed
  SUITE=core                 wave      95 passed, 0 failed
```

The two `--jdk-only` arms were compared VECTOR BY VECTOR, not by totals: 136
lines each, zero differing. `TIMEOUT=600`; no timing claim is made from any arm.

**The funnel: 27 of 27.** One row, `FileOutputStream.write([BII)V`, reports
`invocations_complete: false`, so precondition 4 rests on its `outcome`
instead -- `native-won` in the control's report. The strict registry drops 33
registrations over the 27 triples, none of them `native-won` in the wave's
report against 22 in the control's.

**+33 in three arms, paired on ONE binary:**

```text
  arm             OFF             ON              delta
  (default)   3979 / 13590    4012 / 13590        +33 / 0
  management  4006 / 13958    4039 / 13958        +33 / 0
  synthetic   3979 / 13625    4012 / 13625        +33 / 0
```

27 rows and +33 registrations is the unit, not a discrepancy: six triples are
registered twice and the re-tag flips each registration. Measured on the
pre-merge tree at `3944/3971/3944 -> 3977/4004/3977` and AGAIN above, after
lane 5's third residual wave landed on the same three constants in between --
the same +33 in all three arms, on trees 35 registrations apart, and this
wave's OFF column is lane 5's ON column exactly. Totals identical in both columns, and +1
on the `MEASURED_TOTAL_REGISTRATIONS_*` constants -- which the note above them
already recorded and left one low; re-frozen here to the measured figures.

**Kind map: 32 rows amended**, `bridge -> synthetic-stub`, 25/linux, written
rather than regenerated. 32 and not 33 because `PosixFilePermission.valueOf` is
absent from the baseline altogether, and this gate passes and REPORTS a new row.
After the amendment the wave fires 1 050 flips -- exactly the control's count --
and names no row of this table.

Measured on linux/x86_64 against JDK 25.

---

### Wave 6 -- 2026-09-12, 36 rows over `java/io/PrintWriter` (7) and `java/io/PrintStream` (29)

`RETIRED_SHADOW_L4_PRINTWRITER_TRIPLES` and
`RETIRED_SHADOW_L4_PRINTSTREAM_TRIPLES`, two tables under the `java/io/` prefix
wave 1 already admitted. This is the wave §5 reserved, and §5's reason for
reserving it is the first thing this section has to answer.

**The blast radius, named as §5 requires.** `System.out` and `System.err` are
how all nine lanes read their probes, and the failure mode is silent rather
than loud: real `PrintStream.writeln` catches its own `IOException` and sets
`trouble = true`, so a stream whose state is not real DISCARDS. A regression
here reads as every lane's probes going empty at once, with exit code 0, and
the harness gets the blame. Three things were built against that and all three
are in the commit: both probes now report through a `FileOutputStream` as well
as through `System.out` (`L4W6PrintCarrier` is new, `L4PrintStreamSweep` gained
a second channel); the tables are separate so either half can be disarmed at
run time; and `the_console_write_path_is_retired_only_by_the_wave_that_measures_it`
guards `OutputStreamWriter`/`BufferedWriter`/`Writer` across EVERY table in the
tree, so the next wave cannot take the console path as a by-product of
something else.

### 9.21 A blocked list can decay in four places at once

`../../jdk-only/W7-22-shadow-retirement-logging-and-time.md` is the
authority here and it is a good document: one registrar,
`register_printstream_fallback_natives`, holding two classes, measured triple
by triple on 2026-08-11 against HotSpot 25, `PrintWriter` retirable in four
arms and `PrintStream` blocked on five named things. It was never landed, and
§2.1 says why in its own first line: **"the blocker is a platform, not a
question."** The three frozen artefacts are keyed `<jdk-feature>/<os>`, the
gate scripts derive the OS half from the running host, and on Windows they look
up `25/windows`, find nothing and exit **2** -- neither a pass nor a fail. This
wave ran on the Linux host against the JDK 25 image, where that blocker is
simply absent.

Re-read a month later, the five-item blocked list holds in one place:

| item | 2026-08-11 | 2026-09-12 |
|---|---|---|
| 1. `System.out` fabricated: `out`, `charOut`, `textOut`, `closeLock` null | true | `out`/`charOut`/`textOut` are **constructed** by `install_real_stream_fields`; `closeLock` was still null and **is fixed in this commit** |
| 2. `charset` is the ABSTRACT `Charset` | true | **`sun.nio.cs.UTF_8`, concrete** |
| 3. `native_printstream_init_outputstream` does not chain to a real ctor | true | **still true, and made irrelevant**: this wave retires BOTH constructors with the methods |
| 4. `write(String)` is package-private | -- | **`private`**, in all three images; retired with the family either way |
| 5. `write(String,int,int)` is declared by no image | true | **confirmed on 17, 21 and 25**; held back by name and asserted |

Items 1 and 2 were repaired by someone else, in `lang_system.rs`, in between --
**the third time this lane has found a family's recorded blocker already fixed
somewhere else** (§9.4, §9.17). Item 3's repair was never needed, because
retiring the constructors removes the native that needed it. Item 4 was wrong
in a detail that did not change its disposition. Only item 5 survived intact,
and it is the one that says a row must NOT be retired.

### 9.22 The BLOCKED half is the half that fixed things

The wave was built expecting to land `PrintWriter` and to measure
`PrintStream`. It came out the other way round. One binary, the two halves
disarmed independently at run time with `CRATONVM_UNRETIRE_NATIVE_SHADOW` --
which is the whole reason they are two tables and not one:

```text
  arm (--jdk-only)         L4W6PrintCarrier vs HotSpot 25, of 66 rows
  neither retired           24 lines differing
  PrintWriter only          24 lines differing
  PrintStream only           4 lines differing
  both                       4 lines differing
```

`PrintWriter`'s seven rows are verdict-neutral -- which is what 2026-08-11 said
and is why they can be landed on that evidence, re-measured here. **The
twenty-nine `PrintStream` rows are worth ten rows of agreement with HotSpot**,
all of them state on a user-constructed stream: `charOut`, `textOut`, `charset`
and `closeLock` on receivers built by `new PrintStream(sink)` and
`new PrintStream(sink, true)` read null, because
`native_printstream_init_outputstream` writes `out`, `lock` and `autoFlush` and
stops. Retiring the constructors hands construction back to the JDK and all
four fields arrive.

That is §4's silent wrong answer with the volume turned all the way down: the
old receiver still PRINTED correctly, because the shadowing methods read the
one field the shadowing constructor wrote. Native agreed with native; neither
agreed with the image. It is the same shape as wave 4's `toString(int,int)` and
wave 5's `filetime_read_millis` -- **a native that owns both ends of a
convention agrees with itself whatever the convention is** -- and the third
time this lane has met it, which is enough to stop calling it a coincidence.

### 9.23 `closeLock` was null because nothing ran an instance initialiser

The last live item on W7-22's blocked list, and the one thing in this commit
that is not a retirement. `FilterOutputStream.closeLock` is
`private final Object closeLock = new Object()` -- an INSTANCE INITIALISER.
`System.out` is allocated and then field-stuffed, so no initialiser ever runs,
and `install_real_stream_fields` had installed the three fields that have
visible wiring (`out`, `charOut`, `textOut`) and missed the one that has none.

Measured by reflection against HotSpot 25: `java.lang.Object` there, `null`
here, on `System.out` and `System.err`, in both modes. It is now built and
published with the other three, for the reason that function's own comment
already gave: a half-wired stream is worse than either endpoint.

**A field with no wiring to forget is the one a hand-built carrier forgets.**
Census, image adjudication and every behavioural probe are blind to it; only
reflection against the oracle sees it.

### 9.24 A doc comment can be about the function next door

`native_printstream_init_outputstream` is preceded by a doc block that says
"delegates to `PrintWriter(OutputStream, boolean)`", names JUnit's
ConsoleLauncher, and states: "Implementation strategy: delegate to the real
two-arg JDK constructor via `invoke_special` so that the JDK's own
`out`/`lock`/etc. fields get populated correctly."

The function does no such thing. That paragraph belongs to
`native_printwriter_init_outputstream`, which sits immediately below it and
does exactly what the text describes -- an artefact of the pure code move that
split `logging_shims.rs` out of `lib.rs` (and the move's own header comment,
"no logic, signature or ordering changes", is telling the truth). The cost is
that blocked-list item 3 reads as already done to anyone who greps for the
chain and finds the sentence.

**A doc comment is attached to whatever follows it, not to whatever it is
about.** After a code move the two can differ, and no gate in this tree can
see it.

### 9.25 Acceptance

The probe tree was clean on this wave from the first build -- 126 sweep rows,
0 differing from HotSpot in all six arms, retired and un-retired, strict and
compatible. §9.3 says exactly what that is worth on its own, so the corpus was
asked too.

| gate | control (`ebfff4b31`) | wave 6 |
|---|---|---|
| `L4PrintStreamSweep`, 126 rows vs HotSpot 25 | 0 differing | **0**, in all six arms |
| `L4W6PrintCarrier`, 66 rows vs HotSpot 25, `--jdk-only` | 24 lines | **4** |
| corpus `--jdk-only`, `SUITE=all`, `TIMEOUT=600` | 136 passed, 0 failed | **136 passed, 0 failed — identical vector by vector** |
| strict report, `java/io/Print*` rows | 32, **every one `native-won`** | 36 listed, **0 `native-won`** |
| stub ratchet, default / management / synthetic | OFF 4168 / 4195 / 4168 | ON **4204 / 4231 / 4204**, **+36** |
| ratchet totals | 13590 / 13958 / 13625 | unmoved, both columns |
| kind-map gate, 25/linux | fires 1125 | fires **1125**, byte-identical row set |
| census kinds on the family | 38 registrations | **36 `synthetic-stub`**, 1 `intrinsic`, 1 `bridge` |
| `cargo test -p cratonvm-native-builtins --tests` ×3 | — | 12 targets each: **4340 / 4372 / 4521 passed, 0 failed** |
| `cargo test -p cratonvm-native-api` | — | 11 targets, **533 passed, 0 failed** |

The OFF column of the ratchet is that same binary with
`CRATONVM_UNRETIRE_NATIVE_SHADOW="java/io/PrintWriter,java/io/PrintStream"`,
and in that column the test PASSES at the committed baselines — which is what
pins the +36 to this wave and not to the 156 rows L1's wave 8 landed on the
same three constants in between. **+36 against 36 rows**, unlike wave 5's +33
against 27: `lib.rs` registers this family first and
`register_printstream_fallback_natives` overwrites every slot, so the census
sees one registration per triple. A second registrar is not a second
registration; ask the census which one owns the slot.

**The funnel is 7 of 7 and 28 of 29, and the missing one cannot be closed.**
`PrintStream.write(Ljava/lang/String;)V` is `private` in all three images and
is structurally unobservable: in compatible mode its only caller,
`print(String)`, is itself shadowed so the native is never reached; in strict
mode the row is retired so there is no counter to bump. It retires on W7-22's
rule — "it must retire with the family or not at all" — and not on a
measurement, which is a weaker warrant than every other row here and is said
so rather than rounded up.

**Two gates fired on this wave and both were right.**
`no_new_class_is_both_minted_and_retired` (lane 6, landed the same day) caught
`java/io/PrintStream` newly meeting its precondition: a native allocates it and
this wave retires onto it. The narrowing that gate asks for was run before
baselining — the only non-parameter, non-zero declared initialiser in the whole
hierarchy is `FilterOutputStream.closeLock`, **no retired method reads it**
(`PrintStream.close()` overrides `FilterOutputStream.close()` and synchronizes
on `this`), and the second mint site never lets the object reach Java.
Precondition met, hazard not. And `architecture_per_crate_loc_table_matches_reality`
went red on `native-api`: claimed 49 000, threshold 51 450, **51 358 without
this wave and 51 618 with it** — the 260 lines of table and account are what
crossed it, so that row was re-measured and only that row. `jit` is stale in
the same table at 5.11% with zero lines added to `../../../jit` here, and is left
alone: re-measuring a crate you did not move folds another lane's drift into
your commit, which is the same refusal made about the kind-map baseline.

**One dev-red is skipped rather than fixed.**
`buffer_session::tests::only_a_served_class_is_claimed` fails only under
parallel execution — its sibling `overflow_degrades_to_unserved` deliberately
fills a shared process-global table, so the other's `note_served` finds no
free slot and silently no-ops. Alone it passes; `--test-threads=1` passes both;
parallel fails. It landed in `9b7df29bb` and belongs to that lane. It matters
here only because `cargo test --tests` is fail-fast ACROSS TARGETS: red in the
`--lib` target skipped the other eleven in two of the three arms, which is how
a foreign failure hides yours. The counts above are from a re-run with that one
test skipped.


**One vacuous run, caught by the clock.** The first corpus attempt invoked
`run.sh` with `sh` rather than `bash`, which broke on `BASH_SOURCE`; the script
then refused to resolve its root and both arms produced **zero** vectors. The
comparison of two empty sets printed "IDENTICAL to the control, vector by
vector" and returned in seconds. The harness now refuses to call an empty
control a pass. It is the third instrument in this campaign to pass by matching
nothing.

### 9.26 What is left after this wave

* **`java/nio/file/Path`** -- **RETIRED, wave 7, nine of ten rows** (§9.28).
  The ORDERING blocker §9.27 describes is closed: `p57_alloc_path` and
  `p57_alloc_path_raw` mint through `concrete_receiver::alloc_concrete`
  against `path_layout::impl_class()`, so a native-built Path is stamped as
  the real `UnixPath`/`WindowsPath`, not the interface. The tenth row,
  `register(WatchService, Kind[])`, is measured unsafe on a SEPARATE defect
  (§9.28) and stays a native.
* **`java/nio/file/spi/FileSystemProvider` -- RETIRED, four of nine rows**
  (§9.31): `createLink`/`createSymbolicLink`/`newFileChannel`/
  `readSymbolicLink`. All four blockers the first three attempts found are
  closed (ordering, §9.27; `Path.fs`, §9.29; `FileSystem`'s own carrier,
  §9.30; `WindowsPath.type`/`.root`, §9.31). The other five stay native on
  `javap -c` evidence, not measurement gaps -- see
  `RETIRED_SHADOW_L4_FILESYSTEMPROVIDER_TRIPLES`'s own doc comment
  (`../../../native-api/src/retired_shadow.rs`) for why each one stays out.
* **`sun/nio/ch/` (308)** -- package verdict upheld, not beaten.
* **`jdk/internal/foreign` segment/arena/session (70)** -- a decision on record
  says they must not be retired at all.
* **The three `java/io/ByteArrayInputStream` `BaisEvent` observers** (§9.19) --
  correct rows, unretirable until the observation moves to a stream the HTTP
  layer owns.
* **`System.out.out` is a `FileOutputStream` where HotSpot has a
  `BufferedOutputStream`** -- the last two rows this wave's carrier probe still
  differs on, in both modes, and NOT a correctness defect: the writes land, they
  are simply unbuffered. Changing it moves when output reaches the terminal for
  every lane at once, so it is its own decision with its own measurement, not a
  tail-end fix on a retirement commit.
* **Rows no probe in this tree invokes** -- the largest bucket, and still a
  statement about the instrument rather than about the rows.

### 9.27 The Path carrier fix (`636ef84ed`), and why it changes nothing you can see

`path_layout.rs` (2026-09-10) resolved a native-built Path's FIELD LAYOUT
against the real `sun.nio.fs.UnixPath`/`WindowsPath` class -- `fs`, `path`,
`stringValue` land at the real class's own slot indices, so real bytecode
reading those fields sees the right values. It never touched the object's
CLASS STAMP: `p57_alloc_path` still minted every result under the literal
name `java/nio/file/Path`, an interface no `new` opcode in any image can
legally produce (`concrete_receiver.rs`'s own module doc names this species).
`getClass()` and `instanceof` answered the interface, which is exactly what
`sun.nio.fs.*FileSystemProvider`'s own `toUnixPath`/`toWindowsPath` bodies
check before doing anything -- the blocker this wave's own dial sweep
recorded turning `L4FilesSweep` 0 to 172 differing lines.

The fix: `p57_alloc_path` and `p57_alloc_path_raw` (the second producer --
jar/jrt entries, easy to miss because it does not sit beside the first) now
call `concrete_receiver::alloc_concrete` against `path_layout::impl_class()`
-- the SAME name the field-layout resolver already reads, so the two halves
cannot drift the way a hand-kept second copy would -- and every Path native
registration is mirrored onto that concrete class name so `H11-1` dispatch
(receiver's runtime class, not its declared interface) still finds them.
`nfields = 0`: every slot a Path needs is already a declared field on the
concrete class; nothing appended.

**Why four different measurements all read unchanged.** `real-jdk` (default)
and `--jdk-only`, each run patched and unpatched, against a real JDK 25
oracle, on `L4FilesSweep` (395 rows): byte-identical in all four arms, save
four divergences present in EVERY arm including the unpatched ones (a `//`
UNC-path spec, `AccessDeniedException` vs `IOException` specificity on two
rows, and `copy(self, self)`'s boolean) -- none of them Path-identity shaped,
none of them moved by this fix either direction. The reason the fix is
invisible to this probe: `Paths.get`/`FileSystems.getDefault().getPath`
against the REAL default filesystem already runs real `WindowsFileSystem`
bytecode end to end (`new WindowsPath(...)`), never touching
`p57_alloc_path` at all, so every Path `L4FilesSweep` constructs was already
correctly stamped before this fix. The defect only reaches user-visible
behaviour through a SYNTHETIC-FS-backed Path (jar/jrt) or a build where the
default filesystem itself is not real -- L4FilesSweep exercises neither.

**What did move, and is the actual evidence the fix does something**: the
JVMS-6.5 uninstantiable-receiver census (`--jdk-only`, `CRATONVM_DBG_LAYOUT_ALIAS`
family). Unpatched, at `f9255b937`, running nothing but `Paths.get(...)`
once: `java/nio/file/Path (interface, requester=...nio_file.rs:9920)`.
Patched, running the full `L4FilesSweep`: that class is gone from the list
entirely -- the remaining six entries (`Iterator`, `FileSystem`,
`FileSystemProvider`, `Stream`, `PathMatcher`, `BasicFileAttributeView`) are
all pre-existing, all out of this fix's scope, all future work.

**Verification run**: `cargo test` (dev profile, not `--release` -- the fat-LTO
release profile is what starved the build host this fix was originally
attempted on) across `native-api`, `native-io`, `native-builtins`, `vm`,
`native-collections` -- every suite green except two pre-existing failures,
each confirmed unrelated by A/B against the unpatched tree at the same
commit: `stub_ratchet`'s `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` had already
drifted to 4213 (frozen at 4204) before this change touched anything -- the
stub count is 4213 in BOTH arms, unmoved by this fix's +43 (all `Bridge`-kind
mirror rows, zero new `SyntheticStub`s) -- and `probe_fixture_census`'s
`fjp_probe` fixture is missing from this checkout independent of Path.

No retirement table accompanies this commit, matching the
`FILE_STORE_IMPLS`/`DIR_STREAM_IMPLS` precedent in this same file: a carrier
fix is a complete unit of work on its own, and the 11 Path + 9
FileSystemProvider rows are the next wave's, not this commit's.

### 9.28 Wave 7 -- 2026-09-16, nine `Path` rows retired, `FileSystemProvider` tried and blocked on a defect the attempt found

Same-day follow-up to §9.27, once `24f103568` gave `FileSystemProvider` the
same concrete mint. Three findings, in the order a build-per-hypothesis loop
found them.

**A carrier fix's own mirror can have a gap the carrier fix's own author does
not see.** `register_phase57_nio_file`'s mirror covers every row IT
registers, but two natives are registered by OTHER functions in OTHER
crates: `Path.register(WatchService, Kind...)` (`../../../native-io/src/lib.rs`,
`register_watch_service`) and
`FileSystemProvider.newAsynchronousFileChannel` (`register_async_file_channel`,
same file). Neither was in `636ef84ed`/`24f103568`'s mirror window, so both
went silently unreachable the moment their receiver became a concrete class
-- MEASURED as two live `NullPointerException`s on `dev`, not found by
`L4FilesSweep` (which does not call either), found by two purpose-built
probes instead. `47c6ac107` mirrors both from their own registrars, the same
pattern `register_watch_service` already used for its own `WatchService`/
`WatchKey`/`WatchEvent` rows -- it had the right idea for three families and
missed a fourth in the same function.

**`Path` retires nine of ten.** The census (`--jdk-only --explain-jdk-only
--dump-native-registry`, filtered to `owns_slot`/`Bridge`/
`image_declaring_method.has_code`) and two independent `javap -p` reads
agree on ten candidate rows -- the `Path` interface's own DEFAULT/STATIC
methods, which `WindowsPath` never overrides. Nine retire cleanly
(`0478a88dc`). The tenth, `register(WatchService, Kind...)`, is excluded:
EVEN with `47c6ac107`'s mirror restoring the native, retiring the row would
still swap it for real bytecode, and real bytecode needs
`WindowsWatchService.poller`, which this VM never populates -- measured as
the exact same NPE shape, one class over.

**`FileSystemProvider` retirement was tried and is blocked on `Path`'s OWN
`fs` field, not on anything about `FileSystemProvider`.** `path_layout.rs`
resolves `PathSlots::fs` (the index) but `p57_write_path_fields` never
writes it -- a native-built Path's `fs` field is null, always, a gap that
predates this wave by a week and was invisible for as long as no real
bytecode ever reached it. It reaches it now: `WindowsFileSystemProvider`'s
real overrides of `createLink`/`createSymbolicLink`/`newFileChannel`/
`readSymbolicLink` all call `Path.getFileSystem()` internally, so retiring
even these four -- the ones every other measure said were safe, real
`Code`, no synthetic-only behaviour to lose -- makes `L4FilesSweep` **crash
outright** with an uncaught `NullPointerException` at row 373 of 395,
`exit 1`. Confirmed with `CRATONVM_UNRETIRE_NATIVE_SHADOW`: disarm just the
`FileSystemProvider` table (leaving `Path`'s armed) and the crash
disappears, output matches HotSpot exactly. This is a WORSE failure than
the `UnsupportedOperationException` §9.27's own `p57_alloc_provider` doc
comment predicted for the excluded rows -- a crash instead of a caught
exception -- which is exactly why it is measured here rather than assumed
safe by the same reasoning that cleared `Path`'s nine.

No `FileSystemProvider` table is added by this wave. Landing one needs
`path_layout.rs`'s own fix first: populate `fs` at Path-construction time
(`p57_write_path_fields`, alongside `string`/`bytes`), most likely with the
constructing `FileSystem` object -- default, jar, or jrt, whichever one the
allocator actually has in hand at each of `p57_alloc_path`'s call sites.
That is its own measurement (which value is correct for a jar-FS path built
during a walk versus one built fresh?) and its own build, not a tail-end fix
on this commit.

### 9.29 Same-day follow-up to §9.28: the `fs`-field fix landed, and it found a THIRD carrier-identity defect one class further down

`p57_write_path_fields` now resolves `fs` from the Path's own encoded `text`:
`jarfs_decode`/`jrtfs_decode` (already in this file, used by the jar/jrt
allocators) pick the owning virtual FileSystem for a jar/jrt-encoded Path,
everything else gets `p57_default_filesystem_singleton`. Both allocators
already existed (`p57_alloc_jar_filesystem`, `p57_alloc_jrt_filesystem`); this
wave's only change is calling them from the one writer instead of never.

**Verification.** `L4FsFieldProbe` (identity: `abc.getFileSystem() ==
FileSystems.getDefault()`, `pathA.getFileSystem() == pathB.getFileSystem()`
for two paths off the same construction, `resolve()`'s child sharing the
parent's `fs`) and the four §9.28 provider methods
(`createLink`/`createSymbolicLink`/`newFileChannel`/`readSymbolicLink`)
against a real Path all match the JDK 25 oracle exactly -- including the two
Windows privilege-elevation exceptions `createSymbolicLink` throws on a
non-admin account, reproduced byte-for-byte. `L4FilesSweep` (395 rows):
zero new divergences: the same four pre-existing, already-documented lines
(§9.28's own arm; UNC `path[//]`, two `AccessDeniedException`/`IOException`
rows, `copy(self,self)`) and nothing else. `cargo test` across `native-api`,
`native-io`, `native-builtins`, `native-collections`: green except
`stub_ratchet`'s `synthetic_stub_count_does_not_regress`, confirmed
pre-existing and NOT moved by this fix by an A/B against the unpatched tree
at the same commit (both read 4456 SyntheticStub registrations against a
frozen baseline of 4204 -- a platform-dependent drift, not attributable to
this change, which registers no natives at all).

**What this fix does NOT do: land `RETIRED_SHADOW_L4_FILESYSTEMPROVIDER_TRIPLES`.**
With the `fs`-field gap closed, the four-row table §9.28 diagnosed
(`createLink`/`createSymbolicLink`/`newFileChannel`/`readSymbolicLink`) was
retried -- same table, rebuilt, same probes -- and `L4FilesSweep` crashed
again, differently:

```
Exception in thread "main" java/lang/NoSuchMethodError:
'java.lang.String java.nio.file.FileSystem.defaultRoot()'
	at sun/nio/fs/WindowsPath.getAbsolutePath(WindowsPath.java:247)
	at sun/nio/fs/WindowsPath.getPathForWin32Calls(WindowsPath.java:195)
	at sun/nio/fs/WindowsPath.getPathForWin32Calls(WindowsPath.java:172)
	at sun/nio/fs/WindowsFileSystemProvider.newFileChannel(WindowsFileSystemProvider.java:114)
```

Confirmed the cause and confirmed it is isolated to this table (not a
regression in the `fs`-field fix itself) with the same instrument §9.28 used:
`CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/file/spi/FileSystemProvider` makes
the crash disappear, output matching HotSpot exactly. **The table was
reverted a second time** (`git checkout` on `../../../native-api/src/retired_shadow.rs`
alone -- the `fs`-field fix in `../../../native-builtins/src/phases_late/nio_file.rs`
is unaffected and stays landed).

**The root cause is `java/nio/file/FileSystem` itself, and it is the SAME
species of defect as Path and FileSystemProvider before their carrier fixes**
(`636ef84ed`, `24f103568`): `p57_alloc_default_filesystem` /
`p57_alloc_jar_filesystem` / `p57_alloc_jrt_filesystem`
(`../../../native-builtins/src/phases_late/nio_file.rs`) all mint under the literal
name `java/nio/file/FileSystem` -- abstract in every real image. The VM's own
diagnostics already named this before this wave touched anything (the JVMS
6.5 uninstantiable-receiver census lists it, unconditionally, on every run
that allocates a default FileSystem: `java/nio/file/FileSystem (abstract,
requester=...nio_file.rs:12917)`), but it was silent behaviourally as long as
no real bytecode reached a `Path`'s `fs` field -- exactly the shape §9.28's
own defect had for `Path.getFileSystem()` one call deeper. Now that `fs` is
populated, `WindowsPath.getAbsolutePath()`'s real body reads it and calls
`defaultRoot()`, declared only on the CONCRETE `sun.nio.fs.WindowsFileSystem`,
never on the abstract interface this VM's object claims to be --
`NoSuchMethodError`, not the `AbstractMethodError` an abstract-but-declared
method would give, because the interface does not declare `defaultRoot` at
all (it is Windows-implementation-specific, not part of the `FileSystem`
public API).

**Why this is a materially bigger fix than Path's or FileSystemProvider's,
and not a tail-end patch on this commit.** Both of those fixes worked because
the concrete class's OWN fields were a superset this VM could resolve by name
and write by index (`path_layout::resolve_uncached`), and the synthetic
carrier's existing slots already corresponded to real ones (`fs`/`path`/
`stringValue`). `sun.nio.fs.WindowsFileSystem`'s three real instance fields
are `provider`, `defaultDirectory`, `defaultRoot` -- and this VM's synthetic
`FileSystem` (`P57_FS_SLOTS = 4`: separator, mounted-jar path, mounted-jrt
`java.home`, provider) stores something else entirely at three of those four
slots. Stamping the object as the concrete class without also correctly
populating `defaultDirectory`/`defaultRoot` would trade this `NoSuchMethodError`
for a null-valued read wherever real bytecode calls those accessors -- the
exact `path_layout.rs`-shaped problem one class up, requiring its own field
map, its own resolution of what `defaultDirectory`/`defaultRoot` mean for a
jar/jrt-backed synthetic FileSystem (they are OS-path concepts; a jar/jrt
mount has no OS directory), and its own measurement. Not attempted this
session; flagged as a follow-up task (spawned via `spawn_task`, not yet
started as of this wave).

**Net effect of this wave**: the `fs`-field fix lands unconditionally (it is
correct and measured-safe on its own, independent of any retirement --
`Path.getFileSystem()` identity was simply wrong/null before, for every
caller, not only a retirement candidate). `FileSystemProvider` retirement
stays at zero rows, now blocked on `FileSystem`'s own carrier identity rather
than on `Path`'s `fs` field -- one layer of the same defect closed, the next
one down found and diagnosed, not yet fixed.

### 9.30 2026-09-17, the `FileSystem` carrier fix lands, `FileSystemProvider` retirement tried a THIRD time and blocked on a FOURTH defect

Follow-up session to §9.29's flagged task: fix the `java/nio/file/FileSystem`
carrier-identity defect that blocked `FileSystemProvider` retirement.

**The fix.** New module `native-api::filesystem_layout`, the same shape as
`path_layout.rs`: resolves `provider`/`defaultDirectory`/`defaultRoot`'s real
slot indices on `sun.nio.fs.WindowsFileSystem` by NAME (never assumed by
position), returning `None` -- not a guessed layout -- when the concrete
class does not resolve or the platform is not Windows (`resolve_uncached`
gates on `cfg!(windows)` explicitly: `sun.nio.fs.UnixFileSystem`'s field
NAMES were not independently `javap`'d, and a name match alone cannot catch
a TYPE mismatch -- Unix paths are byte-encoded in several JDK internals,
unlike Windows' plain `String`s, so writing a `String` into a field the real
class types as `byte[]` would be the exact type-confused-slot species
`path_layout.rs` closed for `Path`).

`p57_alloc_default_filesystem` (`../../../native-builtins/src/phases_late/nio_file.rs`)
now mints the DEFAULT (file-scheme) FileSystem singleton via `alloc_concrete`
against `filesystem_layout::impl_class()` when a layout resolves, falling
back to the pre-fix synthetic shape otherwise. Jar/jrt-backed FileSystems are
explicitly OUT OF SCOPE and unchanged: their real counterparts
(`jdk.nio.zipfs.ZipFileSystem`, `jdk.internal.jrtfs.JrtFileSystem`) are
unrelated classes with unrelated field layouts this VM does not model, so
`p57_alloc_jar_filesystem` was repointed at a new
`p57_alloc_default_filesystem_synthetic` (the old body, unchanged) instead of
the newly-concrete-capable `p57_alloc_default_filesystem` it used to share.

`defaultDirectory`/`defaultRoot` are computed by invoking the REAL, already-
functional `sun.nio.fs.WindowsPathParser.parse(String)` (package-private,
non-native -- natives bypass Java access control) on `user.dir`, the exact
same call HotSpot's own `WindowsFileSystemProvider` constructor makes --
not a Rust reimplementation of Windows drive/UNC-root parsing. Byte-identical
to HotSpot by construction, not by measurement.

**The blast-radius problem this fix's own review found, and closed before
building anything.** This VM's existing synthetic `FileSystem` object
(`P57_FS_SLOTS = 4`: separator, mounted-jar path, mounted-jrt `java.home`,
provider-cache) and the newly-concrete `WindowsFileSystem` (3 real fields:
`provider`, `defaultDirectory`, `defaultRoot`) do NOT correspond field-for-
field. Seven call sites in `nio_file.rs` read `P57_FS_JAR_FIELD`/
`P57_FS_JRT_FIELD` (indices 1/2) directly off a `this` receiver to decide
"is this a mounted jar/jrt FileSystem" -- `getSeparator`, `getPath`,
`isReadOnly`, `close`, `supportedFileAttributeViews`, `getRootDirectories`,
and `path_owned_by_virtual_fs`. Against a genuinely concrete 3-field
`WindowsFileSystem` receiver, those same indices land on ITS real fields
(`defaultDirectory`/`defaultRoot`, always non-null Strings) instead -- every
one of those sites would have silently misclassified the real default
FileSystem as virtual. Closed with one guard
(`p57_fs_is_synthetic_shaped`/`p57_fs_is_virtual`, `object_num_fields(fs) >=
P57_FS_SLOTS`) every one of the seven sites now goes through, plus a fix to
`p57_fs_provider`'s own undersized-receiver fallback (it already anticipated
a real 3-field receiver reaching it -- see its own pre-existing doc comment
-- but its fallback re-minted a FRESH, uncached provider every call, which
would have made `fs.provider() == fs.provider()` false the moment the
default FileSystem became concrete; it now reads the real `provider` field
back via `filesystem_layout::slots()` instead).

**A cross-crate mirror gap, the SAME species found twice already this lane
(§9.29's own two mirror-gap fixes, and the `WatchService`/`Path.register`
gap from the Path wave).** `FileSystem.newWatchService` is registered in
`../../../native-io/src/lib.rs`'s `register_watch_service`, a different crate and a
different registrar than `native-builtins`'s `register_phase57_nio_file` --
which mirrors every `java/nio/file/FileSystem` registration IT made onto the
concrete class at the end of its own function, and cannot reach a row
registered elsewhere. Added a matching mirror call in
`register_watch_service` itself, onto `filesystem_layout::impl_class()`.

**Verification, once built.** `L4FsCarrierProbe` (default FS identity,
`getClass()`, `isOpen`/`isReadOnly`/`getSeparator`, provider identity across
`fs.provider() == fs.provider()` AND cross-call `FileSystems.getDefault()
.provider()`, `getRootDirectories()`, `supportedFileAttributeViews()`, the
exact `toAbsolutePath()` scenarios that crashed before on both the RELATIVE
and DIRECTORY_RELATIVE branches, a full write/read round trip through a
`toAbsolutePath()`-derived path, `getPathMatcher`, `newWatchService` +
`Path.register`, and `close()`) matches the JDK 25 oracle on every line but
one: `FileSystem.close()`'s exception MESSAGE (`"The default file system
cannot be closed"` here, no message on HotSpot) -- confirmed PRE-EXISTING
and untouched by this fix (the `close` native registration's message text
predates this session; `L4FilesSweep`'s own coarser probe format never
exposed it). `L4FilesSweep` (395 rows): zero new divergences, the same four
pre-existing lines §9.28/§9.29 already carry. `cargo test` across
`native-api`, `native-io`, `native-builtins`, `native-collections`: fully
green, including `stub_ratchet` (an `origin/dev` re-freeze absorbed the
platform-dependent drift §9.29 A/B'd as pre-existing and unrelated).

**`FileSystemProvider` retirement tried a THIRD time, same four-row table
(`createLink`/`createSymbolicLink`/`newFileChannel`/`readSymbolicLink`),
same result: `L4FilesSweep` crashes, differently again.** With `Path.fs` and
`FileSystem`'s own carrier both fixed, real
`WindowsFileSystemProvider.newFileChannel` reaches real
`WindowsPath.getPathForWin32Calls` → `getAbsolutePath` → `isSameDrive`, which
reads `this.root` directly -- `NullPointerException: Cannot invoke
"String.charAt(int)" because "root1" is null`. Confirmed isolated to this
table (not a new regression in the `FileSystem` fix) the same way as every
prior attempt: `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/file/spi/FileSystemProvider`
makes the crash disappear, `L4FilesSweep` clean.

**The FOURTH defect, and it is Path's own "contents" again, not another
carrier's stamp.** `sun.nio.fs.WindowsPath` (`javap -p`) declares SEVEN real
instance fields: `fs`, `type` (`WindowsPathType`), `root` (`String`), `path`
(`String`), `pathForWin32Calls`, `offsets`, plus two static constants.
`../../../native-api/src/path_layout.rs`'s `PathSlots` resolves and
`p57_write_path_fields` writes exactly THREE of those seven: `string`
(→ `path`), `bytes` (Unix only), and, as of §9.29, `fs`. `type` and `root`
are declared, resolved-width-wise accounted for (`width` covers them so nothing
is written OUT of bounds), but never WRITTEN -- permanently null/zero on
every Path this VM's natives build, since `636ef84ed` first minted a
concrete Path. Invisible for the same reason every defect in this whole
family was invisible: no real bytecode read `type`/`root` until a retired
`newFileChannel` let real `WindowsPath` internals run far enough to reach
`isSameDrive`, which reads `this.root` unconditionally with no null guard
(HotSpot never expects it to be null, because on HotSpot it is written in
`WindowsPath`'s own constructor, in the SAME assignment that writes `path`).

**Why this is not a same-session fix.** Unlike `fs` (an object reference to
an already-existing FileSystem) or `defaultDirectory`/`defaultRoot` (parsed
once, from one input, at FileSystem-construction time), `root` and `type`
are PER-PATH: every `p57_alloc_path`/`p57_alloc_path_raw` call site (dozens,
per §9.28's own count) would need to classify its own input the same way
real `WindowsPathParser.parse` does (`ABSOLUTE`/`UNC`/`RELATIVE`/
`DIRECTORY_RELATIVE`/`DRIVE_RELATIVE`) and store the resulting `root` string
alongside it -- not a single construction-time computation the way the
FileSystem fix was. The lowest-risk route, following this wave's own
`WindowsPathParser.parse` precedent for `defaultDirectory`/`defaultRoot`, is
almost certainly to invoke that SAME real, already-functional parser from
`p57_write_path_fields` itself (the one writer every allocator already goes
through) rather than hand-porting Windows path classification into Rust a
second time -- but that needs its own measurement (does every call site's
input text survive round-tripping through the real parser unchanged? jar/jrt
sentinel-encoded strings almost certainly do NOT and would need to keep
today's behaviour) and its own build. Flagged as a follow-up task.

**Net effect of this wave**: the `FileSystem` carrier fix lands
unconditionally (Windows-verified, measured-safe on its own, independent of
any retirement -- `WindowsPath.getAbsolutePath()`/`toAbsolutePath()` and
every other real method that reaches `getFileSystem()` were simply broken or
running the wrong object before, for every caller, not only a retirement
candidate). `FileSystemProvider` retirement stays at zero rows, now blocked
on `Path.type`/`Path.root`, the fourth defect in a four-deep chain that
started at `java/nio/file/Path`'s own carrier stamp in §9.27.

### 9.31 2026-09-17, same day, `WindowsPath.type`/`.root` close the fourth defect, `FileSystemProvider` retirement lands four of nine rows on the fourth attempt

Follow-up session to §9.30's flagged task, in the same worktree
(`jdk-lanes-windowspath-typeroot-20260917`) that section's own doc named in
advance.

**The fix.** `native-api::path_layout::PathSlots` gains two new optional
slots, `kind` (→ `WindowsPath.type`) and `root` (→ `WindowsPath.root`),
resolved by NAME exactly like every other field in this module -- `None` on
Unix, where `UnixPath` has neither field, so no `cfg` is needed (the same
reasoning `path_layout.rs`'s own module doc already gives for `string`/
`bytes`). `p57_write_path_fields` (`native-builtins/src/phases_late/
nio_file.rs`) now calls a new helper, `p57_classify_windows_path`, which
invokes the REAL `sun.nio.fs.WindowsPathParser.parse(String)` on the Path's
own text -- the SAME call §9.30's `FileSystem` fix already used for
`defaultDirectory`/`defaultRoot`, extended one class over. `type`'s result
(a `WindowsPathType` enum reference) is written through unchanged; `root`'s
result has `\` folded to `/` before writing, matching this VM's own
internal Path convention (`p57_alloc_path`'s doc) -- every real reader of
`root` is blind to which separator it uses (`isSameDrive` compares only
byte 0, a drive letter; every other reader slices `path` by `root.length()`,
not by content), so keeping `root` in the same convention as `path` avoids
ever mixing the two.

**The jar/jrt sentinel risk §9.30 flagged, measured rather than assumed.**
`p57_write_path_fields` already computes `jarfs_decode`/`jrtfs_decode` on
the same text (to pick the owning FileSystem); the classification step
skips real-parser classification entirely when either decodes, leaving
`type`/`root` unwritten (null) for those paths -- the SAME state they were
in before this fix, since no jar/jrt Path was ever going to survive a
real-Windows-path parse of a sentinel-encoded string. `p57_classify_windows_path`
itself is additionally fail-soft: any parse failure (a thrown
`InvalidPathException`, or the invoke not returning the expected shape)
leaves the two fields unwritten rather than failing the allocation --
matching the fact that two producing crates never wrote these fields at all
until today, so "still null" is never a new failure mode, only a missed
improvement.

**Verification.** A reflection-based probe (`WindowsPathTypeRootProbe`, not
part of the tree) reads `WindowsPath.type`/`.root` directly for absolute,
relative, UNC, drive-relative and directory-relative inputs, and separately
invokes the package-private `getPathForWin32Calls()` -- the exact method
that threw the `root1 is null` NPE in §9.30's third retirement attempt --
via `setAccessible`. Matches the JDK 25 oracle exactly on every row (`root`
compared with `\` folded to `/` on both sides, the same normalization the
fix itself performs; everything else compared verbatim). `L4FilesSweep`
(395 rows): zero new divergences, the same four pre-existing lines §9.28/
§9.29/§9.30 already carry.

**`FileSystemProvider` retirement tried a FOURTH time, same four-row table,
this time clean.** Re-applied `RETIRED_SHADOW_L4_FILESYSTEMPROVIDER_TRIPLES`
(saved from the third attempt, §9.30). No crash: `L4FilesSweep` still shows
only the same four pre-existing divergences. A dedicated probe
(`FspRetirementProbe`, not part of the tree) exercises `createLink` (hard
link, succeeds and reads back, matching the oracle), `createSymbolicLink`
(throws `FileSystemException` on this machine for lack of the Windows
symlink privilege, matching the oracle's own failure on the same host), and
`newFileChannel` (matches the oracle's successful absolute-path read).
`readSymbolicLink` was not directly exercised -- no symlink could be
created on this host to read back -- and stays UNVERIFIED beyond "the class
carries real `type`/`root` now, so the same NPE class cannot recur"; flagged
rather than claimed.

**One apparent new divergence, traced to an unrelated pre-existing native,
not to this wave.** `FileChannel.open()` on a missing relative path prints
`NoSuchFileException: relprobe.txt: relprobe.txt` on this VM (doubled)
versus the oracle's single `relprobe.txt`. Traced by reading, not guessed:
`java/nio/channels/FileChannel`'s `open` method is natively shadowed
directly (`native_fc_open`, `../../../native-io/src/lib.rs`) -- `javap -c` on
`WindowsFileSystemProvider.newFileChannel` confirms real bytecode calls
`sun.nio.fs.WindowsChannelFactory.newFileChannel`, a class this VM's source
tree never mentions once (`grep -rn WindowsChannelFactory` -- zero hits,
checked), so `FileChannel.open()` NEVER reaches the retired
`FileSystemProvider.newFileChannel` row at all, retired or not. Confirmed
by rebuilding WITHOUT the retirement table and re-running the same probe:
byte-identical output, doubled message included. The doubled message is
therefore `native_fc_open`'s own pre-existing `NoSuchFileException`
construction (`../../../native-io/src/lib.rs`, around the `fd_table` open-error
mapping), orthogonal to this wave and to the retirement table; not
investigated further here (out of scope: a native-message-formatting
defect, not a carrier-identity one) and not blocking, since it existed
identically before this session touched anything.

**`stub_ratchet` re-frozen, both arms, single documented cause.** Landing
the retirement table retags the 4 rows' existing `Bridge` registrations
(mirrored onto all 8 `FILE_SYSTEM_PROVIDER_IMPLS` platform classes by
`register_phase57_nio_file`'s own always-on mirror loop, per §9.28) to
`SyntheticStub` -- 32 rows, case (b) in the gate's own vocabulary, the
honest-labelling direction. Both arms moved by identical amounts (+32
stubs), the gate's own "one-wave" signature; TOTAL registrations moved by a
smaller, unattributed +11 the gate's own message flags as worth diffing
row-by-row against the last freeze commit, not done here (this worktree
branched from `origin/dev` several commits past that freeze, and the
diff landing THIS wave adds no `r.register(...)` calls at all, so the +11
is very unlikely to be this wave's own). See
`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`'s own doc comment
(`../../../native-builtins/tests/stub_ratchet.rs`) for the full account and numbers.

**Net effect of this wave**: `WindowsPath.type`/`.root` are now correct for
every Path this VM's natives build (Windows only; Unix has neither field).
`FileSystemProvider` retirement lands four of nine rows --
`createLink`/`createSymbolicLink`/`newFileChannel`/`readSymbolicLink` --
closing the four-defect, four-session chain that started at
`java/nio/file/Path`'s own carrier stamp in §9.27. The other five rows stay
native on `javap -c` evidence (§9.26), not on any defect still open.

---

### Wave 8 -- 2026-09-18, 24 rows over ten classes, blocked by a VM-wide `--jdk-only` regression this wave found and closed

**A candidate group, found for free.** `L4CensusTail.java` (already in the
tree, part of `../../../apps/probes`, written to reach exactly the rows its own doc
comment names) exercises three groups this lane has never tabled:
`java/io/LineNumberInputStream` (8 rows) and `java/io/StringBufferInputStream`
(5), `java/nio/file/SimpleFileVisitor`'s four erased `(Ljava/lang/Object;...)`
bridge rows, and the `ByteBufferAsCharBuffer{B,L,RB,RL}.toString(II)` family
(4) -- wave 4 retired `.toString(II)` on the `CharBuffer` base class alone
(§9.15), never on the four concrete subclasses that `cb_to_string_range`
(`../../../native-builtins/src/phases_late/charset_buffers.rs`) is ALSO registered
against individually, for vtable-lookup reasons that comment gives. 21 rows,
no new probe to write -- a further 3 (`HeapCharBuffer`/`HeapCharBufferR`/
`StringCharBuffer`, the same loop's other three classes) were found already
measured, in the same run, once the first cut's own acceptance was read back
against it; see §9.32.

Read for safety before measuring anything: `native_lnis_init` and its sibling
accessors (`../../../native-builtins/src/deprecated_util.rs`, ~L1693-1900) write
`LineNumberInputStream`'s state through FOUR fixed slot indices (`0`=wrapped
stream, `1`=line number, `2`=saved line number, `3`=pushback byte) that do
NOT correspond to the real class's five real fields (`FilterInputStream.in`
plus its own `pushBack`/`lineNumber`/`markLineNumber`/`markPushBack`) --
`try_alloc_concurrent_synthetic` stamps the object as the REAL class but the
accessors read/write it by a private convention, which is this lane's
five-times-repeated carrier-defect shape (§9.4, §9.7, §9.11, §9.15, §9.18,
§9.22) if retired PARTIALLY. The read that clears it: this class's own
`<init>` is ALSO in the candidate table. Retiring the whole surface -- `<init>`
through `reset` -- together, so nothing native ever touches those four slots
again and every field is populated by the real class's own `putfield`
bytecode instead, is the same shape wave 6 used for `PrintStream`
("retiring the constructors hands construction back to the JDK and all four
fields arrive") and sidesteps the carrier question entirely rather than
solving it. `StringBufferInputStream` is the same shape, three real fields
against a four-slot synthetic layout. `SimpleFileVisitor` declares no
instance fields at all (`javap -p`), so it carries none of this risk.
`ByteBufferAsCharBuffer{B,L,RB,RL}` inherit wave 4's carrier fix (§9.14) and
`cb_to_string_range`'s convention fix (§9.15) already; nothing new to verify
there beyond the retirement itself.

**Blocked before a build, on a VM-wide regression, not a lane-4 defect.**
Before writing any table, the funnel's own precondition 4 -- "invoked by
**your own** instrument" -- needed a `--jdk-only` run of `L4CensusTail` to
confirm invocation. That run, and every `--jdk-only` run attempted on this
host including a bare `System.out.println("hi")`, produced **zero bytes of
stdout**, silently, exit 0. Not a lane-4 carrier defect: a VM-wide,
pre-existing `--jdk-only` boot regression that already had a follow-up task
open (`task_54e1b4c2`, flagged from the CHM-properties-boot-fix session
earlier the same day) and was root-caused much further than that flag
originally had it, then fixed, in this same session:

* `System.initPhase1()` is deliberately left as REAL BYTECODE on any
  real-JDK-backed image -- an existing, intentional guard in `register_inner`
  (`../../../native-api/src/registry.rs`): `drop_real_layout_synthetic && class_name
  == "java/lang/System" && matches!(method_name, "initPhase1" | "initPhase2"
  | "initPhase3")`. `drop_real_layout_synthetic` is true for both plain
  real-JDK mode and `--jdk-only`. Real `initPhase1` (`javap -c`: no longer
  `native`, real `Code`) opens with `setJavaLangAccess()`,
  `SystemProps.initProperties()`, `VersionProps.init()`, then at bci 12 calls
  `jdk/internal/misc/VM.saveProperties(Map)` -- a method NOBODY HAD EVER
  REGISTERED A NATIVE FOR, so its own real body ran too: `if (initLevel() !=
  0) throw new IllegalStateException("Wrong init level"); savedProps =
  props;`.
* `VM.initLevel()`'s existing native floor-clamps to `max(live, 2)` --
  intentional, so `getSavedProperty`'s *own* first guard (which wants the
  opposite, nonzero) doesn't fire on every later `<clinit>` reading a saved
  property. That clamp makes `saveProperties`'s `!= 0` check permanently
  true, so every real-JDK-backed run threw "Wrong init level" on
  `initPhase1`'s very first substantive call, before `VM.savedProps` was
  ever populated -- and `ModuleLayer.boot()` hit the same still-null map
  later via `ClassLoaders.<clinit>` reading it, failing the SECOND guard
  ("Not yet initialized") instead. Per JVMS 5.5 that permanently marks
  `ClassLoaders` erroneous for the rest of the process; `System.out` was the
  visible casualty. Confirmed via `RUST_BACKTRACE=1` capture at the actual
  throw sites, not inferred from logs.

**Fixed**, `0f740b28f` on `origin/dev`: `native_vm_save_properties`
(`../../../native-builtins/src/lang_system.rs`), registered next to
`native_vm_get_saved_property` (its existing getter counterpart), takes the
`Map` real bytecode just built and writes it straight to `VM.savedProps` via
`set_static_field_by_name`, skipping the unsatisfiable level check entirely
-- same shape and same file as the getter. `System.out.println` now prints
under `--jdk-only`. `cargo test -p cratonvm-vm --test synthetic_diff`: ok.
`t14_system_conformance`: 12 passed. `cratonvm-native-builtins --lib`: 4276
passed, 0 failed. A sibling session (`task_54e1b4c2`, coordinated live rather
than duplicated) had two defensive hardenings in flight for the same
symptom; this fix is upstream of both.

### 9.32 Acceptance -- the 24-row candidate group, retired

With `--jdk-only` working, the candidate group measured clean and was
tabled in three tables, `RETIRED_SHADOW_L4_DEPRECATED_STREAMS_TRIPLES` (13),
`RETIRED_SHADOW_L4_FILEVISITOR_TRIPLES` (4) and
`RETIRED_SHADOW_L4_CHARBUFFER_VIEWS_TRIPLES` (7 -- the four
`ByteBufferAsCharBuffer{B,L,RB,RL}` rows plus `HeapCharBuffer`,
`HeapCharBufferR` and `StringCharBuffer`, added same-day once their own
`L4CensusTail` rows -- already captured in the same run -- were read back
against the table's own first cut, which had left them for later on no
stronger reason than caution).

`L4CensusTail`, 126 rows, oracle JDK 25 on windows/x86_64:

```text
  strict (--jdk-only), un-retired FileSystemProvider*   1 line differs
    resolveSibling absolute   C:\tmp\abs (oracle) vs \tmp\abs -- pre-existing,
    a Path row outside this wave's three tables, untouched by it
  everything this wave tabled (rows 1-38, 66-79, 88-95)   0 lines differ
```

\* `mapped()` (row ~80) still hits the pre-existing, unrelated
`sun/nio/fs/WindowsNativeDispatcher.CreateFile0` gap (§4's own "the largest
prefix lane" territory -- a Windows I/O native this VM has never
implemented, colliding with wave 9's `FileSystemProvider.newFileChannel`
retirement for a read+write open specifically). Scored with
`CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/file/spi/FileSystemProvider` to
reach the rows after it; not this wave's to fix, and not a survivor of
anything this wave tabled.

**Whole-class retirement, not per-method.** `native_lnis_init` and its
sibling accessors write `LineNumberInputStream` through four fixed slot
indices that do not correspond to the real class's five real fields
(`FilterInputStream.in` plus `pushBack`/`lineNumber`/`markLineNumber`/
`markPushBack`) -- `StringBufferInputStream` the same shape, three real
fields against a four-slot layout. Both tables retire `<init>` through every
accessor together, mirroring wave 6's `PrintStream` fix, so no native ever
touches those slots again and the real constructor's own `putfield`s
populate the real fields at the real offsets. `SimpleFileVisitor` declares no
instance fields (`javap -p`) and carried none of that risk.
`ByteBufferAsCharBuffer{B,L,RB,RL}`, `HeapCharBuffer`, `HeapCharBufferR` and
`StringCharBuffer` all inherit wave 4's carrier fix (§9.14) and
`cb_to_string_range`'s convention fix (§9.15); this wave only adds the table
rows wave 4 itself never tabled, across all seven classes that registration
loop covers.

**24 refusals, 0 survivors.** `--jdk-only-report`: every one of the 24 rows
reports `synthetic-native-registered` with `survivor: null` -- nothing else
owns any of these slots, so this is a retirement and not an inert re-tag.
LineNumberInputStream/StringBufferInputStream are each registered from TWO
files (`deprecated_io_util.rs` and `deprecated_util.rs`, last-write-wins);
both registration attempts for every retired triple are refused, matching
the table rather than one winning copy.

**What this wave does NOT claim.** The kind-map/stub-ratchet gates were not
run: both are keyed `25/linux` and refuse on Windows, the same platform
limitation §6 and every prior wave on this host have already named.

Measured on **windows/x86_64 against JDK 25**.

---

### Wave 9 -- 2026-09-18, the funnel step run against a working `--jdk-only` for the first time in this lane's history

**The instrument, finally live.** §9.1's own funnel (owns_slot, `Bridge`,
real image `has_code`, invoked by `--jdk-only`) needed a working
`--jdk-only` to run at all, and this is the first session in this lane's
history that had one. Fifteen probes already in the tree
(`L4Reach`/`L4FileSweep`/`L4FilesSweep`/`L4ByteBufferSweep`/
`L4CharBufferSweep`/`L4TypedBufferSweep`/`L4StreamTailSweep`/
`TailFamilySweep`/`L4PrintStreamSweep`/`L4CensusTail`/`L4AbsPath`/
`L4FfmLayoutSweep`/`L4TrustPath`/`L4W5Sweep`/`L4W6PrintCarrier`), each with
`--dump-native-registry --explain-jdk-only`, unioned across all fifteen
dumps: **155 candidate rows over roughly 25 classes**, invoked at least once,
not in any existing table. The two largest:

```text
  java/nio/file/Files           32 rows   (static utility surface)
  sun/nio/fs/WindowsPath         19 rows   (Path interface methods,
                                            registered on the CONCRETE class
                                            separately from wave 7's table,
                                            which retired only the
                                            INTERFACE's default/static rows)
  java/nio/DirectByteBuffer     14
  java/io/WinNTFileSystem       11
  sun/nio/fs/WindowsFileSystemProvider  11
  sun/nio/fs/WindowsFileSystem  10
  (smaller clusters down to single rows across ~19 more classes)
```

This is the honest size of the "no probe reaches it" bucket §9.6 named --
now a worklist with real invocation counts instead of a statement about the
instrument. Full listing kept in session scratch, not reproduced here; the
shape (which classes, roughly what size) is what matters for the next
session picking this up.

### 9.34 `java/nio/file/Files` was attempted and reverted -- a Windows I/O gap, not a lane-4 defect, and it likely blocks most of this wave's other big clusters too

All 32 `Files.*` rows were tabled and built. `L4FilesSweep` (the lane's own
established oracle-diff tool for this exact surface, §9.27 onward) did not
produce a soft diff -- it crashed on its FIRST filesystem call, hard:

```text
Exception in thread "main" java/lang/UnsatisfiedLinkError:
  sun/nio/fs/WindowsNativeDispatcher.GetFileAttributesEx0(JJ)V
    at L4FilesSweep.rmrf (cleanup, before the probe's first real row)
    at java/nio/file/Files.deleteIfExists
    at sun/nio/fs/AbstractFileSystemProvider.deleteIfExists
    at sun/nio/fs/WindowsFileSystemProvider.implDelete
    at sun/nio/fs/WindowsFileAttributes.get
    at sun/nio/fs/WindowsNativeDispatcher.GetFileAttributesEx
```

`WindowsFileAttributes.get` is the real JDK's canonical Windows "stat" call
-- the thing `exists`/`isDirectory`/`isRegularFile`/`size`/
`getLastModifiedTime`/`readAttributes`/`isSymbolicLink` all resolve through
on this platform. This VM has never implemented
`sun/nio/fs/WindowsNativeDispatcher.GetFileAttributesEx0` (the same species
gap as the already-known `CreateFile0` one blocking `newFileChannel`, §9.31
onward, a different Win32 primitive under the same `WindowsNativeDispatcher`
surface this VM's real-JDK-image support was never asked to cover). Table
reverted whole -- not carved, because the shared "stat" call plausibly
blocks most of the 32 rows rather than one -- and not carried forward
half-measured.

**This is very likely why `sun/nio/fs/WindowsPath`'s 19 rows and
`sun/nio/fs/WindowsFileSystemProvider`'s 11 are ALSO not safe to retire yet**
without the same primitive: both call through the same real
`WindowsFileAttributes`/`WindowsNativeDispatcher` machinery for their own
comparison/existence/attribute-reading paths. Not independently confirmed
this session -- named as the leading hypothesis for whoever measures those
two next, so the same crash is not rediscovered from zero.

The fix, if wanted, is implementing the missing `WindowsNativeDispatcher`
Win32 natives this VM's `sun/nio/fs/` support has never covered -- a
platform-I/O task in its own right, not a shadow retirement, and out of
this wave's scope.

### 9.35 Acceptance -- `Paths.get`, the one row this wave's funnel could confirm clean

`java/nio/file/Paths.get(String, String...)` is pure string-joining into
`p57_alloc_path_checked` -- no filesystem call, no
`WindowsNativeDispatcher` reach. Tabled alone
(`RETIRED_SHADOW_L4_PATHS_TRIPLES`), built, measured:

```text
  L4FilesSweep, --jdk-only, 395 rows (full run, FileSystemProvider
  un-retired to pass the pre-existing CreateFile0 cutoff -- see §9.31)

    retired                    4 lines differ from oracle
    un-retired (CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/file/Paths)
                                the SAME 4 lines differ, byte-for-byte
```

The four: a `//` UNC-path spec, `AccessDeniedException` vs `IOException`
specificity on two rows (`readAllBytes dir`, `newByteChannel on dir for
write`), and `copy(self, self)`'s boolean -- the exact four §9.27-§9.31
already carry, confirmed identical with this row retired or not. `1
refusal, 0 survivors`. `L4CensusTail`: unaffected, still the one
pre-existing `resolveSibling` diff §9.32 already names.

`cargo test -p cratonvm-vm --test synthetic_diff`: ok. `cargo test -p
cratonvm-types --test doc_citation_paths`: 2 passed.

Measured on **windows/x86_64 against JDK 25**.

### Wave 10 -- 2026-09-18, `WindowsNativeDispatcher.GetFileAttributesEx0` and `CreateFile0`, the primitive §9.34 named

§9.34 named the fix and put it out of scope: implement the missing
`WindowsNativeDispatcher` Win32 natives. This wave does the first two --
`GetFileAttributesEx0` and `CreateFile0` -- as genuine Win32 FFI, not
shadow retirement, following the precedent already in this file
(`hkcr_string_value` / `RegGetValueW`, backing
`RegistryFileTypeDetector.queryStringValue`, `#[link(name = "advapi32")]`,
no new Cargo dependency).

**`GetFileAttributesEx0`**: a real `GetFileAttributesExW` call, decoding
into the exact `WIN32_FILE_ATTRIBUTE_DATA` byte layout real
`WindowsFileAttributes.fromFileAttributeData(long, int)` expects --
offsets 0/4/12/20/28/32 for
`dwFileAttributes`/`ftCreationTime`/`ftLastAccessTime`/`ftLastWriteTime`/
`nFileSizeHigh`/`nFileSizeLow`, derived from `javap -c` on JDK 25, not
assumed. Verified byte-exact against a captured HotSpot oracle: retiring
nothing, just running `L4FilesSweep` under `--jdk-only` with the native
implemented, the path/attribute/directory-stream/walk/copy/move section
(~250 lines, everything up to the channel tests) now matches HotSpot
exactly.

**`CreateFile0`**: a real `CreateFileW` call. This one surfaced a live
correctness risk, not just a missing feature. Real
`WindowsChannelFactory.open`'s bytecode stores `CreateFile0`'s return
value straight into `FileDescriptor.handle`, and that `FileDescriptor`
then backs a real `sun.nio.ch.FileChannelImpl`. But every native already
in this VM that reads `FileDescriptor.fd`/`.handle` -- most concretely
`FileChannelImpl.truncate`'s own concrete-class override (§bug-27,
`lib.rs:8621`) -- already treats that value as an id into this VM's own
`fd_table`, because until this wave every `FileDescriptor` in the VM was
minted by an fd_table-backed open. A genuine Win32 `HANDLE` is
frequently a small integer too (early handles are commonly small
multiples of 4), so handing the raw value to Java could alias an
unrelated, already-open fd_table entry -- a wrong-file read, a wrong-file
truncate, silent cross-file corruption on a live channel op. Not a
crash, and not something the oracle-diff harness would necessarily catch
on a run that doesn't happen to collide.

Fixed by not letting the raw `HANDLE` reach Java at all: the new
`FdTable::insert_win32_file` (`../../../native-api/src/fd_table.rs`) wraps it in a
`std::fs::File` via `FromRawHandle` and inserts it through the SAME
allocator every other fd uses, and `CreateFile0`'s registration returns
that fd_table id, not the kernel handle. One id space, one producer,
same as every other fd in the VM -- the `FileChannelImpl.truncate`
override (and any future `FileDispatcherImpl` native) needs no special
case for a Windows-real-handle-backed channel.

**Measured**: with both natives in place, `L4FilesSweep` runs to
completion (`rc=0`, 395 rows) under plain `--jdk-only` -- no
`CRATONVM_UNRETIRE_NATIVE_SHADOW` workaround needed, unlike every prior
wave's measurement of this probe -- and matches HotSpot on every line
except the same 4 already-documented pre-existing divergences (`path[//]`,
`readAllBytes dir`, `copy self`; see §9.27-§9.31, §9.35). This includes
full channel coverage end to end (`channel write`/`read`/`size`/
`truncate`/`position`/negative-argument checks/`closed channel read`),
which is what actually exercises the fd-aliasing fix above -- confirmed
those lines match the oracle exactly, not just that the run didn't crash.

The full 15-probe lane-4 battery (§9.33's list) re-run clean against this
build: only the 3 already-documented pre-existing failures
(`TailFamilySweep`, `L4PrintStreamSweep`, `L4W6PrintCarrier`), no new
regressions. `cargo test -p cratonvm-native-api` (full suite, including
`synthetic_diff` and `doc_citation_paths`): ok.

**Not done by this wave**: `Files.*`'s 32 candidate rows (§9.34) are
still not retired. `Files.newByteChannel` itself already runs the real
`WindowsChannelFactory`/`CreateFile0` path unconditionally today --
that's WHY this wave's probe reached `CreateFile0` at all, independent of
any retirement table -- because the fd_table-backed synthetic override
lives on the ABSTRACT `FileSystemProvider.newFileChannel`
(`phases_late/nio_file.rs`, comment: "Real JDK delegates to
WindowsFileSystemProvider.newFileChannel which overrides this abstract
method") and this VM's native dispatch is keyed on the RESOLVED
declaring class, so `WindowsFileSystemProvider`'s own concrete override
always wins and that synthetic path is dead code on Windows. The other
31 `Files.*` rows are still native-backed by the VM's own
`vfs_*`/`p57_*` helpers (not by real `FileSystemProvider` delegation),
and retiring them would newly route through the same
`WindowsFileSystemProvider`/`WindowsNativeDispatcher` chain this wave
just proved works for `newByteChannel`.

Do NOT assume the channel read/write/size/position/truncate surface is
now fully safe for arbitrary `Files.*` retirement just because this
wave's probe passed end to end: `channel.read`/`.write`/`.size`/
`.position` on a real `FileChannelImpl` resolve to a separate,
pre-existing fast-path native family (`native-io/src/
file_channel_fast_read.rs`, `register_file_channel_fast_io`) that reads
`FileDescriptor.fd`/`.handle` the same fd_table-id way `truncate`'s
override does -- consistent with this wave's fix, not requiring a new
one -- but that fast path explicitly REFUSES (falls through to real
`FileChannelImpl` bytecode via `invoke_special_bytecode_only`) for
argument shapes it does not special-case (see its own `stats::bump(&
stats::READ_REFUSED)` sites). This run's own census line
(`[cratonvm] filechannel fast I/O: read fast=2 refused=1 write fast=1
refused=0 pos fast=4 refused=1 size fast=3 refused=0`) shows the refused
cases did occur and still passed -- plausibly because the refused lines
in this particular probe (`closed channel read`, `channel negative
position`) throw before ever reaching real I/O, not because the
fallthrough path's own native dependencies are covered. `sun/nio/ch/
FileDispatcherImpl`'s own JNI-native methods (`read0`/`pread0`/`write0`/
`pwrite0`/`close0`/`preClose0`/`size0`/`truncateFile0`/`force0`) are
still confirmed unimplemented anywhere in this codebase (grepped, zero
matches) -- whether the real `FileChannelImpl` bytecode a `refuse()` falls
through to ever reaches them, on this or another probe, was NOT measured
this wave. Confirm with a probe that forces the refused path on a live
read/write (not just a pre-I/O throw) before trusting either answer.

Measured on **windows/x86_64 against JDK 25**.

### 9.36 `sun/nio/fs/WindowsPath`'s 19 rows -- attempted immediately after Wave 10, reverted: a fabricated-carrier `root` mismatch, not a Win32-native gap

Wave 9's §9.34 named `sun/nio/fs/WindowsPath`'s 19 candidate rows as the
leading hypothesis for what Wave 10's `GetFileAttributesEx0`/`CreateFile0`
fix might also unblock. 17 of the 19 are pure path-string manipulation
with no native call at all (`javap -c` confirmed no `WindowsNativeDispatcher`
reach for `compareTo`/`endsWith`/`equals`/`getFileName`/`getFileSystem`/
`getName`/`getNameCount`/`getParent`/`getRoot`/`hashCode`/`isAbsolute`/
`normalize`/`relativize`/`resolve`/`startsWith`/`subpath`/`toUri`, and
even `toAbsolutePath()` turned out to be pure bytecode over
`WindowsFileSystem.defaultDirectory()`); only `toRealPath` genuinely
reaches native code. Tabled, built, measured against `L4FilesSweep`,
`L4CensusTail`, and two freshly-captured HotSpot oracles for `L4Reach`
and `TailFamilySweep` (the two probes invoking `toAbsolutePath`/
`toRealPath`) -- **not a Win32-native gap at all**:

```text
Exception in thread "main" java/lang/NullPointerException:
  Cannot invoke "String.length()" because "this.root" is null
    at L4FilesSweep.pathText(L4FilesSweep.java:140)
    at sun/nio/fs/WindowsPath.getFileName(WindowsPath.java:327)
```

`L4CensusTail` failed too, differently (`DirectoryNotEmptyException`
escaping uncaught where the oracle does not throw). Both regressions,
both immediate -- `L4FilesSweep` and `L4CensusTail` had been clean (rc=0,
oracle-exact) one wave earlier with these 19 rows still native.

**Root cause, corrected** (an earlier draft of this section overstated
this as "no producer ever sets `root`" -- traced one level further and
that is wrong; leaving the record straight rather than compounding one
imprecise claim with another). `p57_alloc_path` (the allocator behind
`WindowsFileSystem.getPath`/`Paths.get`/`Path.of`) calls
`p57_write_path_fields`, which DOES attempt to populate `root`/`type` --
via `p57_classify_windows_path`
(`native-builtins/src/phases_late/nio_file.rs:171`), which runs the
REAL `sun.nio.fs.WindowsPathParser.parse` through the interpreter, not
a Rust reimplementation. For most inputs this works and `root` ends up
correctly populated.

The gap is narrower and sharper: `p57_classify_windows_path` returns
`None` -- silently, leaving `root`/`type` unset -- whenever
`WindowsPathParser.parse` THROWS (its `match parsed { Ok(Some(...)) =>
.., _ => return None }` folds a thrown exception and every other
non-match into the same `None`). And `WindowsPathParser.parse("//")`
throws `InvalidPathException` -- correctly, matching HotSpot -- while
THIS VM's own `p57_alloc_path` accepts `"//"` as a valid path with no
exception at all. That accept/reject mismatch is not new: it is the
SAME already-documented pre-existing divergence `L4FilesSweep`'s own
oracle-diff has carried since §9.27 (`path[//] toString`: this VM
`\\`, HotSpot `THREW InvalidPathException`). The crash trace confirms
it exactly: `L4FilesSweep.pathText` prints `path[//] toString` (the
allocation succeeded) and crashes on the very next line, `path[//]
fileName` (`q.getFileName()`) -- `Paths.get("//")` didn't throw, so the
loop's `catch (InvalidPathException)` never ran, but the object it
produced has no `root`, because classifying `"//"` correctly throws
the exception this VM's own allocator declined to.

This means the defect is not "every `WindowsPath` this VM mints has an
unset `root`" -- most do not. It is specifically: **any input this VM's
looser `p57_alloc_path` accepts but the real `WindowsPathParser` would
reject** produces a `WindowsPath` with `root`/`type` silently unset.
`"//"` is the one instance measured so far; there may be others sharing
the same accept/reject gap, not enumerated this wave.

**The fix, if wanted, is narrower and cheaper than the first draft of
this section implied**: close the accept/reject gap at `p57_alloc_path`
itself (reject what `WindowsPathParser` would reject, matching HotSpot)
rather than reworking every `WindowsPath`-minting call site. That would
also retire the `path[//]` line of the pre-existing four-line divergence
in the same stroke. Not attempted this wave -- it touches every path
allocation in the VM, not only `WindowsPath`, and deserves the same
broad measurement discipline as any change with that blast radius, not
a quick pass under context pressure. Reverted whole
(`git checkout -- native-api/src/retired_shadow.rs`), not carved, since
until the accept/reject gap is closed, ANY retired `WindowsPath` method
reading `root` can hit the same unset field on some other input, not
only `"//"`.

`sun/nio/fs/WindowsFileSystemProvider`'s 11 candidate rows (§9.34's other
hypothesis) were not attempted this wave — they very likely construct or
receive `WindowsPath` receivers the same way, so the same `root` defect
should be expected there too, not re-discovered from zero.

Measured on **windows/x86_64 against JDK 25**.

### §9.36 follow-up -- the `root`/`type` gap fixed standalone; WindowsPath retirement tried again and blocked by a SECOND, deeper defect

The `root`/`type` gap §9.36 traced (`p57_classify_windows_path` folding a
thrown `InvalidPathException` into a silent `None`, leaving the slot at
Java's default `null` instead of the empty string real `WindowsPath.root`
always holds) is now fixed: `p57_write_path_fields`
(`../../../native-builtins/src/phases_late/nio_file.rs`) falls back to
`root=""`/`type=WindowsPathType.RELATIVE` (via the enum's own
`valueOf`, not a hand-rederived classification) whenever
`p57_classify_windows_path` cannot classify the input at all, instead of
leaving both fields unset. Landed standalone -- it is a real, general
correctness fix (no `WindowsPath` this VM mints is ever left with a
field real bytecode reads unconditionally sitting at `null`) independent
of whether `WindowsPath`'s instance methods are retired. Verified
inert-but-safe: full `cargo test -p cratonvm-native-api`, the 13-probe
lane-4 battery (only the same 3 pre-existing failures), and the
`L4FilesSweep`/`L4CensusTail`/`L4Reach`/`TailFamilySweep` oracle-diffs
all identical to the wave-10 baseline -- expected, since nothing yet
reads `WindowsPath.root`/`.type` via real bytecode without the
retirement itself.

**Retried the 19-row `WindowsPath` retirement with the fix in place.**
It got much further -- past the `"//"`/`getFileName` crash entirely,
through the whole `path[...]` sweep -- and hit a SECOND, unrelated
defect:

```text
Exception in thread "main" java/lang/IllegalArgumentException
    at L4FilesSweep.pathText(L4FilesSweep.java:152)
    at sun/nio/fs/WindowsPath.getName(WindowsPath.java:42)
    at sun/nio/fs/WindowsPath.getName(WindowsPath.java:686)
```

immediately preceded by `getName(0) |a\b\c|` — the WHOLE path string,
where HotSpot (and the whole point of `getName(0)`) answers just `a`,
the first component.

**Root cause, this time genuinely architectural, not a field-population
gap.** Real `WindowsPath` caches a lazily-computed `Integer[] offsets`
field (`javap`: `private volatile java.lang.Integer[] offsets`,
populated by `initOffsets()` on first name-indexing call), computed by
scanning `this.path` for separator characters. `initOffsets()`'s
separator scan is for `'\\'` — real `WindowsPath` always stores `\`
internally. This VM's `p57_alloc_path` deliberately does NOT: its own
doc comment states the choice outright — "Windows accepts '\' as a
separator and HotSpot's WindowsPath stores '\', but rendering '\' lives
at the `toString()`/`getPath()` display boundary... internally we keep
'/'". That choice is presumably load-bearing for this VM's OWN native
`WindowsPath` accessors (which are written against `/`-stored paths),
but it means real `initOffsets()`, run over a VM-stored `/`-separated
string, finds **zero** separators in ANY multi-component path — every
`getName`/`getNameCount`/`subpath`/iteration on a real `WindowsPath`
built by this allocator sees the WHOLE path as one name component. Not
a `"//"`-shaped edge case this time — this hits every multi-segment
Windows path.

This is NOT the same species as the `root` gap (a field left at its
Java default). It is two different internal representations of "the
same" state — this VM's native accessors keyed on `/`, real bytecode's
own lazily-cached `offsets` keyed on `\` -- colliding the moment real
bytecode is the one doing the reading. Fixing it properly is a VM-wide
question (does `p57_alloc_path` switch its internal convention to `\`
on Windows, at the cost of re-auditing every existing native reader of
`WindowsPath.path`, or does something else reconcile the two only where
real bytecode is reached) -- squarely the "own dedicated measurement
pass" this section's first draft already anticipated, not a quick
follow-up. `RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES` was reverted a second
time (`git checkout -- native-api/src/retired_shadow.rs`); the
`root`/`type` fix above was kept, since it is correct and safe
independent of this larger blocker.

Measured on **windows/x86_64 against JDK 25**.

### §9.36 final follow-up -- landed, 16 of the 19 rows, after finding the "own dedicated measurement pass" was two bugs, not one platform-wide rewrite

The "separator-convention mismatch" the prior follow-up named as needing
its own dedicated pass turned out, on closer measurement, to be TWO
separable things: most of `WindowsPath`'s methods read a lazily-cached
`offsets` field (fixable by pre-populating it correctly, no VM-wide
change needed); two methods (`getFileName`/`getParent`) instead do a
FRESH, direct scan with no cache to intercept. Telling those apart is
what turned an apparently VM-wide blocker into a bounded, landable fix.

**Bug 3 -- `"a//b"` (a legitimate path, not an edge case) came back
`nameCount=3`.** Real `WindowsPath.path` is always the ALREADY-NORMALIZED
string `WindowsPathParser.parse(..).path()` returns -- single separators
only, by construction -- so real `initOffsets()`'s naive per-character
scan never needs to handle a double separator; it cannot occur. This
VM's `p57_alloc_path` only trims a trailing separator and converts `\`
to `/`; it does not collapse an internal `a//b` into `a/b`. A literal
translation of `initOffsets()`'s algorithm over that uncollapsed string
counted the separator run's inner (empty) span as its own segment.
Fixed in `p57_windows_path_offsets`: a run of consecutive separators
only ever closes ONE segment. Every offset still indexes into the real,
uncollapsed stored string -- real accessors read
`path.substring(offset, nextSeparatorOrEnd)`, which is correct
regardless of what is between two offsets -- so this is a computation
fix, not a storage-format change.

**Bug 4 -- `getFileName()`/`getParent()` don't use `offsets` at all.**
`javap` on the whole class: exactly two `String.lastIndexOf`/`.indexOf`
call sites in `WindowsPath`, one in each of these two methods, and
nowhere else. Both scan `this.path` directly for the literal character
`'\\'` (92) EVERY call -- no cache, so no field this VM can pre-populate
intercepts it. Since `path` is stored with `/`, that scan finds nothing
in ANY multi-component path (not an edge case -- the common case) and
both methods fall through their "no separator found" branch, which
treats the ENTIRE remaining string as the answer:
`getFileName("a/b")` returned `"a/b"` instead of `"b"`, `getParent`
returned `null` instead of `"a"`. This is the one genuine piece of the
"VM-wide" blocker the prior follow-up was right to defer: fixing it for
real means changing what `path` is stored with, which every OTHER
native reader of `WindowsPath.path` across this codebase also assumes
is `/`-separated -- a real, its-own-measurement-pass change, not
resolved this wave.

**Bug 5 -- `toRealPath` reaches a genuinely missing Win32 native, found
only once the 13-probe battery (not just the 4 probes used to iterate
on bugs 1-4) was run.** `toRealPath`'s real bytecode chain
(`WindowsLinkSupport.getRealPath` → `WindowsNativeDispatcher.
GetFullPathName` → `GetFullPathName0`) hits one of the ~80
`WindowsNativeDispatcher` Win32 natives this VM has still never
implemented (wave 10 implemented exactly 2:
`GetFileAttributesEx0`/`CreateFile0`). Not a silent wrong answer --  a
loud `UnsatisfiedLinkError` -- but it broke `L4TrustPath`
(`java.security.KeyStore.getInstance` → `Security$SecPropLoader.
loadFromPath` → `WindowsPath.toRealPath`), a probe none of the
per-attempt measurements in this section had exercised. This is exactly
why the FULL battery, not just the probes a given bug happens to touch,
gates landing.

**Resolution: exclude all three rows (`getFileName`/`getParent`/
`toRealPath`), retire the other 16.** `javap`-confirmed no other method
in the class does its own separator scan (the two-occurrence count
above is exhaustive) and no other row in the 19-candidate set reaches
`WindowsNativeDispatcher` at all, so nothing else in the set can hit
either defect. `getFileName`/`getParent`/`toRealPath` stay on this VM's
own native implementation, unaffected by any of this wave's changes
since they were never touched. `RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES`
is now 16 rows.

**Measured**: `L4FilesSweep` (rc=0, matches the oracle exactly except
the same 4 already-documented pre-existing divergences), `L4CensusTail`
(rc=0, BYTE-IDENTICAL to its oracle, zero divergences -- the ones that
returned nonsense before bugs 3/4's fixes), `L4Reach` (rc=0), and the
full 13-probe lane-4 battery -- only the same 3 already-documented
pre-existing failures (`TailFamilySweep`, `L4PrintStreamSweep`,
`L4W6PrintCarrier`), `L4TrustPath` included and clean after excluding
`toRealPath`. Full `cargo test -p cratonvm-native-api` and
`doc_citation_paths`: ok.

**What this wave leaves for whoever picks up `getFileName`/`getParent`/
`toRealPath`, or the `WindowsFileSystemProvider` cluster §9.34 also
named**: for `getFileName`/`getParent`, assume the SAME question needs
asking first -- does the target method read a lazily-cached field
(fixable narrowly, as `offsets` was) or scan `path` directly for `'\\'`
(unfixable without the VM-wide storage-convention change)? `javap -c`
on the whole class, grepped for `lastIndexOf`/`indexOf`, answers it in
one pass rather than a crash-and-patch cycle per method. For
`toRealPath`, the blocker is a single missing Win32 native
(`GetFullPathName0`) in the same family wave 10 already implemented two
of -- likely the cheapest of the three to close.

Measured on **windows/x86_64 against JDK 25**.

### Wave 14 -- 2026-09-18, `WindowsPath.toRealPath` retired: the "single missing native" §9.36 predicted was four, and the landing exposed three sibling regressions that were not this lane's

§9.36's last paragraph called `toRealPath` "likely the cheapest of the three"
to close because it needed "a single missing Win32 native (`GetFullPathName0`)".
That was wrong by a factor of four, and the way it was wrong is worth
recording because it is the same mistake §9.36 itself warned about (fixing
the probes you iterate on, not the chain).

**The false start.** Implementing only `GetFullPathName0` and retiring the two
`toRealPath` rows made `L4TrustPath` pass in the debug build, and I stopped
there. The release-build 13-probe battery then died with
`UnsatisfiedLinkError ... FindFirstFile0`. `l4run.sh` sends stderr to
`/dev/null` by design, so the battery reported it as a bare rc=1 row-count
mismatch; the error only appeared when I ran the binary directly. The real
`WindowsLinkSupport.getRealPath` chain is:

* `GetFullPathName0` -- used only to normalise `.`/`..`; NOT the whole story.
* an ALWAYS-run per-component case-correction walk: `FindFirstFile0` +
  `FindClose` for every path component;
* `GetFileAttributes0(String)` for the final segment;
* a symlink-follow branch (`resolveAllLinks` -> `readLinkImpl`) that needs
  `DeviceIoControlGetReparsePoint`.

**What landed** (`../../../native-builtins/src/phases_late/nio_file.rs`, next to wave
10's two natives): `GetFullPathName0` `(J)Ljava/lang/String;`,
`GetFileAttributes0` `(J)I`, `FindFirstFile0`
`(JLsun/nio/fs/WindowsNativeDispatcher$FirstFile;)V`, `FindClose` `(J)V`, as
kernel32 FFI over the `NativeBuffer` path arguments (`GetFullPathNameW` is the
two-call form with no trimming; `INVALID_FILE_ATTRIBUTES` is turned into
`GetLastError`; `FindFirstFile0` fills `handle`/`name`/`attributes` on the
passed `FirstFile`; errors throw `sun/nio/fs/WindowsException(I)V`). The
extern `FindFirstFileW` had to match `win_case_correct`'s declaration exactly
or `-D clashing-extern-declarations` fails the build.
`RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES` is now **18 rows** (the two
`toRealPath` descriptors, kept in sorted order: `Ljava/...Path;` sorts before
`Lsun/...WindowsPath;`; an unsorted pair breaks the binary search and
`every_table_is_sorted_and_unique`). `getFileName`/`getParent` stay excluded
(§9.36).

**Scope caveat, stated plainly.** Retired for the NON-SYMLINK case. A path
that reaches the `resolveAllLinks` branch still needs
`DeviceIoControlGetReparsePoint`, which is not implemented; it fails loudly
(`UnsatisfiedLinkError`), not silently. `../../../apps/probes/L4ToRealPath.java` (new,
10 rows: abs, isAbsolute, dotted, dotted-equals, dotdot, upper-case, dir,
dir-isDirectory, missing -> `NoSuchFileException`, nofollow) uses a FIXED path
under the temp dir because `l4run.sh`'s sed only collapses `/tmp/<word><digits>`.

**Measured** (windows/x86_64, JDK 25, release build of the merged tree):
`L4ToRealPath` and `L4TrustPath` both DIFF strict=0 compat=0. The rest of the
battery is unchanged from wave 13: the diffs that remain are
`L4FilesSweep` (`path[//]`, `readAllBytes dir`, `copy self`), `L4W5Sweep`
(`createLink`/`createSymbolicLink` -> `UnsatisfiedLinkError`, view/stream class
names, directory count), `L4FfmLayoutSweep` (2 rows) and `L4AbsPath`, which
differs only by the random temp-file name it prints. `jdk-only-refusal-survivors`:
0 rows, matches baseline (4612 refusals). Regression suite: `SUITE=all` 136/136;
`SUITE=core` 94/95 with `RMapGcStress` a harness TIMEOUT (rc=124) that passes
alone at `TIMEOUT=600` (host load); `--jdk-only` 133/136, the three failures
(`RNioNoFollow`, `RJdkNio`, `RBigIntMontgomery`) being present in the pre-wave
baseline (104/136 before wave 14 had them in a list of 32).

**Two hazards worth keeping.**
1. `../../../regression-suite/run.sh` writes fixed shared `BUILD`/`MODBUILD` directories.
   Two arms run concurrently corrupt each other; the three arms MUST be
   sequential.
2. This host misreports timing-sensitive tests under concurrent cargo load
   (`init_level` timing test, native-builtins `--lib` under `management`,
   `RMapGcStress`); confirm any such failure in isolation before believing it.

**Three sibling regressions this landing found and fixed** (all on the merged
tree, none touching `toRealPath`; each root-caused and committed separately):
`5fa6253fe` (`real_provider_cache` was a raw `parking_lot` lock, tripping the
lock-discipline ratchet at 428 > 427: now an `OrderedPlMutex` at
`LockLevel::Scratch`); `270d497d8` (a sibling's `estimateSize` change returned
`Long.MAX_VALUE` for the `ConcurrentHashMap` `KeySetView` spliterator, which is
unsized but CONCURRENT with a real estimate); `68a1d38b6` (`flag-inventory.md`
count 1,455 -> 1,456 and a `flag_declaration_guard` row for a sibling test's
re-exec marker `CRATONVM_TEST_NPE_OPTIMIZING_BODY_CHILD`). `../../../ARCHITECTURE.md`'s
native-api LoC row (53,000 -> 56,000) was also stale from wave 13; wave 14
brings the crate to 55,774. Two further gate failures were confirmed
pre-existing by stash repro and are NOT this lane's: `unconstructed_carrier_gate`
and `stub_ratchet` (flagged as a separate task); an intermittent JIT segfault in
`RSslLiveSession` (flagged; it did not recur on the final tree).

**Diagnosed while measuring, not fixed here -- the actual state of the
separator-convention blocker.** `RJdkNio` (`dir.relativize(sub)` returns
`..\..\..\..\..\..\Users\...`) and `RNioNoFollow` are the visible symptom, and
the mechanism is now pinned by dumping the fields through reflection: a path
PARSED by this VM is stored `C:/U/x/q` with `offsets` populated, while the
bytecode `WindowsPath.resolve` that wave 13 retired builds `C:/U/x\q`
(`path + "\\" + other`) with `offsets=null`. `toString()` hides the difference;
`equals`, `startsWith` and `relativize` compare the raw strings and disagree
(`q.equals(Path.of("C:\U\x\q"))` is `false` on this VM, `true` on HotSpot).
This is the same `/`-vs-`\` storage convention §9.36 deferred for
`getFileName`/`getParent`, and it is broader than that note said: it also
breaks the arithmetic wave 13 retired.

Measured on **windows/x86_64 against JDK 25**.

### Wave 15 -- 2026-09-19, the separator convention fixed at its root: `WindowsPath.path` now holds what the JDK's own class holds, `getFileName`/`getParent` retire, and two suite vectors that had been red since before wave 14 go green

Wave 14's last section pinned the mechanism; this wave fixes it. The
decision that mattered was WHERE. §9.36 called the storage convention
"VM-wide" and deferred it, and the deferral was sound while the only
readers were this VM's natives: they all assume `/`. The way out was to
leave every native reader's view alone and change only what the SLOT holds.

**The design (`../../../native-api/src/path_layout.rs`).** Two functions, one
pair of rules. `stored_form(text)` is what a Path's `path` slot HOLDS: on
Windows, `\`-separated, exactly as the real `WindowsPath` stores it;
identity everywhere else and identity for a jar/jrt sentinel string
(`\u{1}`-prefixed, opaque). `canonical_form(text)` is its inverse, applied
by every native READER of the slot, so the rest of the VM keeps seeing
one `/`-canonical spelling. A Windows filename can never contain `\`, so
both directions are loss-free there. Writers: `p57_write_path_fields`
(`native-builtins`) and both `native-io` producers. Readers:
`p57_path_slot_string`, `native-io`'s `path_slot_string` and
`read_path_str`.

**What that repaired, by measurement.** `RJdkNio`'s
`dir.relativize(sub)` returned `..\..\..\..\..\..\Users\...` on this VM
and `a\b\c` on HotSpot. Reflection over the fields (a throwaway probe,
kept in the session scratchpad only) showed the mechanism: a parsed path
was stored `C:/U/x/q` with `offsets` populated, and the bytecode
`WindowsPath.resolve` that wave 13 retired builds `path + "\\" + other`,
i.e. `C:/U/x\q` with `offsets=null`. `toString()` hid the difference;
`equals`, `startsWith` and `relativize` compare the raw strings and
disagreed (`q.equals(Path.of("C:\\U\\x\\q"))` was `false`).

**Three follow-on defects the new probe found, none of which the
`RJdkNio` vector would have.** `../../../apps/probes/L4PathShapes.java` is 27
input shapes (relative, rooted, drive-relative `C:x`, UNC, `..`, `.`,
`a//b`, `a\\b`, trailing separators, spaces, non-ASCII) x 13 accessors,
409 lines against HotSpot.

1. *Uncollapsed separator runs.* `p57_alloc_path` only trims a trailing
   separator, so `a//b` stored uncollapsed and hashed/compared unlike
   its parsed twin. The fix is to STOP re-deriving what the real parser
   already computes: `p57_write_path_fields` now classifies FIRST via
   `WindowsPathParser.parse` and stores the parser's own `path()` --
   runs collapsed, a UNC root closed with `\` (`\\srv\share\`). The
   classification pin lives on the pin stack for the whole function
   (`unpin_native_roots` is stack-shaped: unpinning the outer pin
   releases it).
2. *Offsets were UTF-8 byte offsets.* `p57_windows_path_offsets` walked
   `path.as_bytes()`; `offsets` are indices into a Java `String`. A
   non-ASCII name (`\u00e9\\\u00e8`) pushed every later offset past the
   end and the real `WindowsPath.normalize` died with
   `StringIndexOutOfBoundsException`. Now UTF-16 code units. This bug
   was latent since the wave-13 offsets work; nothing had a non-ASCII
   path to expose it.
3. *`Path.toString` trimmed a UNC root's closing `\`.* Both native
   `toString` registrations went through `file_normalise_path`. On
   Windows they now return the slot as stored, which is what the real
   method returns.

**Rows.** `RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES` 18 -> **21**:
`getFileName()Ljava/nio/file/Path;`, `getParent()Ljava/nio/file/Path;`,
`getParent()Lsun/nio/fs/WindowsPath;` (the covariant bridge, as
`toRealPath` had). `L4PathShapes` is DIFF strict=0 over all 409 lines.

**Why `RNioNoFollow` had to be worked at the same time.** It was red
before wave 14 (present in the 104/136 baseline) and the failure was not
in its NIO logic: `Files.createSymbolicLink` reached
`WindowsNativeDispatcher.CreateSymbolicLink0`, which this VM had never
implemented, and the vector's own guard is `catch (Exception)` -- an
`UnsatisfiedLinkError` is an `Error`, so the bail-out that exists for a
host without the symlink privilege never ran. Implementing the native
turns the failure back into what HotSpot reports on this host
(`FileSystemException`, privilege not held), which took three more
natives in the same chain before it went green, each found only by
running the vector to the next `UnsatisfiedLinkError`:

* `CreateSymbolicLink0`, `CreateHardLink0` (link surface);
* `FormatMessage(I)` -- every FAILING call asks for the system's error
  text before `translateToIOException` picks the exception type, so the
  first failure of anything in the class dies here (JNI body
  reproduced: 255-unit buffer, `FORMAT_MESSAGE_FROM_SYSTEM`, NULL when
  the code has no text, trailing `\n`/`\r`/`.` dropped when longer than 3);
* `GetFileInformationByHandle0`, `GetFileSizeEx`, `CloseHandle` -- the
  first HANDLE-taking natives. A `HANDLE` Java holds is an fd_table id
  (`CreateFile0` absorbs the real one; see `FdTable::insert_win32_file`),
  so each resolves the id through `fd_table().clone_file`.
  `FileChannel.open` reaches them because
  `WindowsFileSystemProvider.newFileChannel` runs as bytecode now.

Also added while the path-only pattern was in hand, so the next wave does
not have to: `DeleteFile0`, `RemoveDirectory0`, `CreateDirectory0`,
`MoveFileEx0`, `CopyFileEx0`, `SetFileAttributes0`. They share one body
(`win32_path_native_body`) behind a macro, because `register` takes a
plain `fn` pointer and cannot capture a name or an arity.
`CreateDirectory0` refuses a non-zero security-descriptor argument rather
than creating the directory with default permissions; `CopyFileEx0`
passes a NULL cancel flag. **None of the six is reached by a probe in
this wave**, so they are implemented and compile, and are untested beyond
the shared body; treat the first real caller as their measurement.

The `-D clashing-extern-declarations` lint bit again: `win_file_identity`
declared `GetFileInformationByHandle` over a local struct pointer, this
wave declares it over `*mut u8`. The older declaration was changed to
match.

**Measured** (windows/x86_64, JDK 25, debug binary unless stated):
`L4PathShapes` strict 0/409; `RJdkNio` PASS, `RNioNoFollow` PASS (22
checks) through the suite harness; a createSymbolicLink + createLink
probe prints the same two lines on both VMs. Gates: `native-api --lib`
468/468; `cratonvm-types` all green including `doc_numeric_claims`;
`native-io` 529 passed; `native-builtins --tests` in default,
`management` and `synthetic-jdk` -- only the two failures that predate
this wave (`stub_ratchet::synthetic_stub_count_does_not_regress`,
`unconstructed_carrier_gate` for `java/nio/HeapCharBuffer`), unchanged.

**Release build** (the cargo gates above were run on the same tree;
release measured after them). Regression suite, the three arms strictly
sequential: `--jdk-only` **134/136**, `SUITE=all` **135/136**,
`SUITE=core` **94/95**. Every failure is a harness or intermittent one and
none is a path or NIO vector:

* `RBigIntMontgomery` -- harness TIMEOUT (rc=124) in the `--jdk-only`
  arm, and it still times out ALONE at `TIMEOUT=600`. It was in the
  pre-wave failure list (133/136) and is not touched by this wave.
* `RMethodSiteCache` -- `EXCEPTION_ACCESS_VIOLATION` (rc=139) in the
  `--jdk-only` arm, PASSES alone. The intermittent-JIT-crash family that
  wave 14 flagged; not attributable to this wave from one occurrence.
* `RMapGcStress` -- harness TIMEOUT in both the `SUITE=all` and
  `SUITE=core` arms (it also timed out in wave 14's core arm), PASSES alone
  at `TIMEOUT=900`. Three consecutive full-arm timeouts is more than one
  flake and is worth someone measuring on a quiet host; it is a
  collection-stress vector, not a filesystem one.

Wave 14's `--jdk-only` arm was 133/136 with `RNioNoFollow`,
`RBigIntMontgomery` and `RJdkNio` red; this wave clears the two NIO
ones. Probe battery (17 probes, strict/compat diff lines): `L4TrustPath`
0/0, `L4ToRealPath` 0/0, `L4PathShapes` 0/58 (the compat residual named
below), `L4FileSweep`/`L4ByteBufferSweep`/`L4CharBufferSweep`/
`L4TypedBufferSweep`/`L4PrintStreamSweep`/`L4StreamTailSweep`/
`L4TailSweep2`/`L4BridgeSweep`/`L4W6PrintCarrier` 0/0, `L4Reach` 0/2,
`L4CensusTail` 0/2, `L4AbsPath` 8/8 (the probe prints a random temp-file
name), `L4FilesSweep` 16/18, `L4FfmLayoutSweep` 4/56, and **`L4W5Sweep`
6/14, down from 16/14** -- the link natives turned its `createLink =
UnsatisfiedLinkError` row into the oracle's `ok`. Nothing got worse.
`jdk-only-refusal-survivors`: 0 rows, matches baseline (4614 refusals).

**What is still not done, stated so it is not read as done.**

* *Compat mode is unchanged and still differs* on `L4PathShapes` (58
  lines): the native `getFileName` of a bare drive (`C:`) and of a UNC
  root answer `C:`/`share` where HotSpot answers `null`, and the native
  `hashCode` is not case-insensitive (`a` hashes 97; HotSpot's
  `WindowsPath.hashCode` folds case: 65). Retirement is how this lane
  closes a native, and `--jdk-only` is where the tables apply; the
  compat natives were not rewritten.
* *Symlink follow.* `DeviceIoControlGetReparsePoint` is still
  unimplemented. On a host WITH the privilege, `RNioNoFollow`'s
  `symlinkArms()` would reach it; on this host those arms are skipped on
  both VMs, so the vector's check count (22) matches the oracle without
  exercising the branch. That is exactly the failure mode the vector's
  own header warns about, and it is not closed here.
* *The rest of `WindowsNativeDispatcher`.* Roughly 65 natives remain:
  directory streams (`FindFirstFile1`, `FindNextFile0`), `SetFileTime0`,
  `SetEndOfFile`, `DeviceIoControlSetSparse`, volume/drive queries, the
  ACL/security-descriptor/token family, and the IOCP/`ReadDirectoryChangesW`
  async family. The `WindowsFileSystemProvider` cluster §9.34 named needs
  most of the first group.

Measured on **windows/x86_64 against JDK 25**.

### Wave 16 -- 2026-09-19, the Windows filesystem cluster: thirteen more `WindowsNativeDispatcher` natives, 23 rows over six classes, and the eager `path_layout` resolve that real bytecode had been missing

Wave 15 ended by naming the cluster §9.34 had blocked on: directory streams,
volume/drive queries, `SetFileTime0`, `SetEndOfFile`, sparse and reparse
`DeviceIoControl`. This wave implements those, retires the classes that were
waiting on them, and finds one initialisation-order defect on the way.

**The natives** (`../../../native-builtins/src/phases_late/nio_file.rs`,
`win32_register_fs_natives`, kernel32 FFI): `FindFirstFile1`, `FindNextFile0`
(the first fills the caller's `FirstFile` out-param; the search HANDLE is a
raw i64 because nothing in the fd_table owns it), `GetVolumePathName0`,
`GetVolumeInformation0`, `GetDriveType0`, `GetDiskFreeSpaceEx0`,
`GetDiskFreeSpace0`, `GetLogicalDrives`, `SetFileTime0`, `SetEndOfFile`,
`DeviceIoControlSetSparse`, `DeviceIoControlGetReparsePoint`,
`GetFinalPathNameByHandle`. Path arguments are NativeBuffer addresses read as
NUL-terminated UTF-16; failures throw `WindowsException(I)V`; out-param
objects are written with `set_field_by_name` under a pin (the allocation of
the name string is a GC point). Java-held HANDLEs are fd_table ids, so the
handle-taking ones resolve through `clone_file` / `file_size`, as wave 15's
did. Shared argument decoding lives in `win32_long_arg`, `win32_wide_arg` and
`win32_handle_file`; `win32_find_first_file` now returns the whole
`WIN32_FIND_DATA` tuple and `win32_find_next_file` reuses it.

**The defect: `Path.toString()` was empty for every path real bytecode
created.** `path_layout`'s slot table (`PathSlots`, a process global) was only
resolved lazily from the first native that took a `&mut` context. The
`&dyn NativeContext` readers (`p57_path_slot_string` and `native-io`'s
`path_slot_string`) cannot resolve it, so a `WindowsPath` built by
`WindowsPathParser`/`WindowsFileSystem.getPath` bytecode -- which is every
path once `WindowsFileSystem` retires -- read back as `""` until something
else happened to touch the table first. `L4FilesSweep`, `L4TrustPath` and
`L4PathShapes` all crashed on it in the first debug run. `p57_alloc_default_filesystem`
now calls `p57_path_slots_init(ctx)` before it returns the carrier, so the
table exists before any bytecode path can. The `path_layout` decision of wave
15 is unchanged; this only fixes when the table is filled.

**Rows.** New `RETIRED_SHADOW_L4_WINFS_TRIPLES`, **23 rows**, registered in
`RETIRED_SHADOW_TABLES`, sorted, with `wave_sixteen_winfs_is_twentythree_rows`:

* `WindowsDirectoryStream`: `close`, `iterator`;
* `WindowsFileStore`: `getBlockSize`, `getTotalSpace`;
* `WindowsFileSystem`: `close`, `getPath`, `getPathMatcher`,
  `getRootDirectories`, `getSeparator`, `isOpen`, `isReadOnly`, `provider`,
  `supportedFileAttributeViews`;
* `WindowsFileSystemProvider`: `getFileAttributeView`, `getFileStore`,
  `getPath(URI)`, `getScheme`, `isHidden`, `isSameFile`, `newByteChannel`,
  `newDirectoryStream`, `readAttributes`;
* `WindowsPath.toString`.

**Blocked or held, with the reason, so they are not read as forgotten.**

* `WindowsFileSystemProvider.newWatchService` -- needs the IOCP /
  `ReadDirectoryChangesW` async runtime; this VM has none.
* `WindowsFileSystemProvider.checkAccess` -- reaches `WindowsSecurity` (token,
  `AccessCheck`), the security-descriptor family, none of which is implemented.
* `WindowsFileAttributes.fileKey` -- HELD on purpose. An earlier draft of this
  table had 24 rows including it; the existing guard test
  `the_held_windows_attribute_triple_is_not_retired` failed on it. The row was
  dropped (24 -> 23) and the new table's test asserts it stays out.

**Two guard tests were wrong about the tree, not the tree about them.**
`a_prefix_alone_retires_nothing` used `WindowsPath.toString` as its
"not retired" example; this wave retires it, so it now uses
`("sun/nio/fs/WindowsPath","notAMethod","()V")`. The fileKey triple above was
the other. Both were found by the `native-api --lib` gate and not by any
probe; that is what the guards are for.

**Untested natives, stated as such.** No per-native invocation counter was
taken in this wave, so "reached" is inferred from the probes' behaviour
(directory listing, file-store and time/size queries in `L4FilesSweep` and
`L4W5Sweep` moved from diff to match), not counted. Those two probes do not
call `GetDiskFreeSpace0`, `DeviceIoControlSetSparse` or
`DeviceIoControlGetReparsePoint`; the last is the primitive wave 15 named for
`RNioNoFollow`'s symlink-follow arm, which is still skipped on this host (no
symlink privilege) on both VMs. Treat the first real caller of any native
listed above that a probe does not demonstrably exercise as its measurement.

**Measured** (windows/x86_64, JDK 25). Debug binary, strict diff lines against
HotSpot: `L4FilesSweep` **4, down from 16**, `L4W5Sweep` **4, down from 6**,
`L4Reach` 0, `L4CensusTail` 0, `L4TrustPath` 0, `L4ToRealPath` 0,
`L4PathShapes` 0 (compat 58, unchanged residual from wave 15), `L4FileSweep` 0,
`L4AbsPath` 8 (random temp-file name, as before); `RJdkNio` and `RNioNoFollow`
PASS. Gates: `cratonvm-types` and `native-io` green; `native-api --lib`
469/469; `native-builtins --tests` in default / `management` /
`synthetic-jdk` only the two failures that predate wave 15
(`stub_ratchet::synthetic_stub_count_does_not_regress`,
`unconstructed_carrier_gate` for `java/nio/HeapCharBuffer`).

**Release build, regression suite, three arms strictly sequential:**
`--jdk-only` **135/136** (only `RBigIntMontgomery`, the harness TIMEOUT that
predates this lane's waves and fails alone as well), `SUITE=all` **136/136**,
`SUITE=core` **95/95**. Wave 15's `RMethodSiteCache` and `RMapGcStress` red
did not recur in this run; one clean run is not evidence they are fixed.
Probe battery on the release binary (strict/compat diff lines): `L4TrustPath`
0/0, `L4ToRealPath` 0/0, `L4PathShapes` 0/58, `L4FileSweep`, the buffer,
`PrintStream`, `StreamTail` and `W6PrintCarrier` sweeps 0/0, `L4Reach` 0/2,
`L4CensusTail` 0/2, `L4AbsPath` 8/8 (random temp name), `L4FilesSweep` **4/18
(was 16/18)**, `L4FfmLayoutSweep` 4/56, `L4W5Sweep` **4/14 (was 6/14)**.
`L4TailSweep2` and `L4BridgeSweep` exit 1 with no output on ALL THREE arms
including HotSpot, so in this run they compare nothing (not diagnosed here;
any earlier "0/0" recorded for them is not coverage). `jdk-only-refusal-survivors`:
0 rows, matches baseline (refusals seen 4636, up from 4614 in wave 15). The
four `L4FilesSweep` strict lines that remain were not re-diagnosed this wave;
wave 15's notes attributed the `Files`-level residue to `readAllBytes` on a
directory, `copy` onto itself and `newInputStream`/`newOutputStream` class
names, which is the wave-18 work.

**What is still not done.** The compat-mode residuals of wave 15 stand;
compat mode's natives were not rewritten. Still un-retired in this lane, and each with a named reason
rather than a number: `java/io/WinNTFileSystem` (eleven wrappers over `*0` JNI
natives this VM does not implement), `java/nio/file/Files` (13 rows plus
`FileSystems`, `FileVisitResult`), the buffer classes, `CoderResult`,
`FileDescriptor$1`, `NativeThreadSet`; and the security / ACL / IOCP natives.

Measured on **windows/x86_64 against JDK 25**.

### Wave 17 -- 2026-09-19, `java.io.WinNTFileSystem`, all of `java.nio.file.Files`, the Windows provider/store rows and 65 buffer-family rows: 134 rows, and the four `Files` methods that recursed until their provider halves retired with them

Wave 16 left the Windows provider stack executable as bytecode. This wave
retires what sits above it and measures it. Four new tables in
`../../../native-api/src/retired_shadow.rs`, all registered in
`RETIRED_SHADOW_TABLES`:

| Table | Rows | Content |
|---|---|---|
| `RETIRED_SHADOW_L4_WINNTFS_TRIPLES` | 11 | the Java wrappers of `WinNTFileSystem` (`list`, `getLength`, `getBooleanAttributes`, `checkAccess`, `createDirectory`, `createFileExclusively`, `getLastModifiedTime`, `setLastModifiedTime`, `setPermission`, `setReadOnly`, `getSpace`); the `*0` JNI natives they call were already registered and stay |
| `RETIRED_SHADOW_L4_FILETMP_TRIPLES` | 1 | `File.createTempFile(String,String)` |
| `RETIRED_SHADOW_L4_FILES2_TRIPLES` | 57 | `Files` (32), `FileVisitResult` synthetics (2), `FileSystemProvider.newInputStream`/`newOutputStream` (2), `WindowsFileSystemProvider` (9), `WindowsFileStore` (10), `WindowsFileSystem.getFileStores`, `WindowsDirectoryStream.spliterator` |
| `RETIRED_SHADOW_L4_BUFFERS2_TRIPLES` | 65 over 20 classes | `DirectByteBuffer` (14), `Buffer` (4), `FileDescriptor$1` (6), `CoderResult` (5), typed-buffer `clear`/`get`/`put` (15), `NativeThreadSet`, `HeapByteBuffer`, `Buffer$2`, `BufferedOutputStream`, `ByteOrder`, `Util`, `FileChannelImpl.open` and `$Closer.run`, and singles |

One native fix in `../../../native-io/src/nio_native.rs`: the Windows spelling
`write0(fd, addr, len, boolean append)` ignored `append`, so an append-mode
`FileChannel` overwrote from position 0. It now seeks to end-of-file first
(`Files.write(p, b, APPEND)` was the row that showed it). The seek is not
atomic with the write.

**The funnel was wrong, and that is why the number moved.** The candidate
script keyed records by triple and kept the first one seen. A triple can
have two records in one dump -- a shadowed duplicate (`owns_slot: false`,
`invocations: 0`) and the owning one -- so the owning row's invocations were
merged into a record that was then discarded by the `owns_slot` filter. It
reported 13 `java/nio/file/Files` rows; there are 32. The script now lives
in `../../../apps/probes/l4funnel.py` with the fix.

**Retiring all of `Files` crashed at startup, and the reason was not in
`Files`.** `EXCEPTION_STACK_OVERFLOW` on the first `Files.createDirectory`.
`CRATONVM_UNRETIRE_NATIVE_SHADOW` bisects in runs: retire everything, then keep
exactly one `Files` method retired at a time. Only `copy`, `createDirectory`,
`delete` and `move` overflowed. `concrete_receiver.rs:185` registers
`WindowsFileSystemProvider.{copy,createDirectory,delete,move}` as natives that
DELEGATE to the matching `Files` native (`nio_file.rs`, "one body per
operation"), so `Files.delete` as bytecode called `provider.delete`, which is
that native, which called `Files.delete`. Retiring the provider halves in the
same table turns the loop into the JDK body -- `CreateDirectory0`,
`DeleteFile0`/`RemoveDirectory0`, `MoveFileEx0`, `CopyFileEx0`, the wave-15
natives that had no caller until now. They never appeared in the funnel
because nothing invoked them while `Files` was native (`invocations: 0`).

**Rows that carry an observation, found by the measurement and not by
reading:**

* `File.deleteOnExit` -- retired, `L4FileTempExit` showed HotSpot deleting
  the file, the directory and its child at exit while this VM left all three.
  The JDK body registers `DeleteOnExitHook` through
  `JavaLangAccess.registerShutdownHook`; the VM does not run `Shutdown`'s
  hook slots (`shutdown hooks: ran=0`). Reverted to native, blocker named.
* `Bits.reserveMemory` -- retired in the first release build, and
  `RBufferPoolCount` failed (`countMoved=false`, `usedMoved=false`, both
  routes). The mechanism is inferred, not traced: `bits_reserve_memory`
  (`../../../native-io/src/direct_buffer.rs`) is the VM's own accounting, and the pool
  MXBean's count did not move once the bytecode version ran instead. Row
  dropped; the vector passes with it native (debug binary, then the final
  release arms).
* `FileSystems.newFileSystem` (both overloads) -- retiring them turned
  `newFileSystem(Path, ClassLoader)` on a text file from
  `ProviderNotFoundException` into "no throw" in `L4CensusTail`; they lean on
  `installedProviders`, which stays native (wave 9).
* `WindowsFileSystemProvider.getFileSystem(URI)` -- retired alone it returns a
  filesystem that is not `FileSystems.getDefault()`, which is still the native
  carrier: `getFileSystem(file:///) == getDefault()` became `false` in
  `L4WinStoreAttrs`. Row dropped; `getDefault` is the identity anchor.

**Three older guards were re-read rather than edited to pass.**
`the_provider_family_retirement_is_four_of_nine_bucket_ab_rows` held
`FileSystemProvider.newInputStream`/`newOutputStream` out for a capability-gate
reason; retiring them is what turns the class name from `FileInputStream` to
`ChannelInputStream` (the last `L4W5Sweep` residue) and no vector went red, so
they are out of the held list, with a pointer. The typed-buffer guard said no
wave had measured `java/nio/Buffer`; it now allows exactly `position`, `limit`,
`capacity` and `<init>`. `the_phase2_wave_retires_only_the_triple_it_measured`
asserted `FileChannelImpl.open` stays native because retiring it had been
inert; it is reached now (6 invocations across the lane-4 probe census), so the assertion
flipped with the reason written next to it.

**`unconstructed_carrier_gate` flagged a new class, `java/io/WinNTFileSystem`.**
Its constructor writes `slash`, `semicolon`, `altSlash`, `userDir` from system
properties, so the precondition is met; `javap -p -c` on the 11 retired wrappers
shows none reads an instance field (each is `getFileForWin32Calls` + the `*0`
native). Baselined with that verdict and its expiry: retiring `normalize`,
`resolve`, `getSeparator` or `getDefaultParent` voids it. The gate's other
failure, `java/nio/HeapCharBuffer`, is not this lane's and predates it.

**Measured** (windows/x86_64, JDK 25). Debug binary, strict diff lines vs
HotSpot, before -> after the wave: `L4FilesSweep` 4 -> **0** (`readAllBytes` on a
directory now `AccessDeniedException`; `copy` onto itself no longer throws),
`L4W5Sweep` 4 -> **0**, new `L4WinStoreAttrs` 16 -> **0** (FileStore attributes,
`getFileStores`, `dos:*` attribute map, `toString`, `spliterator`,
`newFileSystem(file:///)`), new `L4FileTempExit` matches HotSpot with
`createTempFile` retired and `deleteOnExit` native.

**Release build, regression suite, three arms strictly sequential:**
`--jdk-only` **135/136** (only `RBigIntMontgomery`, the harness TIMEOUT that
predates this lane's waves), `SUITE=all` **136/136**, `SUITE=core` **95/95**. The
first release build, which still had `Bits.reserveMemory` retired, scored
`--jdk-only` 134/136 (`RBufferPoolCount` and `RBigIntMontgomery`); the arms above
are on the rebuilt final tree. Probe battery (strict/compat diff lines):
`L4TrustPath` 0/0, `L4ToRealPath` 0/0, `L4PathShapes` 0/58, `L4FileSweep`, the
buffer, `PrintStream`, `StreamTail` and `W6PrintCarrier` sweeps 0/0, `L4Reach`
0/2, `L4CensusTail` 0/2, `L4AbsPath` 8/8 (random temp name), **`L4FilesSweep`
0/18 (was 4/18)**, `L4FfmLayoutSweep` 4/56, **`L4W5Sweep` 0/14 (was 4/14)**,
**`L4WinStoreAttrs` 0/36**. `jdk-only-refusal-survivors`: 0 rows, matches
baseline (refusals seen 4789, from 4636).
Gates: `cratonvm-types` and `native-io` green; `native-api --lib` 473 passed;
`native-builtins --tests` in default, `management` and `synthetic-jdk`: only the
two failures that predate this lane (`stub_ratchet::synthetic_stub_count_does_not_regress`,
`unconstructed_carrier_gate` for `java/nio/HeapCharBuffer`).

**A sibling's measurement removed two rows after the merge.** The first cut of
the buffer table also retired `Buffer.session()` and `Buffer.checkSession()`, and
was green on the whole suite and every probe. `origin/dev` then arrived with
`the_jotp_wave_refuses_the_buffer_session_pair`, whose doc comment records that
a segment-backed buffer's real `session()` runs against this VM's own arena
carrier (`NoSuchMethodError` on `ArenaImpl.checkValidStateRaw`; netty's
`PcapWriteHandlerTest` 17 failed, `AdaptiveBigEndianDirectByteBufTest` 76
failed). Nothing in this lane's probes or the suite builds a segment-backed
buffer, so no arm here could have caught it. The pair is out of the table
(67 -> 65 rows, 136 -> 134 in total). `javap -p -c` shows the rows that remain
(`Buffer$2.acquireSession`/`releaseSession`, the `DirectByteBuffer` accessors)
reach the session only via `Buffer.session()`, which stays the native shim, so
they should not reach the carrier -- an argument from the bytecode, **not a
measurement**. After the merge the tree was rebuilt (release) and re-measured
in part: `--jdk-only` **135/136** (only `RBigIntMontgomery`) and the 17-probe
battery with numbers identical to the pre-merge ones above; `SUITE=all` and
`SUITE=core` were NOT re-run on the merged tree, and neither netty test was run:
those are still owed.

**What this does not establish.** The buffer-family table (65 rows) is backed
by the existing sweeps plus the suite arms, and §4 requires probes that read
data back; that was not re-audited row by row. `jdk/internal/foreign` was not
touched. The remaining un-retired rows and their blockers, and the gap that
`invocations > 0` leaves in the funnel, are in
[`lane-4-handoff-20260919.md`](lane-4-handoff-20260919.md). The lane is not at
§8's Done.

Measured on **windows/x86_64 against JDK 25**.
