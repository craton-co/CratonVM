# Operating a `--jdk-only` lane

| | |
|---|---|
| **Status** | Permanent. This is process, not a work item; it does not retire when a campaign does. |
| **Normative source** | [`../feature-designs/jdk-only-mode.md`](../feature-designs/jdk-only-mode.md) |
| **Companions** | [`../jdk-only-migration.md`](../jdk-only-migration.md) · [`../jdk-only-native-review.md`](../jdk-only-native-review.md) · [`../known-issues/jdk-only/INDEX.md`](../known-issues/jdk-only/INDEX.md) |

**Why this page exists separately from any handoff.** The eight-lane campaign of
2026-08-28/29 ran from `known-issues/jdk-only/HANDOFF-20260828-SCOPE.md`, whose
§3 and §5 were the operating rules every lane worked from. Those rules outlived
the campaign, and leaving them in a handoff meant the handoff could not retire
without taking them out of circulation. Everything below is durable; the dated
campaign material stays where it was.

Each rule here was learned by getting it wrong once. The cost is recorded with
the rule, because that is the part that makes it stick.

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
When the check's input is the whole registry rather than one file, fall back to
running the same gate on a pristine `origin/dev` worktree before blaming yours.

**A red that moves with load is not automatically a flake.** A TLS vector looked
exactly like one — green, red, green across three solo runs, and failing on the
HOTSPOT side while CratonVM passed its own checks. It was the vector's own
client loop discarding the reply it asserts on, because `unwrap()` consumes one
TLS record per call and three arrive in a single read under load. **Read the
page the vector is documented on before writing a diagnosis of it.**

The current known-red list is dated material and lives with the campaign that
measured it, in `known-issues/jdk-only/`. Re-derive rather than trust a list
older than a day.

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
