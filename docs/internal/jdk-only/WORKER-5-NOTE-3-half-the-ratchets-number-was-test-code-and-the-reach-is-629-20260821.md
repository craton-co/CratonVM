# WORKER-5 NOTE 3 — 197 of the untyped-alloc ratchet's 359 sites were `#[cfg(test)]`, and the reach it could not see is 629

**Status: FIXED, MEASURED.** Lane WORKER-5, 2026-08-21, at `22cb4338d`.
`scripts/untyped-alloc-ratchet.sh` v6 → v7. No VM change, no build.

---

## 1. The task, and what it turned into

The brief asked for one thing: v6's **LIMIT 7** says the gate counts DIRECT
SPELLINGS, not reach — `alloc_ref_array` is ONE counted site with 159 callers,
so routing a caller to a typed allocation removes a real fabrication and moves
the number by ZERO. *"Either give it call-graph awareness or state the limit
where people read the number, not only in the file header."*

Measuring that turned up a second defect that is larger.

## 2. MEASURED: 197 of the 359 counted sites — 54.9% — are inside a `#[cfg(test)] mod` of a PRODUCTION file

v6's `EXCL` is a git **PATHSPEC**:

```
:!*/tests/*  :!*test_*.rs  :!*_test.rs  :!*/test_utils.rs  :!*/test_mock.rs
```

It can drop a test FILE. It cannot see inside one. So a `#[cfg(test)] mod tests`
block in a production file counted as production fabrication surface.

The worked example is the one **v6's own header calls out**:

> `t27_tls.rs` is deliberately NOT excluded: production source, test-shaped
> name, and the single largest producer in the tree.

`t27_tls.rs` has 48 counted sites, and **35 of them are inside `mod tests`
(lines 8177–11577)**. Keeping the file and dropping its test module is what v6
meant to do and could not express.

| | v6 | v7 |
|---|---:|---:|
| objects | 208 | **33** |
| arrays | 151 | **129** |
| sites | 359 | **162** |
| test_sites (excluded, printed) | — | **197** |

**The drop from 359 to 162 is a CORRECTION, not an improvement.** Nothing was
fixed. `test_sites=197` prints on every run and the baseline file says so in a
comment, so the excluded population never goes dark.

By file, the excluded 197: `t27_tls.rs` 35, `util_concurrent_ext.rs` 29,
`lang_class.rs` 17, `native-collections/lib.rs` 13, `http_url_connection.rs` 12,
`classloader.rs` 9, `jca/provider_chain.rs` 8, `net_phase_e.rs` 8, then 30+
files at 1–7.

### 2.1 The first attempt at this measurement was WRONG, which is why the rule is what it is

The first range detector counted `{`/`}` depth and **OVERRAN**: braces inside
Rust string literals are not braces, so `mod tests` in `t27_tls.rs` ran to EOF
instead of to 11577 and the analysis attributed 35 sites to a block that holds
none of them — while, by coincidence, reporting a similar total (179 vs 197).

A top-level `mod` ends at the next **COLUMN-0 `}`**, which no string literal can
fake. Verified against `t27_tls.rs`'s four `#[cfg(test)]` blocks — `(103,115)`,
`(7939,8174)`, `(8177,11577)`, `(15150,15179)` — and the site at 19190, which
LOOKS like it should be in `mod tests` under the depth model and is production
code.

*A probe's setup is code that can be wrong.* The over-count and the correct
count differed by 18 and neither was obviously wrong from the total alone.

### 2.2 DISCOVERY still runs inside test blocks, deliberately

`byfn` and `widths` are the gate's fail-safe half: a new spelling or a new
carrier width is a new SHAPE wherever it appears, and v1/v2 were wrong precisely
because a spelling they had never seen was silently absent. So they are computed
over ALL sentinel lines, test blocks included.

That also keeps both fields **byte-identical to the v6 baseline**:

```
widths=1 2 3 4 5 6 7 8 12
byfn=alloc_object=228 new_ref_array=147 define_class=6 try_new_ref_array=4 class_is=1 alloc_object_of=1
```

so this change moves only the columns it claims to move. (Worth knowing:
`define_class`, `class_is` and `alloc_object_of` — the three spellings v6
discovered but did not count — occur **only** in `#[cfg(test)]` blocks today.
A production-only discovery would have silently dropped all three.)

## 3. `reach` — the answer to LIMIT 7

```
reach = SUM over counted production sites of max(1, callers(enclosing fn))
      = 629   against 162 direct sites at 22cb4338d
```

The enclosing `fn` of each counted site is attributed by a per-file scan, then
one alternation `git grep` counts call sites of those names and one more
subtracts their definitions. A site in a function nothing calls counts once; a
site in a helper counts once per caller. **Re-routing a caller now moves a
number.**

The top of the helper table, MEASURED:

| callers | defs | sites | fn |
|---:|---:|---:|---|
| **157** | 1 | 1 | `alloc_ref_array` |
| 30 | 1 | 2 | `alloc_wrapper` |
| 29 | 2 | 2 | `alloc_time_synthetic` |
| 12 | 1 | 1 | `alloc_path` |
| 10 | 1 | 1 | `build_object_stream_class` |
| 9 | 2 | 2 | `alloc_obj` |

`alloc_ref_array` is the LIMIT-7 example and it measures at 157 in-tree callers
here (the brief said 159; different tip, same shape). 115 distinct enclosing
functions hold the 162 sites.

## 4. The limits are now PRINTED WITH THE NUMBERS

This is the half of the brief that is about where people read things. CI logs
are where the number is read; a file header is not. Every run prints:

```
    L1 reach follows ONE level of call graph. Both columns are FLOORS.
    L2 attribution is by function NAME; 3 name(s) have >1 definition.
    L3 grep over source text — a cfg-gated or macro-generated site is invisible.
       CRATONVM_DBG_ANONALLOC=1 is the runtime instrument; neither alone is
       authoritative (H0-6 §8 measured it attributing 4.4% of events).
    L4 this ratchets DRIFT. It is not the size of the problem.
```

L2's count is computed per run, not written down: today
`alloc_time_synthetic`, `alloc_obj` and `register_https_session_accessors` each
have two definitions, so their caller counts are pooled and the figure is not
exact. Saying so costs one line.

## 5. `--selftest` now makes the gate FAIL, on all nine paths

v6's selftest checked only that the patterns MATCHED something. The gate's own
history is nine versions of confident numbers, including **v3, which printed
`IMPROVED by 84 … ok` with rc=0 while matching NOTHING**. So:

```text
  ok   object growth trips (rc=1)      ok   improvement reported (rc=0)
  ok   array growth trips (rc=1)       ok   matching baseline ok (rc=0)
  ok   reach growth trips (rc=1)       ok   missing baseline (rc=2)
  ok   new width trips (rc=1)          ok   zero-match guard (rc=3)
  ok   new spelling trips (rc=1)
  selftest OK — all nine paths fire
```

Two env hooks exist for it (`RATCHET_BASELINE`, `RATCHET_CRATES`); the zero-match
case is driven by pointing `RATCHET_CRATES` at a directory with no sentinel, and
it reproduces v3's exact failure mode as a PASS of the guard.

The selftest also asserts `test_sites > 0` and `reach >= sites`: if the
`cfg(test)` block filter stops matching, the count silently returns to the v6
over-count, and a zero there is the tell.

## 6. Performance, because it gated the design

The awk pass reads ~527,000 lines. With every regex applied unguarded it took
**11.6 s per run**, and a nine-run selftest could not finish inside a two-minute
budget. Putting every regex behind a literal `index()` guard —
`index($0,"ClassId::new(0)")`, `index($0,"fn ")`, a first-character test for the
`#[cfg(test)]`/`}` state machine — brings it to **3.3 s with byte-identical
output**.

## 7. What this does NOT establish

* **`reach` is ONE level.** A caller of a caller of `alloc_ref_array` is still
  invisible. Both columns remain FLOORS and the run says so.
* **Attribution is by function NAME**, so a name defined twice pools its
  callers. Three names do today.
* **It is still a grep over source text.** A `cfg`-gated or macro-generated
  site is invisible, and `CRATONVM_DBG_ANONALLOC=1` (the runtime instrument)
  attributed only 4.4% of events in `H0-6` §8. Neither is authoritative alone,
  and this record does not claim the two now agree — they were not compared.
* **`reach` is not a count of defects.** It weights a site by how many callers
  could reach it, which is an upper bound on blast radius, not a measurement of
  how many of those callers actually fabricate at runtime.
* **The `cfg(test)` exclusion was not audited row by row.** 197 sites were
  classified by the block scanner and spot-checked at `t27_tls.rs:10104`
  (a test fixture building a session carrier) and `19190` (production). A
  systematic audit was not done.

## 8. NOMINATIONS

* **N1 — re-read any number quoted from this gate before 2026-08-21.** Every
  `objects`/`arrays`/`sites` figure in the record set is roughly 2.2× the
  production surface. `H0-6`'s 49→84 growth series and `H23-3`'s array column
  are both in that population.
* **N2 — the second level of reach is worth one more grep.** `alloc_ref_array`'s
  157 callers are themselves functions with callers. The same two-grep trick
  applied twice would give a level-2 number; it was not done here because the
  ambiguity of name-based attribution compounds and L2 would need to say so.
* **N3 — `#[cfg(test)]` blocks in production files are a general blind spot**,
  not one this gate invented. Any pathspec-based census in `scripts/` has the
  same hole. The block scanner in v7 is ~12 lines of awk and is reusable.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-3` — untyped-alloc ratchet v7. **197 of v6's 359 counted sites
  (54.9%) were inside `#[cfg(test)] mod` blocks of production files**; the
  production surface is 162, and the drop is a correction, not a fix. New
  `reach` column answers v6's LIMIT 7 at **629** (`alloc_ref_array`: one site,
  157 callers). Limits now print with the numbers; `--selftest` fires all nine
  failure paths. MEASURED.
