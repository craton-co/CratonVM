# The `fmt` CI job is red on roughly half of all pushes, and it is not the pushes' fault — OPEN 2026-09-06

**Status:** the SYMPTOM is fixed (§5 option 4 landed the same day — the job now
asks about the lines the diff touched); the DEBT is still open and §1's census
still describes it. §4 says why paying it down is not this record's to do.
**Gate:** `.github/workflows/ci.yml`, job `fmt` ("Formatting (changed files)").

---

## 1. The measurement

Every tracked `.rs` file outside `native-builtins/vendor/`, checked with the
same command the job runs (`rustfmt --check --edition 2021`):

| | |
|---|---|
| tracked `.rs` files | **978** |
| not `rustfmt`-clean | **171** (17%) |
| total hunks | **1,883** |

And the number that matters, because the job checks *changed files* rather than
the whole tree:

> Of the last 60 non-merge commits on `dev`, **33 touch at least one of those
> 171 files.**

So the job fails on about **55% of pushes**, for formatting that the push did
not introduce and that its author usually cannot see without running `rustfmt`
by hand on a file they only edited three lines of.

## 2. Where the debt is

Per crate, by file count:

| crate | dirty files |
|---|---|
| `vm` | 44 |
| `native-builtins` | 31 |
| `jit` | 26 |
| `gc` | 24 |
| `types` | 8 |
| `cuda-bridge` | 8 |
| `jit-cuda` | 6 |
| `classloading` | 6 |
| `native-api` | 4 |
| `native-io` | 3 |
| `vm-cli`, `native-collections` | 2 each |

The worst files are also the most-edited ones in the repository, which is the
whole reason the hit rate is 55% rather than 17%:

```text
258  gc/src/lib.rs
177  vm/src/lib.rs
137  gc/src/zgc.rs
124  vm/src/runtime/mod.rs
119  jit/src/lib.rs
102  native-builtins/src/lib.rs
 81  vm/src/runtime/interpreter.rs
 66  gc/src/g1.rs
 41  jit/src/x64.rs
 32  vm/src/jit/mod.rs
```

## 3. What it costs, concretely

`rustfmt --check` exits 1 for two unrelated reasons — a formatting difference
and a parse failure — and the job reads only the exit code. `ci.yml` says so
itself, which is why a separate parse job exists beside it. A red `fmt` on
inherited debt therefore trains readers to ignore the one signal that also
means "your file no longer parses".

It also blocks every later step for that push, so the practical effect is that
a lane touching `interpreter.rs` gets no CI at all until it either reformats a
file it did not write or someone re-runs past the failure.

Observed twice in one session (2026-09-05/06) by one lane, on two unrelated
changes, which is what prompted the census.

## 4. Why this is NOT fixed here

`rustfmt`-ing the 171 files is a 1,883-hunk diff across the four busiest crates.
On 2026-09-06 there were **five lanes building in their own worktrees at the
same time** (`cvm-psl4`, `cvm-vtpub`, `wt-sfr`, `cvm-devh2`, and this one), all
pushing to the same `dev`. A reformat of `vm/src/lib.rs`, `gc/src/lib.rs`,
`jit/src/lib.rs` and `native-builtins/src/lib.rs` would put a conflict in front
of every one of them, for zero behavioural gain, at a moment nobody asked for
it. That is a scheduling decision, not a technical one, and it belongs to
whoever owns the branch rather than to a lane that happened to trip over it.

Two further hazards a bulk run has to answer for, both already recorded in this
repository's own history:

* **source-witness tests read the working tree and key on literal lines.**
  `vm/src/vm/vm_init.rs`'s F30 witness matches whole trimmed lines
  (`if config.use_synthetic_jdk {`, the closing `"Real JDK mode: …"` tracing
  line); `rustfmt` moving either is a red gate whose message points at the
  wrong thing.
* **`cargo fmt --all` is wider than it looks** — it reformats a file's entire
  `mod` tree, so a run "on one crate" reaches files the author never opened.

## 5. What to do instead, in increasing order of commitment

1. **Per-lane, today:** before pushing, run
   `rustfmt --check --edition 2021 $(git diff --name-only origin/dev...HEAD -- '*.rs')`
   and compare the count against the same files at `origin/dev`. Landing at or
   below the baseline is achievable in minutes and is what the two 2026-09-05/06
   changes did; it does not make the job green, but it keeps the debt from
   growing and makes the red provably not yours.
2. **One crate at a time, on a quiet day:** `gc` (24 files, 485 hunks in its
   three biggest) is the best first target — it is the least cross-cut by other
   lanes and its dirty files are concentrated.
3. **The whole 171**, once, with the full workspace suite run either side. That
   is the only version that makes the job mean something again, and it needs a
   window with no lanes in flight.

A fourth option: have the job check only the **hunks the diff touched** rather
than whole changed files. That makes the gate say what its name says and needs
no tree-wide edit.

**This one landed, 2026-09-06.** `tools/check_changed_rust_formatting.py` runs
the same `rustfmt --check --edition 2021` over the same changed files and keeps
only the hunks overlapping lines the diff touched; `ci.yml`'s `fmt` job calls
it, and is now named "Formatting (changed lines)".

Two `rustfmt` behaviours make it a script rather than three lines of `bash`:

* **it follows `mod` declarations.** `rustfmt --check vm/src/runtime/
  interpreter.rs` reports hunks in `vm/src/runtime/interpreter/
  dispatch_static.rs`. A hunk is therefore attributed by the path in its OWN
  header, never by the file that was passed in — and it also means §1's
  per-file counts are per-*module-tree* counts, so 1,883 is an upper bound on
  distinct hunks rather than an exact one. The file count of 171 is exact.
* **its non-zero exit has two meanings**, formatting and a parse failure, which
  `ci.yml` already says is why a separate parse job exists. The script reports a
  parse failure separately and always fails on it.

Inherited hunks are counted and printed on every run, so the debt above stays
visible; it just stops being charged to whoever touched the file next.

Measured against four real `dev` commits — the old check, then the new one:

| commit | old | new | hunks it attributed |
|---|---|---|---|
| `9047a9945` | FAIL | FAIL | 4 of 426 |
| `70c486744` | FAIL | **pass** | 0 of 22 |
| `045413437` | FAIL | FAIL | 1 of 135 |
| `7f21336d5` | FAIL | **pass** | 0 of 67 |

Two of four stop being red for other people's formatting; the other two stay red
and now point at one and four hunks instead of 135 and 426. `045413437`'s single
hunk is a `pub mod exec_sampler;` the commit itself added out of alphabetical
order — a real defect the old output buried under 134 inherited ones.

Falsified as well as confirmed, because a filter that passes everything would
also have produced the two passes above: a deliberately misformatted line that
the diff adds FAILS; a file that no longer parses FAILS, reported as a parse
error rather than a formatting one; a diff with no `.rs` passes; and a correctly
formatted addition to `vm/src/runtime/interpreter.rs` (81 inherited hunks)
passes while printing all 81.

## 6. Reproducing the numbers

```bash
for f in $(git ls-files '*.rs' | grep -v '^native-builtins/vendor/'); do
  n=$(rustfmt --edition 2021 --check "$f" 2>/dev/null | grep -c '^Diff in')
  [ "$n" -gt 0 ] && echo "$n $f"
done | sort -rn
```

and, for the hit rate:

```bash
cut -d' ' -f2- dirty.txt | sort > /tmp/dirty.txt
for c in $(git rev-list --no-merges -60 origin/dev); do
  git diff-tree --no-commit-id --name-only -r "$c" | grep -qxFf /tmp/dirty.txt && echo "$c"
done | wc -l
```

Both take about seven minutes on Azure host 2 and need no build.
