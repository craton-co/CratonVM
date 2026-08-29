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
| **L7 definition of done** | **DONE 2026-08-29** — all three workloads run to completion under `--jdk-only`; `compatibility_classes: 0` and `synthetic_stub_invocations: 0` on five arms and on **181 H2 test classes**. Four VM fixes, none of them a `--jdk-only` defect. All 4 residuals discharged: 2 fixed, 1 verified will-not-fix, 1 measured at 7 sites and handed on as a lane. Then the two Phase 4 items nobody had run: **P4-A** a corpus (218 classes, both arms) — **zero failures `--jdk-only` produces that compatible mode does not**, and Phase 2's worklist is **1065** native-won triples, not the 334 five probes saw; **P4-B** `--features synthetic-jdk` built and run for the first time. Instrument gap closed: 53 classes handed back that `new` could not produce, on runs reporting `compatibility_classes: 0`. Lane doc retired to `internal/jdk-only/`; records are `the-definition-of-done-run-on-the-three-real-workloads-20260828.md`, `the-four-residuals-two-closed-one-was-a-family-of-thirty-and-one-is-a-lane-20260829.md`, `P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md` and `P4B-synthetic-jdk-mode-run-for-the-first-time-20260829.md` | `/data/cvm-l7dod-20260828` (Linux build host) | `claude/l7-dod-20260828` |
| **L1 `Unsafe`** | **DONE 2026-08-28** — 516 probe rows, 24 defects fixed, 5 recorded residual categories. Lane doc retired to `internal/jdk-only/`; record is `l1-unsafe-516-rows-24-defects-and-the-sub-word-atomics-that-never-returned-20260828.md` | `/data/cvm-l1u-20260828` (Linux build host) | `claude/l1-unsafe-20260828` |
| **L6 concurrency & threads** | **DONE 2026-08-29** — 109 native-won triples, 546 probe rows, 33 defects fixed, 0 residuals of its own. Lane doc retired to `internal/jdk-only/`; record is `L6-concurrency-lane-complete-20260828.md` | `/data/cvm-l6cc-20260828` (Linux build host) | `claude/l6-concurrency-20260828` |
| **L3 `java.util` collections** | **DONE 2026-08-29** — 609 owning rows across 56 classes, 1879 probe rows in twelve probes, 69 defects fixed, 8 recorded residuals. Lane doc retired to `internal/jdk-only/`; records are `l3-java-util-collections-1879-rows-and-69-defects-20260828.md` and `a-bound-method-reference-is-a-different-dispatch-door-20260828.md` | `/data/cvm-l3u-20260828` (Linux build host) | `claude/l3-util-collections-20260828` |
| **L8 the long tail** | **IN PROGRESS 2026-08-29** — 217 unprobed rows across 56 classes (§2.1); 2 of 7 batches closed, 16 defects, 5233 probe rows 0-diff | `/data/cvm-l2s-20260828` (Linux build host) | `claude/l8-tail-20260829` |

**All seven lanes are DONE** — L1 through L7, the last of them on 2026-08-29.
Six of the seven lane handoffs are retired to `internal/jdk-only/`; L5's
lives with its records in this directory.

**This page is NOT retired with them, and should not be.** Three things on it
are still live:

* **Phase 2 is not adjudicated.** The lanes measured the surface; the worklist
  is **1065 distinct `native-won` triples** (P4-A, corpus-wide — not the 334 a
  five-probe screen saw), and `[has_code≠retire]` applies to every one of them:
  a 0-diff argues KEEP as often as it argues retire.
* **§4 still carries OPEN, owned items** — the FFM interface-classed identity
  family, sized but deliberately not fixed, and `KeyStore.getInstance("JCEKS")`,
  unclaimed and missing in both modes.
* **§5 is the operational surface every lane runs from** — the landing
  protocol, the known-red vectors and gates on `dev`, and the instrument traps.
  Retiring it would move that out of the directory people read.

A page that still poses a question belongs in `known-issues/`, even when the
work that prompted it has landed.

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
| **Phase 1** — fabricated receiver kills its caller | **5 of 9 lanes closed.** No `NoClassDefFoundError` in 80 probe rows. A/B/D/G/I clear. |
| **Phase 2** — retire the shadows | **the bulk of the remaining work.** §2 below. |
| **Phase 3** — correctness gaps no census sees | **CLOSED.** 35 rows, 0 differences, both modes, including the `aastore` covariance check the page still calls its one live red. |
| **Phase 4** — the evidence base | **CLOSED 2026-08-29 (L7).** All three workloads ARE checked out on `azure-host-2` — the blocker was a host, not an absence. Five arms, `compatibility_classes: 0` and `synthetic_stub_invocations: 0` on every one, every fabrication request named with its requester `file:line`. **P4-A and P4-B are now done too:** a 218-class corpus under `--jdk-only` against HotSpot (0 strict-only failures; the worklist is 1065, not 334) and `--features synthetic-jdk` compiled and run for the first time (49 vectors: 1 pass, 48 fail, 53 distinct missing natives with callers). |

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

| batch | rows | classes | state |
| --- | ---: | --- | --- |
| `java/net/URI` + `URL` | 14 | and `uri-resolve-folded-…-20260826.md` §4 deferred a fix pending exactly this probe | **DONE 2026-08-29** — `UriRecompositionSweep`, 1258 rows 0-diff both modes, **7 defects**; the deferred row was 26. `l8-tail-uri-seven-defects-and-a-deferral-that-was-26-rows-20260829.md` |
| Throwable and the exception hierarchy | 117 | `Throwable` 16 + 41 classes at 2-5 each, all sharing one registration set | **DONE 2026-08-29** — `ThrowableFamilySweep`, 3975 rows 0-diff both modes, **9 defects**. `l8-tail-throwable-nine-defects-and-the-one-the-registry-had-to-name-20260829.md` |
| `java/math/BigInteger` | 24 | the largest single class left | open |
| `java/lang/System` + `Runtime` + `Object` + `System$Logger` | 26 | | open |
| `java/security/MessageDigest` + `AccessController` | 20 | | open |
| `jdk/internal` — `VM`, `SharedSecrets`, `Signal`, `AbstractClassLoaderValue` | ~19 | | open |
| `java/lang/ref` | ~12 | GC-adjacent | open |

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

---

## 3. The method — four families in, it is mechanical

Five families are done this way: **993 probed rows, 48 defects, 48 fixed.**

1. **Take your families' `native-won` triples from the report.** Do not pick
   methods by hand and do not probe what the report says already loses.
2. **Write ONE differential probe per family group**, asking the CONTRACT EDGES.
3. **Run it in BOTH modes** against HotSpot 25.0.3+9 as oracle.
4. **Check `owns_slot` BEFORE editing** (§5).
5. **Fix, rebuild, RE-RUN THE PROBE.** Never conclude from reading.
6. Record what you found AND what passed.

### Aim at edges, not the happy path

**All 48 defects so far are on contract edges. Not one was a wrong answer to an
ordinary call, in any of five independent families.** Nulls, bounds, refusal
types, constructor validation, naming special cases, callback boundaries.

Two consequences:

* it predicts where your rows will yield;
* **a probe that exercises only the happy path will report your family clean when
  it is not.** That is how a shim's middle stays correct while its perimeter
  rots unnoticed.

Concretely, from the four done: every `ByteArrayOutputStream` bounds row already
passed *including* `off + len` overflowing to a negative int — the case a check
written `off + len > b.length` gets wrong while looking right. The bounds logic
was correct; it never ran, because a null buffer returned before it.

### Probe hygiene, each learned by getting it wrong

* **stdout only** (`2>/dev/null`). `2>&1` puts VM tracing in the diff; the
  asymmetric version of that mistake invented four phantom differences.
* **Print no value the two VMs may choose independently** — identity hashes,
  addresses, thread names, timings, iteration order of a hash container,
  a resolver's answer. Four harness artefacts came from exactly this.
* **Check the ROW COUNT before reading the diff.** A run that died partway
  produces a short file whose missing tail `diff` reports as ordinary `<` lines.
  That hid a whole probe section once and 128 of 160 rows in another the same
  day. `probes/…Sweep` runners print `rows N/M` for this reason.
* **A crash early in a probe masks every later defect.** Fixing it usually
  uncovers more work rather than finishing it.

Working runner: `probes/dodscreen.sh`, and the sweep runner pattern in any of
`ArraysHashSetShadowSweep` / `HashMapShadowSweep` / `ClassShadowSweep` /
`BaosCollectionsShadowSweep`.

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
| `Arena`/`MemorySegment` report an INTERFACE as an instance's class | **MEASURED 2026-08-29, still OPEN, blocker is a CONTRACT decision not a patch.** All four `Arena` factories in BOTH modes; every `MemorySegment` under `--jdk-only` only (its compatible-mode carrier landed 2026-08-22 and strict REFUSES it, falling back to the interface). One defect, one blocker: is `cratonvm/internal/foreign/MemorySegmentImpl` a compatibility stand-in that `--jdk-only` is right to refuse, or the VM's own allocation shape? See `arena-and-memorysegment-hand-out-an-interface-and-jdk-only-is-the-worse-mode-20260829.md` |
| **The BEHAVIOURAL half of the same surface: CLOSED 2026-08-29.** 199 differential rows over the segment/arena/layout API (`apps/probes/FfmSegmentSweep.java`) found **nine defects that are not identity** and every one is now 0-diff in both modes: a native `asReadOnly()` segment ACCEPTED WRITES; `ByteOrder` was minted per call so `ValueLayout.JAVA_INT.order() == ByteOrder.nativeOrder()` was false; `Arena.global().close()` succeeded; `allocate(-1)`, two bad alignments and `ofArray(null)` did not refuse; and `s.asSlice(0, s.byteSize()).equals(s)` was false. The identity rows to the left are what REMAINS after those. | **DONE** — `ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md`. It also measures what the identity defect does NOT break: `isInstance`, `instanceof`, `isAssignableFrom` and a class-keyed `HashMap` round-trip all answer correctly on an interface-classed segment, in both modes — so the contract decision to the left is a decision about identity alone. || ~~`AsynchronousFileChannel.write` returns `CompletableFuture` not `PendingFuture`~~ | **FIXED by L6, 2026-08-29**, along with three behavioural gaps beside it that 38 differential rows found. §6 of the same record. |
| `Module.canUse` over-approximates | **L5 (mine)**, documented in the registrar |
| `KeyStore.getInstance("JCEKS")` unsupported | unclaimed; NOT a `--jdk-only` item, missing in both modes |
| `java/lang/StringBuilder` cluster | `WORKER-3-NOTE-3` has it open — **L2 must check that note first** |
| ~~`Properties.values().iterator()` mints the fabricated `cratonvm/internal/ArrayListViewItr` through a `try_alloc_synthetic(..)?` with no refusal arm~~ | **FIXED by L6, 2026-08-29** — after first recording it as L3's. It was not one vector: it also killed `MapViewBehaviourProbe` at row 0 of 194 and `ItrClassProbe` at row 31 of 66 under `--jdk-only`. The route it was said to need is one call to a function that already existed. `L6-concurrency-lane-complete-20260828.md` §9. |
| ~~`Class.forName("[L<absent>;")`'s `ClassNotFoundException` names the DESCRIPTOR, not the element~~ | **FIXED by L1's `39e2ded07`, 2026-08-28**, between L6's arms run and its push. It is what made `RExceptions` and `RJdkFailure` red for every lane; `L6-concurrency-lane-complete-20260828.md` §8 records the measurement that attributed it to pristine `dev`. |

---

## 5. Rules that keep seven lanes from colliding

### Worktrees and branches

Work in **your own worktree on your own branch**. Land by pushing to `dev`.

### The big registrar files are shared

`native-collections/src/lib.rs` is **74 570 lines** and hosts many families;
`native-builtins/src/lib.rs` is 47 708. L3, L4 and L6 will all touch
`native-collections`. This is survivable — different functions are different
hunks and git merges them — but only if you **merge `origin/dev` often**, not
once at the end.

### `owns_slot` before editing — this costs a build if you skip it

A method can be registered by several files and only one wins.

```bash
cratonvm --java-home "$JDK" --dump-native-registry C:/windows/shaped/reg.json -cp probes/out YourProbe
# then read, for your (class, name, descriptor):  owns_slot, invocations
```

`owns_slot: false` means **your edit is inert**. `invocations: 0` on the winner
means your probe never reached it and the diff proves nothing either way.

I lost a 33-minute build to this today on `Arrays.fill` — fixed the
`phases_early.rs` registration, and `native-collections` owned the slot. The same
dump showed the *winning* `copyOf` already carried the guards I was about to
write while its losing twin still had the broken body: **a duplicate pair can sit
half-fixed indefinitely**, and nothing fails until registration order changes.

### The VM sometimes lies about identity

`List.of(..).getClass().getName()` reports `java.util.ImmutableCollections$List12`.
The receiver's real class is `cratonvm/internal/Unmodifiable*` — this VM funnels
every unmodifiable view through seven synthetic classes and fakes `getClass()`
via `getclass_immutable_marker`. **A guard written against the name your probe
prints can never fire.** That was the second inert fix of the day, and unlike the
`owns_slot` one, nothing you can READ will reveal it. Re-measure, never re-read.

### Instrument traps, all still live

* a dump/report flag placed **after** the main class is silently ignored — no
  file, no warning, exit 0;
* the report path must be **Windows-shaped** on this host, or the VM prints
  `os error 3`, continues, and the file never appears;
* the report is **not written when the program calls `System.exit`**;
* `tools/flag-census/render-inventory.sh` **REFUSES** to run when
  `flag-surface.txt` disagrees with `INVENTORY`. **Never chain it with
  `render-tokens.sh`** — its refusal scrolls past, one doc regenerates, the other
  silently does not, and the gate stays red for a reason nothing states. Run it
  alone and read its three lines; the `only in INVENTORY:` line names the flag to
  add.

### Landing protocol

```bash
# 1. gate  (this is the whole set; do not shorten it)
cargo test -p cratonvm-types
# Name NOTHING by hand here. `ls native-builtins/tests/` is the authority and it
# GROWS; the hand-written list this replaced named 7 of the 10 that exist.
cargo test -p cratonvm-native-builtins --tests
cargo test -p cratonvm-native-builtins --features management --tests
cargo test -p cratonvm-native-builtins --features synthetic-jdk --tests
# plus --lib for any crate you changed

# 2. the three arms, on a RELEASE build of the merged tree
CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
SUITE=all                bash regression-suite/run.sh
SUITE=core               bash regression-suite/run.sh

# 3. push, in its OWN command, keyed on the gate RESULT
git push origin HEAD:dev
```

**Do not chain the push behind the gates.** I landed a red `doc_citation_paths`
on `dev` earlier in this campaign by keying the conditional on `behind=0` instead
of on the test result.

**Why the gate list stopped naming targets (2026-08-29, L7).** It used to name
seven `--test` targets. `native-builtins/tests/` holds **ten**, and the three it
omitted were `lock_discipline_ratchet`, `eintr_ratchet` and `aes_gcm_kat`. The
first is not a rounding error: it holds this crate to a raw-lock-construction
baseline because **this crate re-enters the VM** — a native callback calls back
into Java, which takes the heap and L10 class-manager locks — so a `Mutex` here
with no `LockLevel` is a deadlock the order checker cannot see. It caught
exactly that in L7's own instrument, on a commit whose other nine gates were
green. A lane following the old list, on its promise of being "the whole set",
would have landed it.

**An unknown `--test` name exits 101, the same code a panicking test gives.**
Seven "failing ratchets" in L7's landing script were seven stale names, and the
output — tail-truncated — was cargo listing the targets that DO exist, which
reads as a list of failures. If a sweep of unrelated guards goes red
identically, suspect the invocation before the tree, and read the FIRST line of
the output rather than the last.

`--features synthetic-jdk` is in the set as well: that mode builds and runs
again as of 2026-08-29
(`P4B-synthetic-jdk-mode-run-for-the-first-time-20260829.md`), and a
`#[cfg(feature = "synthetic-jdk")]` module that nothing compiles rots silently.

Two consequences of `--tests`, both measured on 2026-08-29 rather than inferred:

* **It subsumes `--lib`**, so the line under it is redundant for this crate —
  and the known-red `properties_sidetable` guard two sections down now shows up
  in the gate command itself, `rc=101`, on every branch. Read that section
  before you bisect it.
* **The feature arms genuinely cover more**, which is the argument for running
  all three: 4176 tests on default, 4208 under `management`, 4352 under
  `synthetic-jdk`. The third arm alone compiles 176 tests nothing else does.

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

### Known-red GATE on `dev`, 2026-08-29 — not a vector, so the list above misses it

`cargo test -p cratonvm-native-builtins --lib` is **4176 passed, 1 failed** on
`origin/dev` as of `a5c67dcda`:

```
properties_sidetable::tests::only_order_insensitive_functions_read_the_unordered_snapshot
  these functions read the UNORDERED side-table snapshot:
  ["native_properties_clone", "native_properties_replace_all"]
```

**It is not your merge, and you can prove that without building anything.** The
test is a source witness over ONE file — `include_str!("properties_sidetable.rs")`
— so its verdict is a pure function of that file's bytes. `git diff origin/dev --
native-builtins/src/properties_sidetable.rs` is empty on any branch that has not
touched it, which makes the red identical to pristine `dev`'s.

It arrived with `5a6348d28` (`Properties.clone()`/`replaceAll()` NPE), whose own
new guard it is: the guard and the two functions it names landed in the same
commit. Left for that lane rather than silenced here, because the guard's two
exits are not equivalent and picking between them is a behavioural call, not a
gate-quieting one — `Properties.clone()` hands its key order to Java through
`keys()`/`stringPropertyNames()`, and `replaceAll` applies a user function in
that order, so "add it to ALLOWED" would be the wrong exit for both.

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

**OPEN, unowned, and worth someone's morning:**
`warm_null_receiver_invokes_throw_npe_jit` — once its inline cache is warm,
`invokespecial` on a NULL receiver does not throw, and the callee runs with
`this == null`. JVMS §6.5, and the test names the consequence it was written
for: the bogus `Cannot read field "interfaces" because "rd" is null` at
`Class.java:1217`. Reproduced identically on four binaries spanning this
session, so it is not recent. `invokevirtual` and `invokeinterface` both throw
correctly; only `invokespecial` is wrong.

### dev's tip is frequently red

Seventeen undeclared-or-unfixtured flags across seven merges today, plus two
rounds of `docs/internal` citations. Expect to clear something that is not yours
on most merges; verify with `git diff origin/dev -- <file>` before editing, then
fix it, because a red gate blocks every lane.

---

## 6. What "done" means for a lane

Not "my probe is green". A lane is done when:

1. every `native-won` triple in its families is covered by a differential probe,
2. the probe is 0-diff in both modes, or each residual row has a record naming
   the measurement and why it was not fixed,
3. the record says what PASSED as well as what failed — that is what tells the
   next person where the work is not,
4. gates and the three arms are green on a merged tree, and it is pushed.

A 0-diff probe is a **precondition** for retiring a shadow, never a
justification on its own: a native may exist because the bytecode path was
measured slower, or because it was measured WRONG once and the native is the fix.
Read the registrar's history and count invocations before proposing a retirement.
