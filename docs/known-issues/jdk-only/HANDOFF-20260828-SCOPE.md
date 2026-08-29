# `--jdk-only`: total scope, and seven parallel lanes — 2026-08-28

**Read this before your lane doc.** It has the whole picture, the method, the
rules that keep seven workers from colliding, and the traps that have each cost
a build cycle today.

Lane docs: `HANDOFF-20260828-L1-unsafe.md` … `L7-definition-of-done.md`.

---

## 0. Who is where — CHECK THIS FIRST

| lane | owner | worktree | branch |
| --- | --- | --- | --- |
| **L5 reflection & class metadata** | **COMPLETE 2026-08-28** — 483 rows, 20 fixed, 0 residuals | `C:\craton\cratonvm\.claude\worktrees\h2-known-issues-206dee` | `claude/jdk-only-mode-handoff-09b48c` |
| **L2 StringBuilder / StringBuffer / AbstractStringBuilder** | **TAKEN 2026-08-28** | `/data/cvm-l2s-20260828` (Linux build host) | `claude/l2-strings-20260828` |
| **L4 `java.io` / `java.nio`** | **DONE 2026-08-28** — 199 native-won triples, 1461 probe rows, 49 defects fixed, 3 recorded residuals. Lane doc retired to `internal/jdk-only/`; record is `L4-the-io-and-nio-worklist-49-defects-and-a-bounds-check-that-killed-the-vm-20260828.md` | `/data/cvm-l4io-20260828` (Linux build host) | `claude/l4-io-nio-20260828` |
| L1, L3, L6, L7 | unclaimed | your own worktree | your own branch |

**L5 is DONE and `lang_class.rs` is free again.** L2 is taken (see the table).
Everything else is unclaimed.

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
| **Phase 4** — the evidence base | instrument built and consumed (`difftest/src/census.rs`); the three DoD workloads are **not checked out on this host**. That is L7. |

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

### OPEN, owned, do not duplicate

| item | owner |
| --- | --- |
| `ConcurrentHashMap.elements()` never terminates | **dev's `a0168ed03`**, bisected in two builds. L6 must know; it is not L6's to fix without talking to that lane. |
| `Arena`/`MemorySegment` report an INTERFACE as an instance's class | `panama.rs` — unclaimed, closest to L1 |
| `AsynchronousFileChannel.write` returns `CompletableFuture` not `PendingFuture` | L6 |
| `Module.canUse` over-approximates | **L5 (mine)**, documented in the registrar |
| `KeyStore.getInstance("JCEKS")` unsupported | unclaimed; NOT a `--jdk-only` item, missing in both modes |
| `java/lang/StringBuilder` cluster | `WORKER-3-NOTE-3` has it open — **L2 must check that note first** |

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

* `RJdkEnumerations` — dev's `a0168ed03`, bisected, recorded. Expect it red in
  the strict and `all` arms.
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
