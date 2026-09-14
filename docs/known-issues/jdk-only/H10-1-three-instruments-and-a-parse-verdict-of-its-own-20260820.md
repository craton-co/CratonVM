# H10-1 — the tier differential no arm could see, a price table that runs itself, and a parse verdict of its own

**Status: FIXED-UNVERIFIED against CratonVM.** Three commits, all in the
harness, CI and `regression-suite/src`. **No CratonVM binary was built or run by
this lane** — the brief forbids it — so nothing below that says "CratonVM"
was measured. Everything below that says "HotSpot", "rustfmt", "bash -n" or
"hermetic" **was**, and each such claim names the command.

Worktree `C:/craton/cratonvm/.claude/worktrees/agent-a7505bcf6fce37987`,
branch `claude/jdk-only-mode-handoff-09b48c`, base `85b6d84ac`
(**stale-base check applied and it had drifted** — the worktree was cut at
`26e4b5db4`, **56 commits and 62 files behind**, which includes every record
this lane's brief told it to read; `HANDOFF-20260820.md` §8's trap, now five of
five lanes). Lane H10, 2026-08-20.

| | commit |
|---|---|
| **H10-A** `RJitMapTierDiff` | `e6adee5ee` |
| **H10-B** blast-radius matrix as a scheduled job | `b79e1247d` |
| **H10-C** merge parse check | `2f6cfda4d` |

**What I ran:**

* HotSpot 25.0.3+9 (`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`, resolved
  not copied) — `javac`, `java`, `java -Xint`, `javap -c`, over `RJitMapTierDiff`.
* `bash -n` on `run.sh`, on both new scripts, and on the shell embedded in both
  workflow files (extracted from the parsed YAML, not eyeballed).
* `python -c "import yaml"` on both workflow files.
* `rustfmt --check --edition 2021` — on `26e4b5db4`'s `stub_ratchet.rs`, on both
  its parents, on the current one, and over **all 1023 tracked `.rs` files**.
* `bash regression-suite/harness-selfcheck.sh` with `SUITE=all` (needs HotSpot
  only, no CratonVM binary) — see §6.
* A hermetic throwaway git repo with a fake `run.sh`, to exercise every exit
  path of the blast-radius script without a VM.

**What I did not run:** `cratonvm` in any mode, `cargo` anything, the
regression suite's CratonVM half, and the two new CI jobs on GitHub.

> **VERIFIED AGAINST A BINARY 2026-09-02.** "What I did not run" listed
> `cratonvm` in any mode, `cargo` anything, and the regression suite's CratonVM
> half. All three have now been run, on a build from this tree.
>
> **H10-A — its own vector passes.** `RJitMapTierDiff PASS`, in the `--jdk-only`
> arm of a 125-vector run. The vector this lane added to see a tier differential
> "no arm could see" is scheduled, runs, and is green.
>
> **H10-C — the parse verdict is the one that mattered, and it is closed.** §"the
> committed baseline document" records `native-builtins/tests/stub_ratchet.rs`
> as not having parsed since a merge, taking the whole `--test stub_ratchet`
> binary and nine other tests with it. It parses and passes now, in BOTH
> configurations, run separately:
>
> ```text
> management     1645 SyntheticStub of 13897 total   baseline 1645, slack 0
> no-management  1634 SyntheticStub of 13529 total   baseline 1634, slack 0
> ```
>
> **`cargo` more broadly:** `jck_conformance` 3 passed — and that one is worth a
> line, because `E33-R11` records the same command as having "ran zero tests, and
> had since the `#![cfg]` was added". A guard that executed nothing looked
> identical to a guard that passed; it now runs three.
>
> **STILL NOT RUN, and not this note's to run:** the two CI jobs on GitHub
> (H10-B's blast-radius matrix and the merge parse check as scheduled jobs).
> Those execute in GitHub Actions, not on this host, and a local run of the
> scripts is not the same evidence as the jobs firing. That item stays open.

---

## 0. The corpus is 105 vectors now, and every published denominator is stale

`RJitMapTierDiff` is registered in `CORE_CLASSES`, so:

| `HANDOFF-20260820.md` §1 says | it now reads |
|---|---|
| `CRATONVM_ARGS=--jdk-only  104 / 104` | `105 / 105` |
| `SUITE=all  99 / 104` | `100 / 105`, same five failures |
| `SUITE=core  63 / 64` | `64 / 65`, same one failure |

Stated first because four other lanes are comparing against those numbers this
week. **A run reporting `104/105` without naming which vector failed has not
been read.** The corpus went 102 → 104 on 2026-08-19/20 for the same reason;
this is the third growth in two days and it is why H10-B's baseline keys on the
failing SET rather than on a count (§2).

---

## 1. H10-A — `RJitMapTierDiff`: the differential `H4-1` O1 asked for and `H7-1` N1 specified

### 1a. The gap, in one sentence

**Every arm of this suite runs each vector exactly once**, so a method that
answers wrongly only *after* tier-up — or only *before* it — is invisible to all
three arms and to HotSpot's diff.

`H4-1` O1 named that failure mode: *"a tier-dependent wrong answer no arm diffs
for"*. `H7-1` then found three real disagreements in the JIT's collection
helpers, the sharpest being `jit_hashmap_put_direct`, whose out-of-contract arm
`break 'fast`ed to `jit_invoke_dispatch` **after `try_hm_int_fast_put` had
already inserted**. The second execution's "previous mapping" is the value that
same call had just written, so `HashMap.put` returns the wrong thing — in
compiled code only. `H7-1` §5b then established that no value-differential
inside the VM would have caught any of the three, and §6b that no vector in the
corpus runs a map operation cold and warm and diffs the two.

### 1b. `H7-1`'s facts, re-verified rather than trusted

| `H7-1` says | I checked | result |
|---|---|---|
| no vector declares a `ConcurrentMap`-typed variable | `grep -n 'ConcurrentMap' regression-suite/src/*.java` | **zero hits, anywhere** — not even in a comment. `jit_concurrent_hashmap_get_direct`'s shape had never occurred |
| `RJitGc` contains no map at all | `grep -c 'Map' src/RJitGc.java` | **0** |
| the three ladders are single-pass-backend only | read `jit/src/lib.rs`'s three bind sites | confirmed; only `Thread.currentThread`, `Preconditions.checkIndex` and `Reference.reachabilityFence` were added at the optimizing/OSR door |
| all four triples are `bridge`, so strict mode refuses them at bind time | `scripts/baselines/jdk-only-kind-map-25-linux.tsv` | confirmed, all four rows |
| `put` is recognised only on the `invokevirtual java/util/HashMap` arm | `jit/src/lib.rs`, the `invoke_kind == 0` arm | confirmed |
| C1 invocation threshold is 500 | `jit/src/tiered.rs`, `CompilationPolicy::default` | `c1_threshold: 500`, `osr_threshold: 10_000`, `c2_threshold: 20_000` |

**The single-pass-only fact is what dictates the vector's shape.** A hot loop
inside one method reaches the compiled tier through the **OSR** door, and the
OSR door binds none of these helpers. So a fixture built as "one big loop full
of `HashMap.get`" would have been the vacuous species this project has
catalogued twice (`W6-5`, `W7-51`): it would tier up, exercise nothing, and pass.
Instead each shape's map call sites live in **their own small static method**,
driven by an invocation-counted outer loop — tier-up through the C1 front door,
`ITERS = 3000` against a threshold of 500, and every loop kept well under 10 000
back-edges so the *driver* does not OSR-compile and become the thing under test.
This is `RJitMultiArrayClass`'s and `RArrayStoreTiers`' idiom, followed rather
than reinvented, and per-shape methods are also why `cold` is a genuinely
interpreted reading for **every** shape and not only for the first one scheduled.

### 1c. The emitted bytecode, verified with `javap` rather than assumed

`javap -p -c` over the compiled vector. The owner in the constant pool is what
the ladders match on, so this is load-bearing:

```text
  invokevirtual  java/util/HashMap.put:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;   17 sites
  invokevirtual  java/util/HashMap.get:(Ljava/lang/Object;)Ljava/lang/Object;                     11 sites
  invokevirtual  java/util/HashMap.remove:(Ljava/lang/Object;)Ljava/lang/Object;                   1 site
  invokeinterface java/util/Map.get:(Ljava/lang/Object;)Ljava/lang/Object;                          5 sites
  invokeinterface java/util/concurrent/ConcurrentMap.get:(Ljava/lang/Object;)Ljava/lang/Object;     5 sites
  invokeinterface java/util/concurrent/ConcurrentMap.put:(…)…                                       3 sites
  invokevirtual  java/util/concurrent/ConcurrentHashMap.put:(…)…                                    5 sites
```

All four ladder triples are present. The `ConcurrentMap.get` row is the one that
did not exist anywhere in the corpus before this commit.

### 1d. THE HOTSPOT ORACLE COLUMN — measured, so the expected table is recorded rather than assumed

`java -cp <build> RJitMapTierDiff` on HotSpot 25.0.3+9, `rc=0`. **Byte-identical
over three runs (md5 `fc251bedf0f79453ceeaed7530d7f488`) and byte-identical
under `-Xint`** — so the oracle's own JIT does not move it, which is what makes
it usable as a two-tier reference. 0.29 s wall.

| shape | oracle value | what it screens on |
|---|---|---|
| `h00-put-insert-returns-null` | `null` | `put` into an absent key |
| `h01-put-returns-previous` | `a` | **`H7-1` §2c**, the one-overwrite form |
| `h02-get-after-overwrite` | `b` | the map holds the new value |
| `h03-get-absent` | `null` | miss |
| `h04-null-key-roundtrip` | `n` | null key is legal in `HashMap` |
| `h05-null-key-put-returns-previous` | `n1` | null key + previous mapping |
| `h06-string-key` | `sv` | `String` key |
| `h07-alloc-hash-key` | `av` | a key whose `hashCode()` ALLOCATES |
| `h08-collide-chain` | `c0\|c1\|c2` | constant `hashCode` — the bucket chain, not slot 0 |
| `h09-boxed-outside-cache` | `big` | `Integer.valueOf(10_000)`: equal, not identical |
| `h10-put-sequence-previous-values` | `null\|v0\|v1\|v2\|v3` | **the sharpest `H7-1` §2c probe** — five successive puts of one key |
| `h11-fill-size-iterate` | `size=200 iter=200 sum=19900` | **the "silently empty map" shape** (`H4-1` §1, `H0-4` §4) |
| `h12-remove-returns-previous` | `b\|null` | `remove` returns the previous mapping and empties the slot |
| `m00-iface-get-present` | `b` | `invokeinterface Map.get` |
| `m01-iface-get-absent` | `null` | |
| `m02-iface-get-null-key` | `n` | |
| `m03-iface-get-alloc-hash` | `av` | |
| `m04-iface-get-collide` | `c1` | |
| `c00-cmap-get-present` | `b` | **`invokeinterface ConcurrentMap.get`** |
| `c01-cmap-get-absent` | `null` | |
| `c02-cmap-get-null-key` | `NPE` | **`H7-1` §2a's shape**: a helper that answers `null` for an unvalidated key reads `null` here where the contract says throw |
| `c03-cmap-get-string` | `sv` | |
| `c04-cmap-get-alloc-hash` | `av` | |
| `c05-cmap-put-returns-previous` | `a` | |
| `c06-cmap-fill-size-iterate` | `size=200 iter=200 sum=19900` | CHM's "silently empty map" half |

and on every one of the 25 rows, HotSpot reports **`moved=-1`** — no tier split.
Totals: `CK RJitMapTierDiff fails=0`, `CK RJitMapTierDiff checks=75`,
`PASS RJitMapTierDiff (75 checks)`.

`run.sh` diffs stdout against this same oracle on every run, so a wrong entry in
the vector's `EXPECTED` table fails on BOTH VMs rather than silently passing.

### 1e. I MADE IT FAIL ON PURPOSE, BOTH WAYS

`W6-5` and `W7-51` are this project's two catalogued instances of a fixture that
could not fail. A new vector is guilty until falsified.

**(1) Perturbed expected value.** `EXPECTED[10]` changed from
`"null|v0|v1|v2|v3"` to `"null|v1|v2|v3|v4"` — the value the `H7-1` §2c defect
would produce. Recompiled, re-run on HotSpot:

```text
FAILED RJitMapTierDiff h10-put-sequence-previous-values COLD: want=[null|v1|v2|v3|v4] got=[null|v0|v1|v2|v3]
FAILED RJitMapTierDiff h10-put-sequence-previous-values HOT:  want=[null|v1|v2|v3|v4] got=[null|v0|v1|v2|v3]
CK RJitMapTierDiff fails=2
Exception in thread "main" java.lang.AssertionError: 2 divergence(s)
rc=1
```

Restored, and the restoration verified by md5 (`882cf3cb1d162e2ee9b31f6795d35c4b`
before and after), not by eye.

**(2) Injected tier split.** In a scratch copy, `h01` was made to return `"b"`
after its 1500th invocation — i.e. `H7-1` §2c's failure mode simulated exactly:

```text
CK RJitMapTierDiff h01-put-returns-previous cold=[a] hot=[b] moved=1500 iters=3000
FAILED RJitMapTierDiff h01-put-returns-previous HOT: want=[a] got=[b]
FAILED RJitMapTierDiff h01-put-returns-previous TIER-SPLIT at i=1500: cold=[a] became=[b] final=[b]
rc=1
```

**Three independent detectors fired for one defect**: the `CK` line differs from
HotSpot's (so `run.sh`'s cross-VM diff reddens it even if every assertion were
deleted), the HOT assertion fires, and the TIER-SPLIT assertion names the exact
iteration. That redundancy is deliberate — `W7-60` is the record of three
scheduled vectors whose entire evidence was deleted by `extract()`, and a vector
whose only evidence is an in-process assertion has one point of failure.

### 1f. What this vector would have caught, and what it will not

**Would have caught:** `H7-1` §2c, the double-applied `put` (`h01`, `h10`), in
`--real-jdk` mode where the helpers bind. **`H7-1` §2a**, the unvalidated key
answering `null` instead of falling back (`c02`). The `H4-1` §1 / `H0-4` §4
"silently empty map" mechanism the moment it reaches a compiled path (`h11`,
`c06`). And the general `H4-1` O1 class: any answer that differs between tiers.

**Will not catch:** `H7-1` §2b, the missing `safe_native_call_prevalidated_objects`
funnel — that is invisible to a return value, as `H7-1` §5b says. And **under
`--jdk-only` it is expected to be tier-INVARIANT by construction**, because all
four triples are `bridge` and `direct_native_helper` refuses them at bind time;
in strict mode the vector *pins* that refusal rather than probing it. If a
`moved=` ever reads non-negative on a strict arm, that is a bigger finding than
anything in `H7-1` — it means a compiled site bound a collection helper under
`--jdk-only`.

---

## 2. H10-B — the blast-radius matrix, scheduled and non-blocking

`scripts/jdk-only-blast-radius.sh` +
`.github/workflows/jdk-only-blast-radius.yml` +
`scripts/baselines/jdk-only-blast-radius-25-windows.txt`. This is `H0-4` N3.

### 2a. Why it is worth automating at all

`CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix>` makes contract §1.4 enforced rather
than counted for that prefix — the native stops winning and real bytecode runs,
which is what a permanent retirement does. **It is the only instrument in the
tree that prices work before it is attempted**, it costs one env var over a run
that already happens, and until 2026-08-20 it had only ever been swept whole-VM
(`all` → 1/36) or over a 36-vector screen. `H0-4`'s six-row table exists because
somebody remembered to type it.

### 2b. The design decision that matters: the cell is the failing SET, not the count

A pass count moves whenever the corpus grows — and it grew three times in two
days, this lane included. A gate keyed on `103` would go red for a reason nobody
can act on, which is exactly `G89-1`'s species: a ratchet red in blocking CI
since 2026-08-14 that *"adjudicated nothing"* for five days. So the baseline
records, per prefix, **the set of failing vectors** plus **the scheduled corpus
at the time it was taken**, and differences classify as:

| class | meaning | fires the gate? |
|---|---|---|
| `REGRESSION` | a vector that passed under this prefix and now fails | yes |
| `REPAIRED` | it failed and now passes — the plan quoting the old number is stale | yes |
| `NEW-VECTOR` | a failing vector that did not exist when the baseline was taken | **no** |
| `RETIRED` | a baselined vector no longer scheduled | no |

`NEW-VECTOR` is the whole point of recording `!corpus`. Without it, adding
`RJitMapTierDiff` — a map vector, which may well fail under an armed `HashMap` —
would have read as a regression in six cells at once.

### 2c. Non-blocking, and the two records that say why

`schedule:` weekly + `workflow_dispatch:`, `continue-on-error: true`, table into
the job summary and an artifact. **These cells are meant to be red**: a cell is
"how many vectors object if you retire this family *today*". A gate whose red
state is its normal state trains every lane to scroll past it and then cannot
report the abnormal one — `G89-1` above, and `H3-1`, which found a gate in the
same blocking CI that had not *compiled* since a merge. The script still returns
a meaningful exit code for a human (`1` = a cell moved); the workflow turns it
into a `::warning::`.

### 2d. The two things `H0-4` §5 says, carried rather than papered over

The script **prints no total, ever**, and says why in its own output: each
prefix is armed ALONE, so *"their sum is not the cost of arming all six, and
nobody has run that combination"*. It also prints that the prefixes **are not
disjoint in effect** — `LinkedHashMap extends HashMap`, `HashSet` is backed by a
`HashMap` — so two rows overlapping is the class hierarchy, not a contradiction.
`--prefixes 'a+b'` exists so `H0-4` N2 (arm two together and see whether they
compose) is one command rather than a code change.

> **CORRECTION 2026-08-22 — the one-call-site premise below is FIXED.**
> `jdk_only_enforce_shadow_for` now has a call site at every one of the
> fourteen dispatch doors, and the leak that premise describes is gone
> (MEASURED before the fix: 890 of 947 armed `Bridge` dispatches never
> asked the dial). **The direction stated here is also wrong**: armed
> cells taken with the one-door dial were not a floor — `java/util/HashMap`
> scored 81/104 half-armed and 86/104 fully armed, because half-armed is a
> corrupt hybrid, not a partial retirement. See
> `WORKER-1-the-dial-now-reaches-every-door-20260821.md`.

Two more caveats are printed on every run: a green cell means "these 105 vectors
raise no objection", not "this family is retirable" (`G79-1`: the corpus has no
AWT vector at all); and per `H7-1` N2 the dial is read at **one** dispatch site,
so every cell is a **floor** for what a real retirement costs.

It also runs an **unarmed control arm first** and reports every cell net of it.
`H0-4` §4 found four of its six cells inflated by one shared row and re-priced
the whole table on that discovery; the script generalises it, and additionally
reports the **common factor** — the vectors failing under *every* armed prefix —
because `H0-4`'s was one defect with four faces, not four defects.

### 2e. The shipped baseline does NOT adjudicate, on purpose

`scripts/baselines/jdk-only-blast-radius-25-windows.txt` carries `H0-4`'s table
(its CHM row's vector list taken from `H0-3` §2, which `H0-4` cites rather than
repeats) marked `!adjudicating no`. The script prints it as a reference beside
the measured table and **exits 2**. It was transcribed, measured on another
binary, another OS and a 104-vector corpus, and has no `!corpus` line. A
baseline from another key cannot score this one, and "no baseline" must never
read as "nothing moved" — the same posture, and the same reason, as the bridge
ratchet refusing every leg but `25-linux`.

### 2f. Verified without a VM: every exit path exercised

A throwaway git repo with a fake `run.sh` that prints a `run.sh`-shaped
transcript whose failing set depends on `CRATONVM_ENFORCE_NATIVE_SHADOW`:

| scenario | expected | got |
|---|---|---|
| no baseline | 2 | **2**, table printed |
| `--update-baseline` then re-run | 0 "unchanged" | **0** |
| a vector newly failing under two armed prefixes | 1 `REGRESSION` | **1** |
| a vector no longer failing | 1 `REPAIRED` | **1** |
| a failing vector absent from `!corpus` | 0 `NEW-VECTOR` | **0** — does not fire |
| baseline with `!adjudicating no` | 2 + reference rows | **2** |
| an arm that prints no `COUNTS:` line | 3 | **3**, "NOTHING was measured" |

A gate never shown to fire is decoration — this feature has shipped three inert
ones (`bridge-ratchet.sh`'s own header says so).

### 2g. What it would have caught

Nothing yet, and that is the honest answer: it is a *pricing* instrument, not a
defect detector. What it *removes* is the failure mode `H0-4` §2 describes —
`G88-1` §5 retagged eight registrars together and read the breakage as evidence
that *"VM-owned state is pervasive"*, when the tree's own answer was a **23×
spread** available for six commands and no build. It would have caught that
argument.

---

## 3. H10-C — a parse verdict of its own

`scripts/merge-parse-check.sh` + a new `merge-parse` job in
`.github/workflows/ci.yml`.

### 3a. What it would have caught, named: `H3-1`

`native-builtins/tests/stub_ratchet.rs` **had not parsed since merge
`26e4b5db4`**, which spliced two versions of one failure message and kept both
argument lists. The whole `--test stub_ratchet` binary went with it; that gate is
the cited evidence for the P0 *Residual synthetic native set* row; it was
blocking CI in **both** configurations; and `origin/dev` still carried the break
at `d8b40ff8f` days later. With `G89-1`'s finding that the same gate had been RED
since 2026-08-14, it adjudicated nothing for six days and then adjudicated
nothing at all.

**Independently reverified here, before writing anything:**

```text
rustfmt --check --edition 2021  <26e4b5db4:native-builtins/tests/stub_ratchet.rs>
  -> 32 errors on STDERR, ALL lexer/parse ("unknown start of token", first at 1159:88)
  -> 0 mod-resolution errors, 0 bytes on stdout
same file at HEAD
  -> 0 stderr, 1565-byte formatting diff on stdout
```

32 is exactly `H3-1`'s number, reached independently.

### 3b. THE FINDING THAT CHANGES THE ITEM: `rustfmt --check` was already there

`.github/workflows/ci.yml` has run a `fmt` job over the diff-touched `.rs` files
since it was split out — `rustfmt --check --edition 2021 "${files[@]}"`,
blocking. So the naive form of this item ("add `rustfmt --check` over the merge's
files") was **already implemented and did not report this defect.** The reason is
mechanical:

> `rustfmt --check` exits **1** for two unrelated reasons, and the `fmt` job
> reads only the exit code.
>
> * a formatting diff → **stdout**, `Diff in <file>:<line>:`
> * a parse error → **stderr**, `error: …`

Measured above: the broken file is stderr-only with zero stdout; the repaired one
is stdout-only with zero stderr. And a whole-tree sweep of **all 1023 tracked
`.rs` files** gives:

```text
SUMMARY: 418 clean, 605 formatting-only, 0 PARSE FAILURE(S), 0 UNRESOLVED MODULE(S)
```

**59% of the tree prints a formatting diff.** So a red `fmt` almost always means
the harmless thing, and a verdict that conflates "this file is laid out
differently" with "this file is not Rust" cannot report the second. The item is
therefore not *add a check*; it is **separate the two verdicts**. The new job
reads stderr only, so its red means exactly one thing and is always actionable.

That whole-tree `0 PARSE FAILURE(S)` is also what licenses making the new job
**blocking**: it cannot go red for a pre-existing reason. `H0-4` §5's discipline
applied to my own gate — verify the floor before arming.

### 3c. Design points that are measurements, not taste

* **Every `.rs` file the diff touched, never a fixed list**, plus — when HEAD is
  a merge — the diff against **every** parent. Replayed on the real merge, the CI
  step's file-set logic yields **exactly the 147 `.rs` files `H3-1` swept**, with
  `native-builtins/tests/stub_ratchet.rs` among them at position 80.
* **It runs in place.** rustfmt follows `mod foo;`. Both parents of `26e4b5db4`
  report `failed to resolve mod boot_path` when checked *out of tree* and are
  silent when checked *in* it — a scratch-directory check is a false-positive
  factory that looks exactly like a merge deleting a module.
* **A separate job, not a step.** The `fmt` job's own header records that being
  the first step of `build-and-test` kept `cargo clippy --workspace
  --all-targets` off `dev` for weeks. Two verdicts that can each be red must not
  be able to hide each other.
* **The edition is read from `Cargo.toml`.** rustfmt defaults to edition 2015,
  where `async fn` is a parse error. A hard-coded year is a false-positive
  factory after the next bump.
* Two fatal classes are kept apart — `DOES NOT PARSE` (a broken file) and
  `UNRESOLVED MODULE` (a missing one) — because the remedies differ, with
  `--skip <glob>` for the one legitimate case (an `include!`/`#[path]` fragment
  that cannot be checked standalone).

### 3d. What it does NOT catch, said plainly in both the script and the job

**It is a parse check, not a type check.** A file that parses can still fail to
compile in every way that matters — including a variant of `H3-1`'s own defect:
`assert!(c, "a {}", x, "b {}", y)` parses fine as a macro token tree and the
compiler rejects it. Also unresolved names, missing imports, borrow errors,
trait bounds, and anything inside a macro body that is never expanded. **A green
result here is not "it compiles."** It runs in front of `cargo check`, never
instead of it, and a CI file that puts it where `cargo check` used to be has made
things worse.

---

## 4. Where I found the brief or a record wrong about the tree

**4a. The brief's H10-C framing.** *"`rustfmt --check` over the files a merge
touched catches this in seconds"* — true, and it was **already in blocking CI**
and did not catch it. §3b. The item's value is the stream split, not the tool.

**4b. `H0-4` §1's table cannot be used as an adjudicating baseline** and the
record does not say so. Its numbers are keyed to a 104-vector corpus on a
Windows binary at `db71dfb40`; the corpus is 105 today. §2e ships them as a
reference that the gate explicitly refuses to score against.

**4c. `H7-1` §6b's `RChmKeySetView` finding understates itself.** It says every
receiver there is declared `ConcurrentHashMap` or `Map`. The stronger and
checkable statement is that `ConcurrentMap` appears **nowhere in
`regression-suite/src`** — not as a declaration, not as an import, not in a
comment. `jit_concurrent_hashmap_get_direct` had zero corpus reach in either
mode.

---

## 5. VERIFICATION PLAN

### 5a. What the orchestrator runs

1. **`bash regression-suite/harness-selfcheck.sh` with `SUITE=all`.** Needs
   HotSpot only, no CratonVM binary. Ratifies that `RJitMapTierDiff` is
   scheduled, that it survives `extract()` with a CK line and a check count
   (G2/G3), that the G5 mode scan does not fire on it, and that no list-hygiene
   guard trips. Ran here — see §6.
2. **The three arms.** Expect exactly the §0 numbers: `--jdk-only` **105/105**,
   `SUITE=all` **100/105** with the same five, `SUITE=core` **64/65** with
   `RImmutableFactoryTypes`. Any other shape is the finding.
3. **`ONLY=RJitMapTierDiff bash regression-suite/run.sh`, then again with
   `--nojit`.** Red without the flag and green with it isolates a divergence to
   the compiled tier, which is where this class of defect lives.
4. **`STRICT_COVERAGE=1`** on any arm — the new vector must not produce a
   coverage warning.
5. **`bash scripts/merge-parse-check.sh --all`** once, on any branch:
   expect `0 PARSE FAILURE(S)`. If it is non-zero, something landed that does not
   parse and the new CI job is about to be correctly red.
6. **`bash scripts/jdk-only-blast-radius.sh --update-baseline --note '…'`** on
   whatever host takes the first real sweep. Until then the job exits 2 and says
   so. Budget ~7 full strict arms, sequential.

### 5b. For H10-A: what a FAILING run looks like versus a passing one

This is the section a new vector needs most, because a vector that passes
trivially is the `W6-5`/`W7-51` species.

**PASSING** — 25 `CK` lines, every one of the form

```text
CK RJitMapTierDiff <shape> cold=[<oracle value>] hot=[<same>] moved=-1 iters=3000
CK RJitMapTierDiff fails=0
CK RJitMapTierDiff checks=75
PASS RJitMapTierDiff (75 checks)
```

with `cold`, `hot` and the oracle column in §1d all agreeing, and `rc=0`.

**FAILING, tier split** (the defect this exists for):

```text
CK RJitMapTierDiff h01-put-returns-previous cold=[a] hot=[b] moved=1500 iters=3000
FAILED … HOT: want=[a] got=[b]
FAILED … TIER-SPLIT at i=1500: cold=[a] became=[b] final=[b]
CK RJitMapTierDiff fails=2
AssertionError: 2 divergence(s)          rc=1
```

`run.sh` reddens it twice over: the assertion sets `rc != 0`, **and** the `CK`
line differs from HotSpot's, so `output differs from HotSpot` fires
independently of every assertion in the file.

**FAILING, both tiers wrong** — `cold` and `hot` agree with each other and
disagree with the oracle; `moved=-1`. That is *not* a tier bug, it is an ordinary
wrong answer, and the two `FAILED … COLD:` / `… HOT:` lines say which.

**THE TRIVIAL PASS TO WATCH FOR.** If `moved` reads `-1` on all 25 rows under
`--real-jdk` **and** the run is fast enough to suspect nothing compiled, the
vector may be measuring one tier twice. The check that settles it costs nothing:
run it under `--nojit` and compare wall time and output. Identical output is
expected; identical *time* means no compilation happened and the second half of
every row is decoration. `H7-1` §5a's `collection_direct_helper_sites()` counter
answers it directly if that lane's change is in the binary — a `(>0, >0, >0)`
under `--real-jdk` is proof the ladders bound.

### 5c. PREDICTED, with falsifiers

| prediction | falsified by |
|---|---|
| `RJitMapTierDiff` passes on CratonVM in all three arms | any red. On `--jdk-only` a red is a bigger finding than on `--real-jdk`, because strict mode should have no second tier here at all |
| every `moved=` reads `-1` under `--jdk-only` | a non-negative one, which would mean a compiled site bound a collection helper despite all four triples being `bridge` — it falsifies `H7-1` §3b |
| the other 104 vectors' verdicts are byte-identical | any movement. Nothing here touches the VM |
| the stub ratchet does not move | any movement — no registration or kind is touched |
| the new `merge-parse` CI job is green on `dev` | a parse failure, which the whole-tree sweep says does not exist today |
| the `fmt` job's behaviour is unchanged | it is not edited; the new job is additive |

---

## 6. The one CratonVM-free gate I could run, and its result

`JDK="C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot" SUITE=all bash
regression-suite/harness-selfcheck.sh` — the harness's own soundness check,
which needs no CratonVM binary. It compiles and runs every scheduled vector on
HotSpot and applies G1–G5.

### 6a. RESULT — measured, at `2f6cfda4d`

```text
HARNESS SELF-CHECK: 105 vectors sound, 0 flagged      rc=0
```

Three things this establishes, none of which needed a VM:

1. **The denominator really is 105.** The self-check reads the class lists out
   of `run.sh`, so this is `run.sh`'s own arithmetic and not mine. §0's numbers
   follow from it.
2. **`RJitMapTierDiff` is sound as an instrument, not merely green.** G2 (does
   anything survive `extract()`) and G3 (does it publish a check count) both
   pass on it — the guards that exist because three scheduled vectors once
   reduced to the constant `PASS <Class>`, which always matches itself
   (`W7-60`, `W7-51`). It publishes 25 `CK` rows and `checks=75`.
3. **No list-hygiene or coverage error.** No unregistered source, no list entry
   without a source, and the G5 mode scan does not fire — the vector's source
   names no non-default runtime mode, checked with `grep -c 'synthetic-jdk'` = 0
   before it was registered.

What it does **not** establish: anything about CratonVM. The self-check runs
HotSpot only, by design.

---

## 7. OUT-OF-FILE EDITS REQUIRED

I own `regression-suite/src/**`, `regression-suite/run.sh`,
`.github/workflows/**`, `scripts/**` and this record. Everything below is
outside that and is written as an exact patch.

### 7a. `docs/known-issues/jdk-only/INDEX.md` — append at the end of the H-wave block

**Add after the `H8-1` row:**

```text
- [H10-1](H10-1-three-instruments-and-a-parse-verdict-of-its-own-20260820.md) — `FIXED-UNVERIFIED` against CratonVM; **HotSpot, rustfmt and hermetic halves MEASURED**. Three instruments. (1) `RJitMapTierDiff`, the tier differential `H4-1` O1 asked for and `H7-1` N1 specified: 25 map shapes read cold and after invocation-counted tier-up, `moved=` published on every row, HotSpot oracle column recorded in §1d, **falsified both ways before landing**. It is the corpus's 105th vector, so **every published denominator gains one** (§0). It carries the first `ConcurrentMap`-typed variable in the corpus — that shape had never occurred. (2) `H0-4`'s blast-radius table as a scheduled, non-blocking job that keys on the failing SET rather than a pass count, so corpus growth reads as `NEW-VECTOR` and not as a regression; prints no total, because the cells do not sum. (3) **`rustfmt --check` was ALREADY in blocking CI over the diff-touched files and did not report `H3-1`** — it exits 1 for a formatting diff (stdout) and for a parse error (stderr) and the `fmt` job reads only the exit code, in a tree where **605 of 1023 `.rs` files print a diff**. A parse verdict of its own, reading stderr only.
```

### 7b. `docs/known-issues/jdk-only/HANDOFF-20260820.md` §1 — the denominators

**Current text (§1, the fenced block):**

```text
  CRATONVM_ARGS=--jdk-only   104 / 104
  SUITE=all                   99 / 104   RImmutableFactoryTypes RJdkProxyIface
                                         RJdkFunctionCombinators RJdkEnumerations
                                         RServiceLoaderDoubleSource
  SUITE=core                  63 /  64   RImmutableFactoryTypes
```

**Replace with:**

```text
  CRATONVM_ARGS=--jdk-only   105 / 105
  SUITE=all                  100 / 105   RImmutableFactoryTypes RJdkProxyIface
                                         RJdkFunctionCombinators RJdkEnumerations
                                         RServiceLoaderDoubleSource
  SUITE=core                  64 /  65   RImmutableFactoryTypes
```

**and add immediately below it:**

```text
The corpus grew to 105 on 2026-08-20 (`RJitMapTierDiff`, H10-1). The failing SET
is unchanged; only the denominator moved. This is the third growth in two days,
which is why the blast-radius baseline keys on the failing set rather than on a
count.
```

**PREDICTED, not measured** — I could not run CratonVM. If the strict arm comes
back `104 / 105`, `RJitMapTierDiff` is red and that is a finding, not a
bad registration; §5b says how to read it.

### 7c. `docs/known-issues/jdk-only/H0-4-the-blast-radius-table-20260820.md` §6 — close N3

**Current:**

```text
* **N3 — put this matrix in CI as a scheduled non-blocking job.** It is six env
  vars over an existing run and it is the only instrument in the tree that
  prices a retirement before it is attempted. Today it exists because somebody
  remembered to type it.
```

**Replace with:**

```text
* **N3 — DONE (`H10-1` §2).** `scripts/jdk-only-blast-radius.sh` +
  `.github/workflows/jdk-only-blast-radius.yml`, scheduled and non-blocking.
  Note two things it does differently from this record: it keys on the failing
  SET rather than the pass count, because the corpus grew three times in two
  days; and §1's table is shipped as a NON-adjudicating reference baseline,
  because it was taken on another binary, another OS and a 104-vector corpus.
```

### 7d. `docs/known-issues/jdk-only/H7-1-…-20260820.md` §8 — close N1

**Prefix N1's paragraph with:**

```text
**N1 — DONE (`H10-1` §1), as `RJitMapTierDiff`, `e6adee5ee`.** Built to this
specification, including the `ConcurrentMap`-declared local this record notes
the corpus has none of. Two deviations worth knowing: the operations sit in
per-shape methods rather than one loop, because §6b's own "single-pass-backend
only" note means an OSR-compiled loop binds nothing; and a "key whose
`hashCode()` allocates" is included as specified, alongside a constant-`hashCode`
collision key for the chain walk. HotSpot oracle column in `H10-1` §1d.
```

### 7e. Nothing else needs editing

Checked, so nobody re-derives it: `regression-suite/jdk-only-coverage.txt` maps
only `RJdk*` vectors to blocker rows and takes no entry for an `RJit*` one;
`regression-suite/harness-uncounted.txt` is for vectors that publish no check
count, and this one publishes 75; and `regression-suite/harness-selfcheck.sh`
**reads the class lists out of `run.sh`** (`sed -n 's/^CORE_CLASSES="\(.*\)"$/\1/p'
… | head -1`), so it needs no edit — but note that `head -1` makes the FIRST
`^CORE_CLASSES="` line authoritative, and `run.sh` has a second one
(`CORE_CLASSES="$PRUNED"`). Anything inserted before the real list must not
begin at column 0 with that token.

---

## 8. NOMINATIONS

**N1 — the `fmt` job should split its own streams, and I did not do it because
it is not mine to weaken.** §3b's mechanism means the existing blocking `fmt`
job could report both verdicts from one rustfmt invocation: stderr → parse
(fatal, always), stdout → formatting (its current, blocking, semantics). Making
the formatting half non-blocking is a real decision about a gate every lane
sees, with a real argument on both sides, and a lane that cannot build should
not take it unilaterally. The measurement that decides it is in this record:
**605 of 1023 tracked `.rs` files print a formatting diff today**, so the
formatting half is red for the majority of touched files and `ci-fmt-step-blocks-every-later-gate`
is the standing lesson about what that costs.

**N2 — `RJitMapTierDiff` has no `--nojit` twin scheduled, and the two-tier
design is only half-verified without one.** `RJitMultiArrayClass`'s header says
"run it BOTH ways" and nothing schedules the second way for either vector. A
`class_cv_args` arm (in `harness-guard.sh`, not a copy — see `run.sh`'s note)
would make it one command. The obstacle is that a `--nojit` twin needs its own
registration and a name, i.e. the `RClassUnloadSweep`/`RClassUnloadSweepGen`
pattern, and `harness-guard.sh` is not this lane's file.

**N3 — measure whether the collection ladders bind at all on this corpus, and
say the answer out loud.** `H7-1` §6c predicts `collection_direct_helper_sites()
== (0,0,0)` under `--jdk-only` and `> 0` for `RMapResizeGc` under `--real-jdk`,
and warns that a zero there would mean *nothing in the corpus exercises any of
the four direct helpers*. `RJitMapTierDiff` is now the corpus's best candidate
for a non-zero. One strict arm and one compatible arm settle it, and the answer
determines whether §1's vector is coverage or a well-formed no-op. **Until it is
run, this vector's reach is asserted from bytecode shape and threshold
arithmetic, not measured** — that is the honest boundary of §1.

**N4 — take the first adjudicating blast-radius baseline, and take it on the
host that will keep taking it.** The gate is inert until then (exit 2 on every
run). Seven sequential strict arms; the workflow's `timeout-minutes: 180` is a
guess by a lane that has never timed one, and the first real run should correct
it.

**N5 — `H0-4` N2 is now one command.** `--prefixes
'java/util/HashSet+java/util/Hashtable'` arms two prefixes in one arm. It is the
first real test of whether the migration can proceed family-by-family at all,
and §2d's caveat — the cells do not sum — is exactly the question it answers.
