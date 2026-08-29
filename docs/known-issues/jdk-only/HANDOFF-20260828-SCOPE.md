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
| **L2 StringBuilder / StringBuffer / AbstractStringBuilder** | **DONE — landed on dev as `3e8280b94`, 2026-08-29** | `/data/cvm-l2s-20260828` (Linux build host) | `claude/l2-strings-20260828` |
| **L4 `java.io` / `java.nio`** | **COMPLETE 2026-08-28** — 199 native-won triples, **1616 probe rows, 1615 identical in both modes**; 52 defects fixed and 8 shadows retired; 1 recorded residual (`FileInputStream.skip`, a resolution finding no registrar edit can move). Lane doc retired to `internal/jdk-only/`; record is `L4-the-io-and-nio-worklist-49-defects-and-a-bounds-check-that-killed-the-vm-20260828.md` | `/data/cvm-l4io-20260828` (Linux build host) | `claude/l4-io-nio-20260828` |
| **L7 definition of done** | **DONE 2026-08-28** — all three workloads run to completion under `--jdk-only`, `compatibility_classes: 0` and `synthetic_stub_invocations: 0` on five arms, four VM fixes, 4 recorded residuals. Lane doc retired to `internal/jdk-only/`; record is `the-definition-of-done-run-on-the-three-real-workloads-20260828.md` | `/data/cvm-l7dod-20260828` (Linux build host) | `claude/l7-dod-20260828` |
| **L1 `Unsafe`** | **DONE 2026-08-28** — 516 probe rows, 24 defects fixed, 5 recorded residual categories. Lane doc retired to `internal/jdk-only/`; record is `l1-unsafe-516-rows-24-defects-and-the-sub-word-atomics-that-never-returned-20260828.md` | `/data/cvm-l1u-20260828` (Linux build host) | `claude/l1-unsafe-20260828` |
| **L6 concurrency & threads** | **DONE 2026-08-29** — 109 native-won triples, 546 probe rows, 33 defects fixed, 0 residuals of its own. Lane doc retired to `internal/jdk-only/`; record is `L6-concurrency-lane-complete-20260828.md` | `/data/cvm-l6cc-20260828` (Linux build host) | `claude/l6-concurrency-20260828` |
| **L3 `java.util` collections** | unclaimed — the last one | your own worktree | your own branch |

**L5 is DONE to the bottom — its residuals too — and every file it held is
free.** L2 is taken (see the table). L1, L4 and L6 are DONE. **L3 and L7 are
unclaimed**, and L3 (`java.util` collections, ~380 rows) is the largest lane
left on the board.

**RE-RUN YOUR FAMILY'S EXISTING PROBES ON THE FINAL BINARY, not only the ones
you wrote.** L4's five new probes were all 0-diff and the lane looked finished;
running the four `java.io` probes that were already in the tree found
`probes/FilePathSweep.java` at **94 differing lines** and the largest single
cause in that lane — a path predicate whose own comment claimed it was
platform-independent and was not. A new probe asks the questions its author
thought of, and L4's author was on a Linux host and did not think of
backslashes. Cheap to do, and it is the only step that can catch what your
fixes broke as well as what they missed.

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
| **Phase 4** — the evidence base | **CLOSED 2026-08-28 (L7).** All three workloads ARE checked out on `azure-host-2` — the blocker was a host, not an absence. Five arms, `compatibility_classes: 0` and `synthetic_stub_invocations: 0` on every one, every fabrication request named with its requester `file:line`. |

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

The remaining ~780 rows are a long tail: **71 of the 183 classes have ≤3 rows
each**. Nobody owns the tail yet; finish your lane before taking any of it.

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
| `Arena`/`MemorySegment` report an INTERFACE as an instance's class | `panama.rs` — unclaimed, closest to L1 |
| ~~`AsynchronousFileChannel.write` returns `CompletableFuture` not `PendingFuture`~~ | **FIXED by L6, 2026-08-29**, along with three behavioural gaps beside it that 38 differential rows found. §6 of the same record. |
| `Module.canUse` over-approximates | **L5 (mine)**, documented in the registrar |
| `KeyStore.getInstance("JCEKS")` unsupported | unclaimed; NOT a `--jdk-only` item, missing in both modes |
| `java/lang/StringBuilder` cluster | `WORKER-3-NOTE-3` has it open — **L2 must check that note first** |
| **NEW.** `Properties.values().iterator()` mints the fabricated `cratonvm/internal/ArrayListViewItr`, and its `try_alloc_synthetic(..)?` has **no refusal arm** — so `--jdk-only` kills the caller with `NoClassDefFoundError`. This is what `RJdkEnumerations` fails on under `--jdk-only` now that L6 fixed the CHM half. | **L3** (`Properties`/`Hashtable` cluster). Its two sibling mint sites land refusals on `real_snapshot_iterator`, which needs a `SnapshotItrRoute` for a Hashtable-backed values view. Falling back to `java/util/ArrayList$Itr` instead would add a `modCount`-less receiver to that class, which is the precondition the bytecode-yield allow-list is waiting on. |
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
cargo test -p cratonvm-native-builtins --test stub_ratchet --test registrar_drift \
  --test registrar_reachability --test essential_wiring_ratchet \
  --test duplicate_registration_gate --test shim_inheritance_guard --test registry_contracts
cargo test -p cratonvm-native-builtins --features management --test stub_ratchet \
  --test registrar_drift --test registrar_reachability
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

### Known-red vectors, so you can tell yours from theirs

* ~~`RJdkEnumerations`~~ — **GREEN as of 2026-08-29, in all three arms.** Two
  lanes, two halves. L6 fixed the compatible-mode half (the CHM values cursor,
  dev's `a0168ed03`). The `--jdk-only` half was a REFUSED FABRICATION THE
  CALLER COULD NOT RECOVER FROM: `Properties.values().iterator()` minted
  `cratonvm/internal/ArrayListViewItr`, strict mode correctly refused it, and
  the mint site's bare `?` handed the refusal to the caller as a
  `NoClassDefFoundError`. Fixed by recovering onto a real `Arrays$ArrayItr`
  snapshot — `a-refused-fabrication-the-caller-cannot-recover-from-20260829.md`.
  **There is now no known-red vector in the corpus.**
* `RExceptions` and `RJdkFailure` — **red on `dev` from `c6ccccbc8` (the L5
  lane) until `L5-residuals-...-20260828.md` §6 fixed it. If you ran the arms in
  that window you saw two reds that were not yours and are not yours to chase.**
  Both assert the same thing: an array `ClassNotFoundException` must name the
  ELEMENT, not the descriptor. Fixed; verify against a binary newer than that
  fix before spending anything on them.
* `RBlockingQueue` — a documented flake (`HANDOFF-20260812.md`, "do not chase
  it"). One failure under suite load, passes standalone and on repeat.

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
