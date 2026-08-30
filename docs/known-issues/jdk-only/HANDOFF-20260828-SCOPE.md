# `--jdk-only`: total scope, and seven parallel lanes — 2026-08-28

**Read this before your lane doc.** It has the whole picture, the method, the
rules that keep seven workers from colliding, and the traps that have each cost
a build cycle today.

Lane docs: `HANDOFF-20260828-L1-unsafe.md` … `L7-definition-of-done.md`. A
finished lane's brief moves to `internal/jdk-only/` with a retirement banner —
L4's and L7's have. **Both came BACK into this directory when the briefs were
first landed on `dev`, after their lanes had already retired them**, so a
duplicate here is a merge artefact rather than a live brief: check
`internal/jdk-only/` for a retired twin before working from one. Both have since
been removed again.

---

## 0. Who is where — CHECK THIS FIRST

| lane | owner | worktree | branch |
| --- | --- | --- | --- |
| **L5 reflection & class metadata** | **COMPLETE 2026-08-29** — dispatch worklist 483 rows / 20 fixed, PLUS its two recorded-open items and five more the probe found: 125 rows, **28 defects over 608 rows**, 1 residual (`invoke`'s reference-argument cast). Records: `L5-reflection-lane-complete-20260828.md` and `L5-residuals-module-packages-and-invokeexact-20260828.md` | `C:\craton\cratonvm\.claude\worktrees\h2-known-issues-206dee` | `claude/jdk-only-mode-handoff-09b48c` |
| **L2 StringBuilder / StringBuffer / AbstractStringBuilder** | **DONE 2026-08-29** — 118 native-won triples, 747 probe rows 0-diff in BOTH modes, 18 defects in 5 root causes, 62 `StringBuffer` shadows retired to the class's own synchronized bodies. Closes `WORKER-3-NOTE-3` N1 and N2 and refutes its §5. The `StringBuilder` retirement is SIMULATED green (armed corpus 111/112, armed probe 0-diff) and priced at **2.0x-3.4x**, so it is declined with a number. Lane doc retired to `internal/jdk-only/`; record is `l2-strings-eighteen-defects-five-root-causes-and-the-writer-half-20260828.md` | `/data/cvm-l2s-20260828` (Linux build host) | `claude/l2-strings-20260828` |
| **L4 `java.io` / `java.nio`** | **COMPLETE 2026-08-28** — 199 native-won triples, **1616 probe rows, 1615 identical in both modes**; 52 defects fixed and 8 shadows retired; 1 recorded residual (`FileInputStream.skip`, a resolution finding no registrar edit can move). Lane doc retired to `internal/jdk-only/`; record is `L4-the-io-and-nio-worklist-49-defects-and-a-bounds-check-that-killed-the-vm-20260828.md` | `/data/cvm-l4io-20260828` (Linux build host) | `claude/l4-io-nio-20260828` |
| **L7 definition of done** | **DONE 2026-08-29** — all three workloads run to completion under `--jdk-only`; `compatibility_classes: 0` and `synthetic_stub_invocations: 0` on five arms and on **181 H2 test classes**. Four VM fixes, none of them a `--jdk-only` defect. All 4 residuals discharged: 2 fixed, 1 verified will-not-fix, 1 measured at 7 sites and handed on as a lane. Then the two Phase 4 items nobody had run: **P4-A** a corpus (218 classes, both arms), now FULLY ADJUDICATED — **185 pass / 19 fail / 14 unresolved**, and **zero failures `--jdk-only` produces that compatible mode does not**; the single candidate for one was the harness (three concurrent `--Xmx 1g` shards OOM'd the strict arm; alone it passes). Phase 2's worklist is **1065** native-won triples, not the 334 five probes saw; **P4-B** `--features synthetic-jdk` built and run for the first time. Instrument gap closed: 53 classes handed back that `new` could not produce, on runs reporting `compatibility_classes: 0`. Lane doc retired to `internal/jdk-only/`; records are `the-definition-of-done-run-on-the-three-real-workloads-20260828.md`, `the-four-residuals-two-closed-one-was-a-family-of-thirty-and-one-is-a-lane-20260829.md`, `P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md` and `P4B-synthetic-jdk-mode-run-for-the-first-time-20260829.md` | `/data/cvm-l7dod-20260828` (Linux build host) | `claude/l7-dod-20260828` |
| **L1 `Unsafe`** | **DONE 2026-08-28** — 516 probe rows, 24 defects fixed, 5 recorded residual categories. Lane doc retired to `internal/jdk-only/`; record is `l1-unsafe-516-rows-24-defects-and-the-sub-word-atomics-that-never-returned-20260828.md` | `/data/cvm-l1u-20260828` (Linux build host) | `claude/l1-unsafe-20260828` |
| **L6 concurrency & threads** | **DONE 2026-08-29** — 109 native-won triples, 546 probe rows, 33 defects fixed, 0 residuals of its own. Lane doc retired to `internal/jdk-only/`; record is `L6-concurrency-lane-complete-20260828.md` | `/data/cvm-l6cc-20260828` (Linux build host) | `claude/l6-concurrency-20260828` |
| **L3 `java.util` collections** | **DONE 2026-08-29** — 609 owning rows across 56 classes, 1879 probe rows in twelve probes, 69 defects fixed, 8 recorded residuals. Lane doc retired to `internal/jdk-only/`; records are `l3-java-util-collections-1879-rows-and-69-defects-20260828.md` and `a-bound-method-reference-is-a-different-dispatch-door-20260828.md` | `/data/cvm-l3u-20260828` (Linux build host) | `claude/l3-util-collections-20260828` |
| **L8 the long tail** | **DONE 2026-08-29** — all 7 batches closed: 56 defects fixed, 4 recorded, 20 141 probe rows 0-diff in both modes (§2.1) | `/data/cvm-l2s-20260828` (Linux build host) | `claude/l8-tail-20260829` |

**ALL EIGHT LANES ARE DONE** — L1 through L8, the last of them (the long tail)
on 2026-08-29. Seven of the eight lane handoffs are retired to
`internal/jdk-only/`; L5's lives with its records in this directory, and L8
never had a brief of its own — it was §2.1 of this page, and its four records
are in the internal tree.

**This page is STILL NOT retired, and still should not be.** Every lane it
scoped has landed, and that is not the same thing as the page having no
question left. Three things on it are live, and the first two would each be a
lane:

* **Phase 2 is not adjudicated.** The lanes measured the surface; the worklist
  is **1065 distinct `native-won` triples** (P4-A, corpus-wide — not the 334 a
  five-probe screen saw), and `[has_code≠retire]` applies to every one of them:
  a 0-diff argues KEEP as often as it argues retire. Nothing in the eight lanes
  touched this — they fixed BEHAVIOUR, which is the prerequisite for
  adjudication, not the adjudication.
* **§4 still carries OPEN, owned items.** The FFM interface-classed identity
  family is measured twice and blocked on a CONTRACT decision rather than a
  patch — is the VM's carrier a compatibility stand-in or its own allocation
  shape? — and `KeyStore.getInstance("JCEKS")` is a JCA format missing in BOTH
  modes, i.e. a feature gap rather than a shadow defect. Neither is closable by
  the lane that happens to be reading this page.
* **§5 is the operational surface every lane runs from** — the landing
  protocol, the known-red vectors and gates on `dev`, and the instrument traps.
  **This one is structural: retiring the page would move the landing protocol
  out of the directory people read**, which is the opposite of what retirement
  is for.

A page that still poses a question belongs in `known-issues/`, even when all
the work that prompted it has landed. What would unblock retirement is the two
lanes above finishing and §5 being rehomed somewhere it stays visible — in that
order, and none of it is a residual of the eight.

**RE-RUN YOUR FAMILY'S EXISTING PROBES ON THE FINAL BINARY, not only the ones
you wrote.** L4's five new probes were all 0-diff and the lane looked finished;
running the four `java.io` probes that were already in the tree found
`probes/FilePathSweep.java` at **94 differing lines** and the largest single
cause in that lane — a path predicate whose own comment claimed it was
platform-independent and was not. A new probe asks the questions its author
thought of, and L4's author was on a Linux host and did not think of
backslashes. Cheap to do, and it is the only step that can catch what your
fixes broke as well as what they missed.

**Two things L3 found that the next lane should read before starting.**
`x::m` and `() -> x.m()` are DIFFERENT DISPATCH DOORS on this VM — a bound
method reference is a MethodHandle that bypasses the force-native gate, so
`t(tag, x::m)` in a probe measures the door and not the family. It cost L3 a
build cycle; write the lambda. And `owns_slot: true` is not enough to know a
registration can fire: if the class INHERITS the method as an interface default,
dispatch resolves to the interface and the class-name row is dead. The dump says
so in the same row — `real_declaring_method.has_code: false` next to
`invocations: 0`.

**And re-run the ARMS after you merge, not only before.** L3's merge of L4 and
L5 turned `RExceptions` and `RJdkFailure` red — a `ClassNotFoundException`
message, nothing either lane's own gates could see. Two green lanes combine into
a red tree; the arms on the MERGED tree are the only thing that says so.

**Every lane that has finished has found defects OUTSIDE its `native-won`
triples, and L5 found five.** The triples are a worklist, not a boundary: they
name where a native beat bytecode, which is a dispatch fact, not a correctness
one. L5's residual round added `Module.getPackages()` (a hand-written table
shadowing the VM's own registry), seven MUTABLE module collections that no
record mentioned, and three `MethodHandle` defects including a well-formed call
returning a wrong VALUE. None was a `native-won` triple. Budget a pass beyond
the list.

Two items L5 first recorded as OPEN were later FIXED, and both had been deferred
for reasons that one lookup would have refuted — `Module.canUse` (the VM's own
`ctx.module_uses` already exposes the registry; no Java callback needed) and the
duplicate-`defineClass` error type (the right `LinkageError` variant already
existed). **Before recording something as too expensive to fix, check the API
you are assuming you lack.**

Add your row to the table above in your first commit so the next worker can see
it.

---

## 1. Where the goal actually stands

The bar is `docs/feature-designs/jdk-only-completion-roadmap.md` §6:

> A Spring Boot application, a servlet container serving HTTPS, and a JDBC
> workload each run to completion under `--jdk-only` with **no fabricated class
> instantiated, whatever its package** — screened against the refused-class set
> the VM reports, not against a prefix.

**That page says FINAL and is stale.** It carries a re-adjudication banner as of
2026-08-27; read the banner, not the body, for status.

| phase | status |
| --- | --- |
| **Phase 1** — fabricated receiver kills its caller | **CLOSED 2026-08-29 — all nine lanes.** A/B/D/G/I by `Phase1Sweep` (80 rows); C/E/F/H by `probes/P1RemainingSweep.java` (29 rows, 0 differing both modes, **0 PHASE1-KILL**), each exercised through the payload the roadmap names and with all four mint sites still LIVE. Record: `phase-1-is-closed-the-last-four-lanes-measured-20260829.md`. It means the stated MECHANISM no longer fires on the nine — not that no fabricated class can kill a caller: `RJdkEnumerations` was exactly that and the CORPUS found it, not a Phase 1 probe. |
| **Phase 2** — retire the shadows | **ADJUDICATED 2026-08-30 (L2). 34 of 270 classes are demonstrably load-bearing; of the other 236, ONE TRIPLE of 1477 survived the evidence and is retired.** The 14-vector corpus passes `ConcurrentHashMap` 14/14; retiring it empties `Properties.keySet()` and kills the probe. §2 below and [`phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`](phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md). |
| **Phase 3** — correctness gaps no census sees | **CLOSED.** 35 rows, 0 differences, both modes, including the `aastore` covariance check the page still calls its one live red. |
| **Phase 4** — the evidence base | **CLOSED 2026-08-29 (L7).** All three workloads ARE checked out on `azure-host-2` — the blocker was a host, not an absence. Five arms, `compatibility_classes: 0` and `synthetic_stub_invocations: 0` on every one, every fabrication request named with its requester `file:line`. **P4-A and P4-B are now done too:** a 218-class corpus under `--jdk-only` against HotSpot — fully adjudicated at 185/19/14, 0 strict-only failures, worklist 1065 not 334 — and `--features synthetic-jdk` compiled and run for the first time (49 vectors: 1 pass, 48 fail, 53 distinct missing natives with callers). **Two methodology results worth more than the counts:** buy the ORACLE before raising your own cap (4 of the slowest 21 do not finish on HotSpot at 1800 s either, so no CratonVM verdict on them can mean anything), and confirm any mode-specific failure ALONE — sharding manufactured one here, as a working directory on the wrong filesystem manufactured another. |

### The finding that reframes the work

`--jdk-only` is now **more correct than the default**. Measured, repeatedly:

```text
Phase1Sweep         --jdk-only 12 differing lines   compatible 18
AtomicUpdaterSweep  --jdk-only 87/87 CLEAN          compatible DIED
Collections.sort(List.of(..))  --jdk-only refuses   compatible SORTS IT IN PLACE
```

Five separate places now. Every fix in this campaign has brought the DEFAULT
into line with strict, not the reverse. If your lane finds a difference that
appears in compatible mode only, that is the expected shape — not a surprise.

---

## 2. Phase 2, sized two ways

### Statically — the adjudication surface

From `--dump-native-registry` on a live run:

```text
12665 registrations      bridge 10350   synthetic-stub 1654   intrinsic 661
 3077 rows whose REAL method has Code
 2244 of those are BRIDGE  <- the adjudication surface, across 183 classes
```

`synthetic-stub` rows are NOT the target: strict already drops all of them and
nothing breaks (`synthetic_stub_invocations: 0` on every probe measured).
`intrinsic` rows are exempt by construction.

### Dynamically — what a program actually meets

The report's `native-shadows-bytecode` rows carry an **`outcome`** field, and
until 2026-08-28 nothing read it:

```text
native-won    579 rows   334 DISTINCT triples   <- the real worklist
bytecode-won  226 rows   104 DISTINCT triples   <- already lost; nothing to retire
```

**A `bytecode-won` row is a native that was registered and lost the dispatch
anyway.** Counting those inflates the worklist ~40% and aims work where there is
nothing to remove. Filter on `outcome == "native-won"`.

### The lane split

Divided by FAMILY, sized on the static surface. Percentages are of 2244.

| lane | families | rows | ~% |
| --- | --- | ---: | ---: |
| **L1** | `jdk/internal/misc/Unsafe` 120, `sun/misc/Unsafe` 102 | 222 | 10% |
| **L2** | `AbstractStringBuilder` 61, `StringBuffer` 58, `StringBuilder` 57 | 176 | 8% |
| **L3** | `Properties` 92, `TreeMap` 41, `ArrayDeque` 33, `LinkedList` 31, `TreeSet` 31, `HashMap` 30, `Collections` 26, `Hashtable` 24, + tail | ~380 | 17% |
| **L4** | `java/io/File` 74, `java/nio/file/Files` 59, `ByteBuffer` 33, `PrintStream` 29, + `java/io` and `java/nio` tail | ~335 | 15% |
| **L5** | `Class` 67, `reflect/Field` 32, `ClassLoader` 29, `System$1` 29, `reflect/Method` 26, `Module` 24 | 207 | 9% | **← taken** |
| **L6** | `ConcurrentHashMap` 48, `Thread` 37, `ForkJoinTask` 36, `ForkJoinPool` 26 | 147 | 7% |
| **L7** | Phase 4 — the three definition-of-done workloads | n/a | — |

The remaining rows are a long tail, and the sizing above is the one taken
BEFORE the seven lanes ran. Re-derived 2026-08-29 on the tree every lane landed
into, from a live `--dump-native-registry`:

```text
bridge rows whose real method has Code and owns its slot   2063  across 195 classes
  claimed by a finished lane                               1753  across 133
  TAIL, unowned                                             310  across  62
    already reached by the tail's existing probes            93
    UNPROBED                                                217  across  56
```

**So the tail is 217 rows, not 780** — six lanes closed 1753 between them. The
unprobed remainder groups into seven batches, and they are what L8 is working:

**ALL SEVEN BATCHES ARE CLOSED (2026-08-29). 56 defects fixed, 4 recorded,
20 141 probe rows at 0-diff in both modes.**

| batch | rows | probe | result |
| --- | ---: | --- | --- |
| `java/net/URI` + `URL` | 14 | `UriRecompositionSweep` 1256 | **7 defects**; the row §4 deferred was 26 |
| Throwable and the exception hierarchy | 117 | `ThrowableFamilySweep` 3973 | **9 defects**; `owns_slot` named the registrar that mattered |
| `java/math/BigInteger` | 24 | `BigIntegerSweep` 13 253 | **3 defects**, all on error paths |
| `java/lang/System` + `Runtime` + `Object` + `System$Logger` | 43 | `SystemRuntimeObjectSweep` 125 | **18 fixed, 4 recorded** |
| `java/lang/ref` | 19 | `RefFamilySweep` 60 | **2 defects**, one of them a hang |
| `java/security` | 27 | `SecuritySurfaceSweep` 1333 | **11 defects**, all on refusal paths |
| `jdk/internal` | 29 | `JdkInternalSweep` 120 | **6 defects**; one line of them was 12 rows |
| *(written along the way)* | — | `HelpfulNpeProbe` 21 | refuted the hypothesis that this VM has no helpful NPEs |

**Row counts here are the probes' own `rows N` lines, not `wc -l`.** Every probe
in this family prints two trailer lines (`rows N` and `DONE <name>`), and
`tail-run.sh` reports `hs=<LINES>`. Taking the runner's number as a row count
overstates every probe by two — small, systematic, and it scales with the number
of probes. Corrected here and in the four records on 2026-08-29.

Records, under the internal tree at `jdk-only/`:
`l8-tail-uri-seven-defects-and-a-deferral-that-was-26-rows-20260829.md`,
`l8-tail-throwable-nine-defects-and-the-one-the-registry-had-to-name-20260829.md`,
`l8-tail-biginteger-system-and-ref-23-defects-and-a-hang-20260829.md`,
`l8-tail-security-and-jdk-internal-seventeen-defects-and-the-tail-is-closed-20260829.md`.

The four recorded residuals are all in the `System` batch and all named with
their reasons in its record: `System.setOut(null)`, `setSecurityManager` (a
documented security-model decision coupled to the exec/Panama gating), and the
two rows of the largest open finding — **`--jdk-only`'s `System.getLogger`
resolves to `SimpleConsoleLogger` rather than the JUL provider**, so every
`System.Logger` in that mode ignores logging configuration. That is a
`ServiceLoader`/module-graph defect, several sizes larger than the rows it
shows up on.

### What the seven batches had in common

The estimate for these seven was ~217 rows and "the tail". What they actually
were, over and over:

* **A comment that recorded a deviation and never priced it.** `BigInteger`'s
  `modPow` said a negative modulus is "outside the BigInteger spec" and computed
  anyway; `Throwable`'s suppressed-exception list said it held an array where
  the JDK holds a `List`, which cost every suppression-bearing throwable its
  serialization; `Signal`'s registrar stated a field order the class does not
  have.
* **One decision with more than one registrar.** `owns_slot` is the column that
  settles which one runs, and twice a fix landed for one descriptor and not its
  neighbour because of it.
* **A defect that announced itself by ARITHMETIC rather than by content.** 280
  identical rows meant a probe bug; nine of ten getters meant a fix at the wrong
  level of the call chain; twelve rows across three signals meant one line.
* **Deferrals that were requests for a measurement.** Two of them, both closed:
  the `URI` recomposition row and the `System.Logger` `OFF` arm.

### Adjudicated, 2026-08-30 — and the instrument matters more than the answer

Full record: [`phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`](phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md).

`CRATONVM_ENFORCE_NATIVE_SHADOW=<class>`, one class at a time, 270 classes,
14-vector smoke set, one pinned binary: **34 LOAD-BEARING, 236 RETIRE-SAFE.**

**Do not use that RETIRE-SAFE column as a retirement list.** Arming all 236 at
once, on the same binary whose unarmed baseline is 118/118:

```text
armed --jdk-only    64 passed, 54 failed     (reproduced in two independent runs)
armed SUITE=all    118 passed,  0 failed     <- the dial is inert outside --jdk-only
armed SUITE=core    78 passed,  0 failed
enforcement_dial  reached 14 123 530  yielded 14 055 769  LEAK 0
```

Fourteen million dispatches yielded to real bytecode, zero leaks — the
retirement was COMPLETE — and the corpus still failed 54 vectors. It also broke
**35 of 78 probe families and killed 20**, several byte-identical to HotSpot
before the retirement.

Each of those 236 classes passes 14/14 alone. **A per-class sweep cannot
predict a set**, and four more full-corpus runs say why it is both reach and
combination: `ConcurrentHashMap` passes the 14-vector smoke set 14/14 and fails
**seven** full-corpus vectors, while `Locale` alone fails none — so no single
class accounts for the 54.

Three further checks, each cheaper than the sweep that produced it:

* **146 of the 236 greens were never asked anything.** The smoke set dispatches
  a shadowed native on only 120 of the 270 classes; for the rest, arming
  changed nothing because nothing was called. Check
  `enforcement_dial.reached > 0`, not a passing vector.
* **A row does not measure the class it names.** `EnforceShadowScope::covers`
  is `starts_with`, so `java/io/File` also armed `FileInputStream`, and
  `java/util/HashMap` also armed `$KeyIterator`. A retirement is per-TRIPLE;
  the dial is per-PREFIX — a fourth difference on top of the three the flag
  documents.
* **It would have re-retired six triples `retired_shadow.rs` deliberately holds
  back**, reason "needs-VM-support: state is not real".

Re-asking that hold list with the families' CONTENT probes — armed against
unarmed on one binary, `yielded/reached` printed so an unasked dial could not
pass as an answer — found the hold list correct and the corpus wrong:

```text
                         corpus (14 vectors)   own content probe        another family's probe
ConcurrentHashMap        14/14 PASS            0 changed / 39 357 y     died 261/302, 53 changed
```

The rows it breaks are `java.util.Properties`', not ConcurrentHashMap's: JDK
25's `Properties` delegates its `Hashtable` methods to an internal
`ConcurrentHashMap`, so `keySet()` comes back `[]` on a three-entry table and
the run dies in `ConcurrentHashMap$KeyIterator.next`. **A retirement's blast
radius is its class's USERS**, and the family's own probe being clean is the
trap rather than the reassurance.

**One triple retired**, and the ratio is the finding:
`sun/nio/ch/FileChannelImpl.truncate(J)`, in a new
`RETIRED_SHADOW_PHASE2_TRIPLES`. Two binaries from one tree differing only by
that entry — `L4Diag` 4 diffs from HotSpot to 0, every other probe delta 0,
118/118 on both. `FileChannel.truncate(-1)` now answers `Negative size` as
HotSpot does instead of `Negative size: -1`.

**Two false starts are recorded with it, and they cost more than the fix.**
`jdk/internal/foreign/ArenaImpl` makes `Arena.allocate()` return the real
`NativeMemorySegmentImpl` — and takes `FfmSegmentSweep` from 40 diffs to 181,
dead at row 18. And the first triple retired was `open`, chosen because it was
the only one the CORPUS had dispatched; it moved nothing, because the probe
that produced the evidence exercises `truncate` (`invocations: 2`) and never
touches `open` (`invocations: 0`). **A dial result is evidence for trying the
table, not the table's result** — the two mechanisms differ, and a build is
what finding that out costs.

Guarded in code, not only here: `the_held_collection_families_are_not_retired`
now carries the measurement and holds the CHM view/iterator classes too.

**If you take a family, it needs all four:** a dispatch proven by
`reached > 0`; content probes of the family AND of every family that embeds it;
image bytecode to yield to (`image_declaring_method` `has_code`, declared or
inherited, not abstract — 262 candidate triples fail this and would trade a
shadow for an `UnsatisfiedLinkError`); and a dispatch actually observed in the
unarmed corpus (1357 fail this).

---

### What is NOT closed

The unowned `--jdk-only` surface is not the same thing as the corpus. These
seven batches close the 217 rows this page scoped; `--jdk-only-report` still
counts native-won triples elsewhere, and the four residuals above are real.

The Throwable row was estimated at ~70 and measured at **117** once the family
was counted from the registry rather than from the class list — every
`Exception`/`Error` subclass in `THROWABLE_FAMILY_CLASSES`, not just the ones
whose names looked central.

**A red on `dev` that was not any lane's merge — FIXED by `c5f66112d` on
2026-08-29, an hour after this note was written.** Kept because the shape
recurs and because the guard did its job. `cargo test -p
cratonvm-native-builtins --lib` failed on `properties_sidetable`'s own
source-witness guard, from `5a6348d28 fix(util): Properties.clone() and
replaceAll() NPE on a Properties this VM built`:

```text
properties_sidetable::tests::only_order_insensitive_functions_read_the_unordered_snapshot
  these functions read the UNORDERED side-table snapshot:
  ["native_properties_clone", "native_properties_replace_all"]
```

That test reads `include_str!("properties_sidetable.rs")` and nothing else, and
the file is byte-identical to `origin/dev`'s — so it reproduces on pristine
`dev` and no merge can be blamed for it. The guard's message offers two ways
out; **the escape hatch (`ALLOWED`) looked like the wrong one**, since a cloned
`Properties` and an in-place `replaceAll` both hand an iteration order back to
Java, which is what `ordered_snapshot_kv` exists for. That was left to the lane
that wrote the fix rather than guessed at from outside it — and that lane
reached the same answer: `c5f66112d fix(properties): clone and replaceAll hand
Java an iteration order, so they read the ordered snapshot`.

**The transferable part is the attribution, not the fix.** The test's input is
`include_str!("properties_sidetable.rs")` and nothing else, so byte-identity
with `origin/dev`'s copy of that one file is a complete proof that a merge did
not cause it. A red you can attribute in one `git diff` is a red you do not have
to bisect. (For the OTHER red of the week, `RSslEndpointIdentification`, see the
Vectors section below — it is fixed, and it was the vector's own bug rather than
the flake it looked like.)

**Re-run the tail's existing probes before writing a new one.** Restored to
`apps/probes/` and taken on the current binary, they are: `LangMiscSweep`,
`TailFamilySweep`, `CharacterSweep`, `IoSystemSweep`, `InetFamilySweep`,
`Phase3Sweep` all **0-diff in both modes**; `UriLocaleSweep` and
`MathSurfaceSweep` red, and both reds are already-recorded known issues (the
`Locale` display-name data gap, the `URI` empty-authority recomposition, and
1-ULP `Math.pow`/`sin`/`log10`). Confirming coverage is the point — that is
seven probes' worth of tail surface nobody has to re-derive.

**First tail slice taken 2026-08-29: `java.lang.invoke`'s LOOKUP and TYPE
surface** — `MethodHandles$Lookup` 6 triples, `MethodType` 5, `MethodHandle` 3,
`MethodHandles` 1. 83 probe rows, **27 differing -> 1**, ten defects. Record:
`the-invoke-lookup-surface-ten-defects-and-one-that-corrupted-an-interned-type-20260829.md`.
Distinct from the DISPATCH surface (`invoke`/`invokeExact`) that L5's residual
round closed — worth knowing if you take another `java.lang.invoke` slice.

---

## 3. The method

**Moved, in full, to [`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md) §1.** The six steps, the aim-at-edges finding and the probe-hygiene list are process rather than campaign material: they were true before this page and are true after it.

The finding worth keeping in front of anyone sizing a lane: **of the first 48 defects, not one was a wrong answer to an ordinary call**, across five independent families. The seven later batches came out the same way — `java.security` had 1313 of 1333 rows already correct and every one of its twenty differences on a refusal path. Aim at the edges.

---

## 4. What is already done — do not redo it

Landed on `dev` today, all verified in both modes:

| probe | rows | result |
| --- | ---: | --- |
| `Phase3Sweep` | 35 | 0 diffs — Phase 3 closed |
| `Phase1Sweep` | 80 | all nine Phase 1 lanes |
| `AtomicUpdaterSweep` | 87 | 0 diffs under `--jdk-only` |
| `InetFamilySweep` | 468 | 0 diffs |
| `KeyStoreFamilySweep` | 116/160 | residual is the JCEKS gap |
| `ArraysHashSetShadowSweep` | 105 | 0 diffs |
| `HashMapShadowSweep` | 82 | 0 diffs |
| `ClassShadowSweep` | 261 | 1 documented row (`canUse`) |
| `BaosCollectionsShadowSweep` | 62 | 0 diffs |

Records live in `docs/known-issues/jdk-only/`. The two that matter most for
planning:

* `the-definition-of-done-screen-run-for-the-first-time-20260828.md`
* `phase-2-worklist-mined-28-defects-in-four-families-20260828.md`

### What L1 found that changes another lane's reasoning

* **A shadow can be unretirable by construction.** The JDK implements the whole
  byte/short/char/boolean atomic family in bytecode, by masking the 32-bit word
  at `offset & ~3`. That is meaningless when `objectFieldOffset` returns a SLOT
  INDEX, so on CratonVM `compareAndSetByte` answered `false` with the right
  witness and `getAndSetByte` / `getAndBitwiseOrByte` **never returned**. Any
  retirement pass reasoning from "the real method has Code" will nominate the
  natives that stand in front of that bytecode; the answer is no. If your family
  has a JDK bytecode body that does OFFSET ARITHMETIC, check it before you
  retire its native.
* **The retirement surface IS the JDK's argument-validation layer.** Nineteen of
  L1's 24 defects are a null check, a bounds check, a size rule or a refusal
  type that lives in a bytecode wrapper and nowhere else, and the `0`-suffixed
  twin the wrapper calls is already registered here — pointing at the SAME Rust
  function. That is why the check has one home.
* **Two spellings of one class need not share one contract.**
  `sun.misc.Unsafe.objectFieldOffset` refuses a record component;
  `jdk.internal.misc.Unsafe.objectFieldOffset` answers an offset. One native
  serves both, so the receiver is the discriminator. A first pass that applied
  the refusal to both replaced one wrong answer with another.
* **The oracle can be the thing that crashes.**
  `sun.misc.Unsafe.allocateInstance(null)` SIGSEGVs HotSpot 25.0.4+7, and the
  crash summary goes to STDOUT, so it both truncates the sweep and pollutes the
  transcript. Null and out-of-bounds arguments belong in a probe that runs one
  call per process behind a `timeout`, where "the VM died", "the VM never
  returned" and "the VM threw" stay three different transcripts.

### OPEN, owned, do not duplicate

| item | owner |
| --- | --- |
| ~~`ConcurrentHashMap.elements()` never terminates~~ | **FIXED by L6, 2026-08-29.** The mechanism was two producers of one carrier class, and the fix keeps `a0168ed03`'s parity win rather than reverting it. `RJdkEnumerations` now PASSES in compatible mode where pristine `dev` fails it. See `L6-concurrency-lane-complete-20260828.md` §2.2. |
| `Arena`/`MemorySegment` report an INTERFACE as an instance's class | **MEASURED TWICE, INDEPENDENTLY, AND THE TWO AGREE.** Behaviour is FIXED (9 defects, 199 rows) -- see `ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md`, which is the record of this defect and sizes the identity residual across FIVE class families. The MECHANISM and the contract question are in `arena-and-memorysegment-hand-out-an-interface-and-jdk-only-is-the-worse-mode-20260829.md`: the carrier is minted through `try_ensure_synthetic_class`, the door `--jdk-only` refuses by design, so strict falls back to the interface. **Still OPEN, and the blocker is a CONTRACT decision, not a patch** -- is the VM's carrier a compatibility stand-in or its own allocation shape? |
| **The BEHAVIOURAL half of the same surface: CLOSED 2026-08-29.** 199 differential rows over the segment/arena/layout API (`apps/probes/FfmSegmentSweep.java`) found **nine defects that are not identity** and every one is now 0-diff in both modes: a native `asReadOnly()` segment ACCEPTED WRITES; `ByteOrder` was minted per call so `ValueLayout.JAVA_INT.order() == ByteOrder.nativeOrder()` was false; `Arena.global().close()` succeeded; `allocate(-1)`, two bad alignments and `ofArray(null)` did not refuse; and `s.asSlice(0, s.byteSize()).equals(s)` was false. The identity rows to the left are what REMAINS after those. | **DONE** — `ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md`. It also measures what the identity defect does NOT break: `isInstance`, `instanceof`, `isAssignableFrom` and a class-keyed `HashMap` round-trip all answer correctly on an interface-classed segment, in both modes — so the contract decision to the left is a decision about identity alone. || ~~`AsynchronousFileChannel.write` returns `CompletableFuture` not `PendingFuture`~~ | **FIXED by L6, 2026-08-29**, along with three behavioural gaps beside it that 38 differential rows found. §6 of the same record. |
| `Module.canUse` over-approximates | **L5 (mine)**, documented in the registrar |
| `KeyStore.getInstance("JCEKS")` unsupported | unclaimed; NOT a `--jdk-only` item, missing in both modes |
| `java/lang/StringBuilder` cluster | `WORKER-3-NOTE-3` has it open — **L2 must check that note first** |
| ~~`Properties.values().iterator()` mints the fabricated `cratonvm/internal/ArrayListViewItr` through a `try_alloc_synthetic(..)?` with no refusal arm~~ | **FIXED by L6, 2026-08-29** — after first recording it as L3's. It was not one vector: it also killed `MapViewBehaviourProbe` at row 0 of 194 and `ItrClassProbe` at row 31 of 66 under `--jdk-only`. The route it was said to need is one call to a function that already existed. `L6-concurrency-lane-complete-20260828.md` §9. |
| ~~`Class.forName("[L<absent>;")`'s `ClassNotFoundException` names the DESCRIPTOR, not the element~~ | **FIXED by L1's `39e2ded07`, 2026-08-28**, between L6's arms run and its push. It is what made `RExceptions` and `RJdkFailure` red for every lane; `L6-concurrency-lane-complete-20260828.md` §8 records the measurement that attributed it to pristine `dev`. |

---

## 5. Rules that keep lanes from colliding

**Moved, in full, to [`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).** Worktrees and branches, the shared registrar files, `owns_slot` before editing, the identity and field-slot traps, the instrument traps, the landing protocol and what "done" means for a lane are all there, with the cost each was learned for.

**Read §5 of that page before your first landing.** The gate set changed on 2026-08-29 — it names no `--test` targets any more, because `native-builtins/tests/` holds ten and the hand-written list named seven. A gate script copied from an earlier lane is a script that runs 7 of 10.

What remains below is DATED: which vectors were red on which day, and which lane fixed what. It decays, and it is kept here rather than moved because a permanent page should not carry a list that is wrong in a week.

### `--features synthetic-jdk --tests` — was red for everyone, FIXED 2026-08-30

If you have been treating that arm's `synthetic_stub_count_does_not_regress`
failure (1591 against baseline 1582) as somebody else's drift, it was not
drift and it is now green. **That arm had no baseline of its own.**
`BASELINE_SYNTHETIC_STUBS` branched on `feature = "management"` and nothing
else, so a third configuration — which compiles registrars the other two do
not — was being scored against the DEFAULT resolve's number.

Worse, its failure line printed `no-management` and named
`BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` as the constant to paste into, which
is the exact re-freeze-from-the-wrong-run hazard that label exists to prevent.
Pasting 1592 there would have admitted nine stubs to the default arm silently.

`BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK` closes both halves. The nine rows by
which that resolve exceeds the default are frozen, **explicitly not blessed**,
and flagged for classification — a first freeze is not an adjudication.

---

### Known-red vectors, so you can tell yours from theirs

* ~~`RJdkEnumerations`~~ — **GREEN as of 2026-08-29, in all three arms**
  (112/112 strict, 112/112 all, 72/72 core, measured twice on two different
  merges). Two lanes, two halves. L6 fixed the compatible-mode half (the CHM
  values cursor, dev's `a0168ed03`) and then the strict half too
  (`844c581fa`, `SnapshotItrRoute::ViewCollection`): `Properties.values()
  .iterator()` minted `cratonvm/internal/ArrayListViewItr`, strict mode
  correctly refused it, and the mint site's bare `?` handed that refusal to the
  caller as a `NoClassDefFoundError`. It is recorded in
  `L6-concurrency-lane-complete-20260828.md`.

  **L2's note below was right when it was written** — it measured the strict-arm
  failure directly and correctly said the cause was no longer `a0168ed03` and
  that the surviving request came from another site. That site was
  `alloc_arraylist_iterator`, and L6 landed its refusal arm the same day. Left
  here because the sequence is the point: three lanes measured the same vector
  and each was right about a different half of it.

  **No vector is known-RED any more, and the one that read as intermittent was
  the vector's own bug.** See `RSslEndpointIdentification` below: it is fixed,
  and the note is kept because of how it looked on the way there.
* `RExceptions` and `RJdkFailure` — **red on `dev` from `c6ccccbc8` (the L5
  lane) until `39e2ded07` fixed it. If you ran the arms in that window you saw
  two reds that were not yours and are not yours to chase.** Both assert the
  same thing: an array `ClassNotFoundException` must name the ELEMENT, not the
  descriptor. Fixed; verify against a binary newer than that fix before
  spending anything on them.
* `RBlockingQueue` — a documented flake (`HANDOFF-20260812.md`, "do not chase
  it"). One failure under suite load, passes standalone and on repeat.
* ~~`RSslEndpointIdentification`~~ — **FIXED 2026-08-29. It was never
  intermittent and it was never CratonVM's**: the vector's own client loop threw
  away the reply it asserts on, and it failed on the HOTSPOT side while CratonVM
  passed all four checks. Kept here as a worked example of a failure mode this
  campaign keeps meeting, not as an open row.
  What it looked like first: green in the `--jdk-only` and `SUITE=all` arms and
  red in the `core` arm of the same cycle, then 2-of-3 standalone passes, then —
  an hour later on a host at load average 14 — red in three consecutive `core`
  runs. That reads exactly like a flake becoming a regression.
  What it was: `unwrap()` consumes ONE TLS record per call, and the loop called
  it once per `read()`. Under load the server's NewSessionTicket, its reply and
  its close_notify arrive in a single 350-byte read; the one `unwrap` consumed
  the 222-byte ticket and produced no application data, the next `read` returned
  EOF, and `break` discarded the 128 buffered bytes that were the reply. Whether
  the records coalesce is a scheduling question — the whole of the
  "intermittency". Proven by ABBA-interleaved A/B on one loaded host: **15
  failures in 40 runs on the committed loop, 0 in 40 on the drained one.**
  **Three things to take from it.** The harness had already said it: guard `G4`
  printed *"the HotSpot oracle run FAILED (rc=1), so the 'expected' side of the
  cross-VM diff is an artefact of the oracle's failure, not ground truth"* — read
  which SIDE failed before reading the diff. A vector that passes standalone and
  fails in the suite is not automatically leakage; this one passed standalone
  because the host was quiet at the time, and reproduced under `ONLY=` once it
  was not. And a failure rate that climbs with load is a race in someone's code,
  not noise to be re-run away — here, ours.

### A gate was red on `dev` for ~5 hours on 2026-08-29 — CLOSED, kept for the technique

`cargo test -p cratonvm-native-builtins --lib` was 4176 passed, **1 failed**
between `5a6348d28` and `b5a784fee`:
`properties_sidetable::tests::only_order_insensitive_functions_read_the_unordered_snapshot`,
naming `native_properties_clone` and `native_properties_replace_all`. Both the
guard and the two functions it names landed in the SAME commit. Fixed by the
owning lane, which took the exit this row argued for — reading the ORDERED
snapshot — rather than adding the pair to `ALLOWED`.

**The reusable part is how ownership was settled: without a build.** The test is
a source witness over ONE file (`include_str!("properties_sidetable.rs")`), so
its verdict is a pure function of that file's bytes, and
`git diff origin/dev -- <that file>` came back empty. That is a proof, not an
inference, and it costs a second. Reach for it before rebuilding a pristine
`dev` — and note it only works because the witness reads a fixed path; a witness
that scans a directory has to be re-run.

**Both halves of the list matter.** This section lists known-red VECTORS, and a
lane that runs the gates first had nothing to check a gate red against.

**A THIRD, open as of `57107dbd5` (2026-08-30). The stub ratchet, both
configurations:**

```
cargo test -p cratonvm-native-builtins --test stub_ratchet
  synthetic_stub_count_does_not_regress
  1582 SyntheticStub natives now registered, exceeding the frozen baseline of 1576
  (management: 1593 against 1587)
```

**Attributed by MEASUREMENT, because this one cannot be settled by a diff:** the
count comes from the whole registry, so no single file's bytes decide it. A
detached worktree at pristine `origin/dev` plus
`cargo test -p cratonvm-native-builtins --test stub_ratchet` reports the
identical `1582` against `1576` — about five minutes, and the only honest way to
tell your rows from theirs. `git worktree add --detach <dir> origin/dev` with its
own `CARGO_TARGET_DIR` so it cannot disturb your build.

The failure text is itself the protocol: run `dump_synthetic_stubs` in both
trees, `comm -23` the sorted `@@STUB` lines, and classify with the SECOND column
before touching either baseline constant. Note the dump is a plain `#[test]`, so
`-- --ignored` runs zero tests and prints nothing.

**And a second one is OPEN as of `ff92ca9a4` (2026-08-29 evening).** Same test,
different row:

```
cargo test -p cratonvm-native-builtins --test registrar_drift   (also with --features management)
  the_drift_baseline_has_no_stale_rows
  STALE BASELINE — 1 recorded drift pair(s) no longer drift.
    register_phase54_atomics
      java/util/concurrent/atomic/AtomicReference.compareAndSet(Ljava/lang/Object;Ljava/lang/Object;)Z
```

It arrived with `7c90ec930` ("de-register the now-slower `AtomicReference
.compareAndSet` stub"), which collapsed the pair and did not regenerate the
baseline — the failure text says to do both in one commit. `registrar_drift.rs`,
`phases_early.rs` and `vm/src/jit/helpers.rs` are byte-identical to `origin/dev`
on any branch that has not touched them, which is how to tell it from yours.
Left for that lane: the fix is to move the triple to `FIXED_NOT_DRIFTING` with
`--dump-native-registry` evidence, which is a claim about their change, not
about the gate.

**Search the known-issues tree for a vector's name before bisecting it.** I ran a
repeat suite to re-derive what that page already said.

**And run the ARMS before you push, not only the gates.** The two vectors above
went red on a commit whose acceptance was 20 green gate binaries. The gates are
registration counts, conformance manifests, flag declarations and doc
citations; a message string inside an exception raised by a native is
behaviour, and only the corpus runs behaviour. The two instruments are
disjoint, and each has now landed a red on `dev` that the other would have
caught.

### Three lanes fixed the same three things on the same evening

While this lane verified, other lanes fixed every one of the reds it had just
fixed — and one of them fixed it BETTER:

* the ```rust doc fence in `a07dd621c` (rustdoc COMPILES those): fixed
  identically on `dev`;
* the array-CNFE regression from `c6ccccbc8`: fixed on `dev` by `39e2ded07`,
  with a control this lane had not run — a detached worktree at pristine
  `d17feaad2`, built from scratch, failing both vectors on its own;
* the three `runtime::resolve::guard` reds from `1dbbe2b36`'s un-rowed
  `find_method_recursive` site: **fixed by REMOVING the site.** This lane had
  raised the allowlist 3 -> 4 and the one-way budget 29 -> 30 to unblock
  everyone. That was the wrong repair, and by the time `dev` was re-read it
  would have re-broken both guards. All three were backed out in favour of
  `dev`.

**Re-read `dev` immediately before you merge, not only before you start.** This
lane read it at 39 commits behind. Every duplicate above was landed by someone
else inside the window this lane spent building and running arms — which on
this host is over an hour. A fix that was correct when you wrote it can be
wrong by the time you merge it.

### The vm gate is 14 targets red on `dev`, and one of them is a JVMS violation

Measured 2026-08-29 with a control worktree at pristine `origin/dev`: **17 vm
test targets fail there.** None of them is any lane's recent work. Two things
every lane needs to know before reading its own gate run:

* **`cargo test` STOPS at the first failing test BINARY.** Without
  `--no-fail-fast` a run showing one failure is not showing you the others — it
  is showing you where it stopped. And do not pipe the output through `head`:
  `cratonvm-vm` emits 40+ `test result:` lines and a cap hides the tail. Grep
  for FAILURE lines only, so an empty result is the green.
* **A test that needs the release binary SKIPS when there is none**, and reports
  `ok`. The control worktree has no binary, so three targets that fail on a
  built tree "pass" there. A control is only a control for what it can run.

`3b2901531` (release prep) untracked all 862 sources under `probes/`, eight of
which were TEST FIXTURES. Their tests return early and report `ok` in 0.00 s
while asserting nothing — 33 assertions went dark. Restored:
`probes/{FjpProbe,BdProbe}.java` and `apps/{executor,jmx,methodhandles,proxy,
selector}_probe/`, `apps/lm_subclass/`. `probe_fixture_census` is the instrument
that catches this and it was correctly red the whole time.

**~~OPEN, unowned, and worth someone's morning:~~ FIXED 2026-08-30.**
`warm_null_receiver_invokes_throw_npe_jit` — `invokespecial` on a NULL receiver
did not throw once warm, and the callee ran with `this == null`. JVMS §6.5, and
the mechanism behind the bogus `Cannot read field "interfaces" because "rd" is
null` at `Class.java:1217`.

**It was not the inline cache.** `invokevirtual`/`invokeinterface` were right
only incidentally — their cache tests the receiver's CLASS, so a null fails
every guard. `invokespecial` is statically bound, and both of the emitters that
bind it jump straight into the compiled callee. The only thing left to raise the
NPE was the callee body faulting on its own, which it does only if it
dereferences `this`:

```text
  private callee body     HotSpot   --nojit   jit (before)
  return 3;               NPE       NPE       NO-THROW(3)
  return this.x;          NPE       NPE       NPE
  return helper();        NPE       NPE       NO-THROW(5)
```

**Two emitters, and the first fix went to a third that was not involved.** The
single-pass direct call (`x64/bytecode_walk.rs`) and the optimizing tier's
`emit_direct_cross_call` (`ir_lower.rs`) both needed the check; which one runs
depends on the callee, so `return 3;` and `return helper();` were fixed by
different patches. `CRATONVM_DBG_IR_COMPILES` and `CRATONVM_DBG_JIT_GEN` name
the tier and the path — reach for them before patching an emitter, because the
disassembly of the *caller* is what proves the callee was not inlined.
`apps/probes/NrpVariants.java` is the three-body probe.

### dev's tip is frequently red

Seventeen undeclared-or-unfixtured flags across seven merges today, plus two
rounds of `docs/internal` citations. Expect to clear something that is not yours
on most merges; verify with `git diff origin/dev -- <file>` before editing, then
fix it, because a red gate blocks every lane.

---

## 6. What "done" means for a lane

**Moved to [`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md) §7**, including the rule that matters most for Phase 2: a 0-diff probe is a PRECONDITION for retiring a shadow and never a justification on its own.
