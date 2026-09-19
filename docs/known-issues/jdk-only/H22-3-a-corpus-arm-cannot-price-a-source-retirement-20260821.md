# H22-3 — a corpus arm cannot price a source retirement, and here are the four measured reasons

**Status: OPEN — MEASURED (each reason carries its own arm or dump), ARGUED
(the generalisation).** Companion to `H22-1` (what landed) and `H22-2` (what
must not). All measurements on the prebuilt `C:/craton/cratonvm-r5.exe`
against HotSpot 25.0.3+9.

Lane H22, 2026-08-21.

`H14-3` §6 is the most careful "what this does not establish" section in this
directory, and it named three of these four. This record's contribution is that
**all four now have a number**, and two of them changed a verdict.

---

## 1. The four gaps, and what each cost

| # | gap | `H14-3` named it? | measured here | verdict it moved |
|---:|---|---|---|---|
| 1 | the armed class set ≠ the registrar's class set | yes, in one sentence | **2 of 3** and **26 of 62** classes | both `H14-3` N1 and N2 |
| 2 | prefixes are not additive | yes, "nobody has run a combination" | two free arms → **`rc=1`** together | `H14-3` N2 |
| 3 | `owns_slot: false` twins turn a deletion into a promotion | yes, §5 of `H14-1` | **16 of 16** on one class | `H14-3` N3 (the one that landed) |
| 4 | the dial is `--jdk-only`-scoped; the source is not | yes | premise checked in Compatible mode | none — the premise held |

### 1.1 The class set (MEASURED)

`CRATONVM_ENFORCE_NATIVE_SHADOW` takes class-name **prefixes**, and `H14-3`
built each cell's prefix list from the shadow CSV — i.e. from classes the
corpus *observed*. A retirement deletes registrations, observed or not.

| registrar | armed | registers on | priced | deletes |
|---|---:|---:|---:|---:|
| `register_string_builder_natives` | 2 | 3 | 128 | 192 |
| `register_throwable_subclass_natives` | 26 | 62 | ~390 | 906 |
| `register_p64_hex_format` | 1 | 1 | 26 | 26 |

The missing class in row 1 is `java/lang/AbstractStringBuilder`, and it is
missing from the CSV for a reason that is itself a measurement: with
`--nojit CRATONVM_DISABLE_INTRINSICS=1`, its 64 registrations take **0**
dispatches while `StringBuilder`'s take 93 and `StringBuffer`'s 28 in the same
run. Dispatch keys on the receiver (`H11-1`), nothing is ever *an*
`AbstractStringBuilder`, so it can never appear in a shadow row — **and it is
the class that makes the retirement fatal** (`H22-2` §2).

**Rule: build the armed class list from `--dump-native-registry`, joined by
registrar, never from the shadow rows.** The rows are a subset by construction
and the subset is not random — it excludes exactly the superclasses that
absorb dispatch, which are exactly the ones a retirement exposes.

### 1.2 Non-additivity (MEASURED — the first counter-example)

`H14-3` §6: *"Each cell was measured alone; their sum is not the cost of arming
several, and nobody has run a combination."* Run:

| arm | result |
|---|---|
| `java/lang/StringBuilder`,`java/lang/StringBuffer` | 517 lines, 16 diff — free |
| `java/lang/AbstractStringBuilder` | 517 lines, 16 diff — free |
| **both** | **0 lines, `rc=1`, `ArrayStoreException`** |

Two zeroes that sum to a crash. The mechanism is general and worth naming: **a
native on a subclass and a native on its superclass are each other's safety
net.** Arming one leaves the other to catch the call; arming both is the first
time real bytecode runs end-to-end through the class hierarchy, and that is the
first time the object model has to be right. Any registrar spanning a
superclass/subclass pair has this shape, and `H14-1` §3.1 shows several
(`register_hashset_natives`: `HashSet` + `LinkedHashSet`;
`register_tree_set_natives`: `TreeSet` + `TreeMap$KeySet`;
`register_io_natives`: three stream classes).

**Rule: a per-family cell is a lower bound on the cost of that family and says
nothing about any other. The only arm that prices a retirement is the one armed
on exactly the classes the retirement deletes.**

### 1.3 The losing twin (MEASURED)

`java/util/HexFormat` carried 42 registrations: 26 owning, and **16 that were
all `owns_slot: false`**, from a second registrar that itself called the winner
last. `H14-1` §5 predicted the failure mode for the *loser* ("a retirement
aimed at a losing registration... measures as no effect"). The real instance
was the mirror image and more dangerous: retiring the **winner** would have
promoted sixteen bodies that a previous record (`W8-C15-2`) had already
condemned as never reading the receiver and panicking on ordinary input — and
the census would have counted it as a 24-row win.

**Rule: before deleting a registration, dump the triple and read every
registration of it, not just yours. If a `false` twin exists, both go in one
commit or neither does.** 162 of the 1402 rows have a twin.

### 1.4 The mode scope (MEASURED, and this one was fine)

The dial only affects `--jdk-only`. Deleting a registration affects **both**
modes of the real-JDK build. For `register_throwable_subclass_natives` this
looked like a blocker, because its call site says it exists for *synthetic-stub*
Throwable subclasses with no bytecode behind them — a Compatible-mode scenario
the dial cannot reach.

Checked rather than assumed: a Compatible-mode
`--dump-native-registry --explain-jdk-only` (`schema 5`, `mode: compatible`,
1693 synthetic-stub natives present) reports `image_has_class: true` for
**every** registration on the throwable family, on `java/util/HexFormat` and on
all three builder classes. With a real JDK on the class path, fabrication is
not taken in either mode. The premise expired; the gap did not bite.

It would bite in the `synthetic-jdk` feature build, where the class library is
absent by construction — which is why `H22-1` **moved** `register_p64_hex_format`
into the synthetic arm rather than deleting it.

**Rule: a retirement in the real-JDK build must state what the synthetic-jdk
build falls back to. "Nothing" is an answer only if the synthetic gate agrees,
and that gate is BLOCKING at 0 fails.**

## 2. What replaces the corpus arm

Not much, and cheaply. The instrument that produced every result in `H22-2` is
one Java file and one shell line:

```bash
JDK="$(dirname "$(dirname "$(command -v javap)")")"
"$JDK/bin/javac" -d . H22Probe.java
"$JDK/bin/java"  -cp . H22Probe > oracle.txt          # the oracle

CRATONVM_ENFORCE_NATIVE_SHADOW="$(cat class-list.txt)" \
  cratonvm.exe --jdk-only -cp . H22Probe > cv.txt 2> cv.err
diff oracle.txt cv.txt                                 # stdout ONLY
```

`2>&1` would put VM tracing in the diff and make every arm look different.

Three properties that made it work, each of which had a way to go wrong:

* **Every assertion prints a deterministic value**, so the diff line count is
  the result. 517 lines; the unarmed baseline is 16 differing lines and that
  number is the yardstick for every arm.
* **The class list is generated from the registry dump**, joined
  `registered_by` → enclosing top-level `fn`, exactly as
  `regression-suite/probes/shadow-triage.py` does — not typed by hand and not
  taken from the CSV (§1.1).
* **A crash is a result, not an error.** Arms 4 and 5 print zero lines and exit
  1; a harness that treated `rc != 0` as "run failed, retry" would have lost
  the finding. `H22-2`'s table reports `518` differing lines for those arms
  because that is what the diff says, and the reader is told what it means.

Wall time per arm: seconds. `H14-3`'s thirteen arms were hours and needed a
lock, a quarantine and three discarded runs (`H14-3` §5).

## 3. This is not an argument against the corpus arm

The corpus arm answers *"does this break the 105 things we ship against"* and
that is the question that decides whether a PR lands. The probe answers *"does
the real bytecode work at all"*, which is the question that decides whether the
PR is worth writing. Run in that order, the cheap one first, and the expensive
one prices only the candidates that survived.

`H14-3`'s thirteen arms remain the most valuable table in the directory. Both
of the verdicts overturned here were overturned by looking at **what the
registrar registers**, which is a property of the source, not of that table.

## 4. The two rows this lane could not price

`H14-3`'s five free retirements are, by path ownership:

| registrar | file | this lane |
|---|---|---|
| `register_p64_hex_format` (+ its twin) | `native-builtins/src/{phases_late.rs,lib.rs}` | **landed** (`H22-1`) |
| `register_string_builder_natives` | `native-builtins/src/lang_string.rs` | **refused** (`H22-2` §2) |
| `register_throwable_subclass_natives` | `native-builtins/src/lang_misc.rs` | **refused** (`H22-2` §3) |
| `register_array_deque_natives` | `native-collections/src/lib.rs` | **not touched** |
| `register_optional_natives` | `native-collections/src/lib.rs` | **not touched** |

The last two are 51 of the 174 rows. `native-collections/**` was on this lane's
DO-NOT-TOUCH list — the orchestrator was holding uncommitted patches against
it — so they were neither retired nor re-priced. **Both have gap §1.1 open**:
`register_array_deque_natives` registers on `java/util/ArrayDeque` and
`register_optional_natives` on `java/util/Optional`, each one class in the
shadow CSV, but neither has been checked against the registry dump for a class
the corpus never observed, and `ArrayDeque` in particular has a superclass
relationship to `java/util/AbstractCollection` of exactly the §1.2 shape.
`H14-3` N3 also flags `ArrayDeque` for a separate reason (the corpus only
touches a deque's ends; a `delete(i)` probe turned an arm red in 2026-08-12).

## 5. This lane ran no `regression-suite/run.sh` at all

Deliberate, and worth recording because it is a departure. The standing
instruction is one invocation at a time with a lock; `H14-3` §5 measured a
concurrent sweep moving a published cell by **twenty vectors**, and `H14` lane
notes measured a result moving from 83/104 to 102/104 the same way.

The sweep would have answered "does `SUITE=all` still give 100/105 with
HexFormat retired" — and it could not have, because **the prebuilt binary this
lane may run does not contain the edit.** A sweep here would have measured the
unmodified binary and reported the acceptance numbers as though they were about
the change. That is worse than not running it.

So the acceptance criteria (`--jdk-only` 105/105, `SUITE=all` 100/105,
`SUITE=core` 64/65) are **UNVERIFIED for this lane's commit** and belong to
whoever builds it. The prediction is that all three are unchanged and the census
falls by 24 (`H22-1` §6).

## 6. NOMINATIONS

* **N1 — amend `H14-3` §1 with a "classes armed / classes registered" column.**
  Every cell in it has gap §1.1 open; two of thirteen are known wrong; the other
  eleven are unchecked. The column is a `--dump-native-registry` join away and
  it tells a reader which cells are load-bearing.
* **N2 — the pre-retirement checklist, four items, from §1.** (a) class list
  from the registry, not the rows; (b) arm the whole list, not a prefix of it;
  (c) read every registration of every triple, twins included; (d) name the
  synthetic-jdk fallback. Every one of the four has a measured instance behind
  it now.
* **N3 — land the probe** (`H22-2` N6). §2 is its recipe; the file is 517
  assertions over StringBuilder/StringBuffer, HexFormat and 62 throwables.
* **N4 — re-price `ArrayDeque` and `Optional`** on their registry class lists
  before either is retired (§4). 51 rows, and the lane that owns
  `native-collections/**` should do it.
* **N5 — teach the sweep harness that `rc != 0` with zero output is a
  RESULT.** §2's third property. Arms 4 and 5 of `H22-2` are the most
  informative in the table and they produced no summary line at all.
* **N6 — a `--dump-native-registry` diff mode.** Every gap in §1 was found by
  joining a dump to source by hand, three times, with three throwaway scripts.
  The join is `regression-suite/probes/shadow-triage.py`'s and it should take
  `--by-registrar <fn>` and print the class list and the twins.

## 7. `INDEX.md` row

```markdown
- [H22-3](H22-3-a-corpus-arm-cannot-price-a-source-retirement-20260821.md) — `OPEN` · **MEASURED per reason, ARGUED in general.** Four reasons a `CRATONVM_ENFORCE_NATIVE_SHADOW` cell does not price the deletion it looks like it prices, each with a number and two of which changed a verdict. (1) The armed class set comes from the shadow CSV — what the corpus OBSERVED — and a retirement deletes registrations: **2 of 3** and **26 of 62** classes were priced, and the missing ones take 0 dispatches precisely because they are the superclasses that absorb them. (2) **Non-additivity now has a counter-example**: `StringBuilder`+`StringBuffer` free, `AbstractStringBuilder` free, both together `rc=1` — a native on a subclass and one on its superclass are each other's safety net. (3) `java/util/HexFormat`'s twin registrar was **16 of 16 `owns_slot: false`**, so retiring the winner would have PROMOTED bodies `W8-C15-2` condemned. (4) The dial is `--jdk-only`-scoped and the source is not; checked in a Compatible-mode dump (`image_has_class: true` throughout) rather than assumed. §2 gives the replacement instrument: one Java file, one `diff` on stdout, seconds per arm against `H14-3`'s hours. §5 records that this lane ran NO sweep, on purpose, because the only binary it may run does not contain its edit.
```
