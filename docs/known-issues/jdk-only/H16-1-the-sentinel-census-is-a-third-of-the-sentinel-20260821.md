# H16-1 — the untyped-allocation census counts 84 of 230 sites, and the one site the whole wave is about is in the 146 it cannot see

**Status: OPEN — MEASURED.** `git grep` over the worktree at `3a6cc90fd`
(`claude/jdk-only-mode-handoff-09b48c` after this lane's fast-forward). No
source change and no build for anything in this record. The blocking ratchet
was executed as shipped; nothing else was run.

Lane H16, 2026-08-21. Corrects `H0-6` §3 and bounds the gate `H10` built from
it.

---

## 1. The finding, in one line

`H0-6` §3 measured the untyped-allocation sentinel at **84 production sites**
and `H10` turned that number into a blocking CI ratchet
(`scripts/untyped-alloc-ratchet.sh`, `.github/workflows/`). Both count only the
spelling `alloc_object(ClassId::new(0), <literal>)`. **There are 230.**

## 2. MEASURED — the four populations

Same crate set and the same test-path exclusions the shipped ratchet uses
(`native-builtins/src native-collections/src native-io/src native-api/src
native-builtins-crypto/src native-builtins-security/src native-awt/src`, minus
`*/tests/*`, `*test_*.rs`, `*_test.rs`, `*/test_utils.rs`, `*/test_mock.rs`).

| # | spelling | sites |
|---|---|---:|
| A | `alloc_object(ClassId::new(0), <LITERAL>)` — **the ratchet's `PAT`** | **84** |
| B | `alloc_object(ClassId::new(0), <any width expr>)` | 121 |
| C | `alloc_object(cratonvm_types::ClassId::new(0), <any width expr>)` | **109** |
| D | of C, those whose width IS a literal | 89 |
| | **B + C — every production sentinel site** | **230** |

The shipped ratchet, run as-is on this tree, reports:

```text
UNTYPED-ALLOC RATCHET
  sites  : 84 (baseline 84)
  widths : [1 2 3 4 5 6 8 12] (baseline [1 2 3 4 5 6 8 12])
  ok — no growth, no new widths.
```

**84 / 230 = 36.5 %.**

### 2a. Two independent blind spots, and only one of them is known

* **The non-literal width.** `PAT` ends `, *[1-9][0-9]*)`, so
  `alloc_object(ClassId::new(0), MAP_NUM_FIELDS)` does not match. The script's
  own header says so — *"a non-literal width is invisible to it"* — and that is
  37 sites (B − A).
* **The fully-qualified path.** `alloc_object(cratonvm_types::ClassId::new(0), …)`
  is the same call, written with the `cratonvm_types::` prefix. **This one is
  not mentioned anywhere in the script, in `H0-6`, or in the baseline file**,
  and it is 109 sites — larger than the entire counted population. 89 of them
  carry a literal width, so they are invisible for the prefix alone.

`H0-6` §3a is careful about a different discrepancy (its 49 vs an in-tree
comment's 30) and explicitly says *"I checked this specifically because
asserting the comment was wrong was the cheaper and more satisfying move."*
That care went to the number it doubted. The spelling it was counting was never
doubted, because a grep that returns results looks like a grep that works.

## 3. MEASURED — the producer ranking is wrong, and the top entry is not on anyone's list

`H0-6` §4 names `native-builtins/src/util_concurrent_ext.rs` as *"the largest
single producer … 24 of 84"* and nominates it (N3) on that basis. Counting both
spellings:

| file | sites (B + C) |
|---|---:|
| `native-builtins/src/t27_tls.rs` | **35** |
| `native-builtins/src/util_concurrent_ext.rs` | 29 |
| `native-builtins/src/phases_early.rs` | 18 |
| `native-builtins/src/lang_class.rs` | 16 |
| `native-builtins/src/http_url_connection.rs` | 12 |
| `native-io/src/lib.rs` | 9 |
| `native-collections/src/lib.rs` | 9 |
| `native-builtins/src/lib.rs` | 9 |
| `native-builtins/src/classloader.rs` | 9 |
| `native-builtins/src/net_phase_e.rs` | 8 |
| `native-builtins/src/jca/provider_chain.rs` | 8 |
| … 28 more files at 1–7 | |

**`t27_tls.rs` is the largest producer and appears in no record, no P0 row and
no lane's brief.** All 35 of its sites are in the qualified spelling, which is
why it has never shown up: the census literally could not see the file.

`H0-6` N3 should be re-aimed. Its argument — *"before that file is migrated,
someone should say why N untyped allocations are correct there"* — is right and
was pointed at the second-largest.

## 4. MEASURED — the site the wave exists for is in the invisible half

`H0-6` §7 identified `AnonymousObject$4` as the `HashMap.Node` and `H0-4` §7
established that the node's class identity is the root of the 22-vector
`java/util/HashMap` blast radius. The single producer of those nodes is

```text
native-collections/src/lib.rs:11874   (at 3a6cc90fd, before this lane's change)
    let new_node = ctx.alloc_object(cratonvm_types::ClassId::new(0), NODE_NUM_FIELDS);
```

**Qualified spelling AND a non-literal width — invisible twice over.** So:

* `H0-6` §3's per-file table says `native-collections/src/lib.rs` holds *"four
  of eighty-four"* and that *"`native-collections` — the crate the whole
  migration plan is organised around — holds four of eighty-four."* The file
  holds **9** by the two-spelling count (3 production + 6 inside its own
  `#[cfg(test)] mod`), and the production three are lines 11874, 50440 and
  50910. **None of the four the census saw is the node line.**
* `H16-2` removes the node line for object-shaped mappings. **The ratchet will
  not move**, because it never counted it. A gate that cannot see the fix is a
  gate that cannot see the regression either.

This is the third instance in this directory of *the instrument reported health
because of how the defect is shaped* (`H1-1`'s capped sink, `H0-4`'s fraction
gate, `H0-6` §2's self-consistent substitution) — and this time the instrument
is the one built to catch that species.

## 5. What this does NOT establish

* **230 is a grep count, not a defect count.** The script's own caveat applies
  unchanged and more widely: a site behind `cfg`, a macro-generated call, or an
  indirection through a helper is invisible to all four patterns. 230 is a
  FLOOR.
* **It says nothing about the 2026-08-12 → 2026-08-20 growth claim.** `H0-6`
  §3's +71 % is one method applied to two revisions and is sound for what it
  compared. I did not re-run the qualified spelling against `684707e60`, so I
  do not know whether the invisible half grew, shrank or held. **NOT MEASURED.**
* **Not every sentinel site is a defect.** `vm_exec.rs:12997` records that the
  substitution is legitimate in both modes for VM bookkeeping types (`H0-6`
  §6). Some of the 230 are that. Which ones is not derivable from a grep.
* **I did not touch the script.** This lane owns `native-collections/src/lib.rs`
  and its own records. Every repair below is a nomination.

## 6. NOMINATIONS

* **N1 — widen `PAT` to both spellings and to a non-literal width, and
  re-baseline in the same commit.** One expression covers it:
  `alloc_object((cratonvm_types::)?ClassId::new(0),` with the width captured
  by a second `sed`. The WIDTHS column then needs a `<expr>` bucket for the
  non-literal ones rather than dropping them. Until this lands the gate's green
  is `84 / 230`, and it is a **blocking** job.
* **N2 — the baseline file must record the PATTERN, not only the count.**
  `scripts/baselines/untyped-alloc-sites.txt` carries `sites=` and `widths=`.
  A pattern change silently re-bases the meaning of both; storing the pattern
  makes that a visible diff. This is the specific mistake this record is about.
* **N3 — re-aim `H0-6` N3 at `native-builtins/src/t27_tls.rs` (35).** It is the
  largest producer and has never been looked at. `util_concurrent_ext.rs` (29)
  stays second and keeps its nomination.
* **N4 — `H0-6` §9 N6 asked for "alert on any growth in width-4 sites,
  separately".** Under the widened pattern the width-4 population is
  34 (unqualified, from `H0-6`) + 34 (qualified) + `NODE_NUM_FIELDS` +
  `LHM_NODE_NUM_FIELDS`-shaped names. **Width 4 is the `Map.Entry` shape**, and
  it is the one width whose sites can be triaged mechanically: each is either a
  node or a carrier handed to Java, and `H16-3` shows the second kind is a live
  wrong answer. Worth a column of its own.
* **N5 — three sites use width `0`.** `PAT`'s `[1-9]` excludes them by
  construction, and a zero-width untyped object is a shape nobody has
  described. Not investigated here.
