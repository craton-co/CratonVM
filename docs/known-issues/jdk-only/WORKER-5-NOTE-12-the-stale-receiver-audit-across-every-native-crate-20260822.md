# WORKER-5 NOTE 12 — the stale-receiver audit across all seven native crates, and the one the tree had already half-fixed

**Status: MEASURED. Three fixes, one gate wired into CI, eighteen findings
handed over.** Lane WORKER-5, 2026-08-22. Answers `WORKER-5-NOTE-11` N2.

> **§7 is a CORRECTION to this record's own headline.** §3 said
> "`native-collections` now has ZERO functions of the shape". That was true **at
> `--depth 1`, which is the only depth this record had measured** — and §5 had
> flagged depth as a limit in the same breath. At `--depth 2` the crate had two
> more. Both are now fixed, the baseline has MOVED to depth 2, and the gate is a
> CI job.

`WORKER-5-NOTE-10` fixed `tm_sync_native_state` and `WORKER-5-NOTE-11` audited
`native-collections` by hand. N2 asked for the other six crates. This is that
sweep, and it is a **committed gate** rather than a one-off list:
`scripts/stale-receiver-audit.py`.

---

## 1. The shape

1. takes a receiver `ObjectRef` **by value**;
2. can **allocate** (so a moving collector may relocate it);
3. returns `()` / `Result<(), _>` — it **cannot hand the refreshed reference
   back**.

(3) is load-bearing. A funnel returning `ObjectRef` is safe by construction: the
caller cannot ignore the new value without ignoring the result. That is why both
fixes so far were signature changes to `&mut ObjectRef` — it makes every
unconverted caller a **compile error** rather than something a grep must find
again.

**A match is not a defect.** The defect needs a call site that REUSES the
receiver afterwards, and that is what the gate counts.

## 2. The sweep — AT DEPTH 1; §7 re-runs it at depth 2 and supersedes these numbers

```text
files 253   fns 16,768   allocating 5,147   matching the shape 131
WITH call sites that reuse the receiver:  17 fn(s), 44 site(s)
```

| crate | fns with reusing sites |
|---|---:|
| `native-builtins` | 14 |
| `native-io` | 2 |
| `native-awt` | 1 |
| **`native-collections`** | **0** at this depth — but see §7, depth 2 found 2 more |
| `native-api`, `-crypto`, `-security` | 0 |

The largest are `capture_inheritable_tl_at_construction` (10 sites),
`p54_huc_do_request` (6) and `ws_require_open` (4). Two were verified by hand
rather than trusted to the tool:

* **`capture_inheritable_tl_at_construction`** reaches allocation through
  `snapshot_inheritable_tl_entries`, whose own comment says pass 3 calls
  `childValue`, *"which is arbitrary application bytecode"*. Its callers then do
  `ctx.object_num_fields(this)`. Real shape.
* **`p54_huc_do_request`** performs an HTTP exchange; its callers then do
  `ctx.get_field(this, 6)`. Real shape.

**They are NOT fixed here.** `native-builtins`, `native-io` and `native-awt` are
other lanes' surfaces, there is no reproduction for any of them, and a
44-site migration across 700k lines would be unreviewable. They are handed over
with the gate that found them.

## 3. The one that WAS fixed, because the tree had already half-fixed it

`tm_migrate_fast_to_array` (`native-collections`) is the exception, and it is
worth the space:

* it **already knew** its receiver moves — it declares `mut this` and reassigns
  it from `tm_install_backing_array`'s refreshed value;
* taking the receiver **by value threw that refresh away at the return**;
* of its two call sites, **one had been protected by hand** with the reason
  written out —

  > *"The migration allocates (and can move this map); pin and refresh the
  > receiver across it."*

  — and **the other had not**, going straight on to `tm_state(ctx, this)`.

That is the "correct helper exists and one call site uses it" shape this
directory already has a name for. The receiver is now `&mut`, the write-back
covers both exits including the early `return`, and the hand-written pin at the
protected site is retired because the funnel does it. Neither caller can forget
again.

`native-collections` now has **zero** functions of the shape with reusing call
sites **at depth 1**. §7 is the correction: at depth 2 it had two more, now also
fixed.

## 4. The gate

`scripts/stale-receiver-audit.py` — report, `--detail`, `--update`,
`--selftest`, `--depth`. Baseline at
`scripts/baselines/stale-receiver-sites.txt` — **the committed baseline is the
DEPTH-2 one, 18 fns / 45 sites** (§7), not the depth-1 figures above. It watches
the POPULATION; it does not rank it, because ranking needs reproductions and the
fixed instances disagree (the TreeMap one failed 100%, `resync_view_set` could
not be made to fail at all).

**Its failure path is exercised**, not assumed. A synthetic allocating funnel
plus a caller that reuses the receiver, appended to `native-io/src/lib.rs`:

```text
  WITH call sites that reuse the receiver: 19 fn(s), 46 site(s)
  TRIPPED: w5_audit_canary_funnel now has 1 reusing site(s) (baseline 0)   rc=1
  [restored]                                                              rc=0
```

`--selftest` needs no tree: six cases covering depth-0 detection, a
comment-only mention, the three caller shapes that are NOT stale uses (rebind,
pin re-read, prose), and a funnel that returns the ref.

### 4.1 The gate found a defect in the gate

The first run reported `seed_buffer_byte_order` as a **directly allocating**
candidate. It does not allocate: `ctx.alloc_object` appears in its **doc
comment**. Three of the first 21 candidates were prose. The detector now reads
CODE only, which is the same error this lane made in `check-probes.sh` (a
`grep` without `-a` reading `Binary file … matches` as a type name) and in its
own first NUL-equivalence proof. Stripping comments cut 21 candidates to 18 and
`allocating` from 5,323 to 5,147.

## 5. What this does NOT establish

* **No reproduction for any of the 18 remaining.** They match the shape; none
  is shown to produce a wrong answer. `resync_view_set` (NOTE-11) is the
  standing example of a matching shape that could not be made to fail.
* **Allocation reachability is bounded by `--depth` (2 in the baseline).** A
  funnel three helpers deep is still invisible, and this is MEASURED rather than
  theoretical: depth 2 found two real defects depth 1 missed (§7). The baseline
  is a FLOOR.
* **`ALLOC0` is a name list.** An allocator spelled a way it does not name is
  missed. It counts `ctx.invoke*` as allocating, which is right (interpreted
  Java allocates) but coarse.
* **The reuse window ends at the enclosing function**, found by the next `fn` at
  column 0. A nested `fn` or a macro body would end it early.
* **Only the FIRST post-call use is reported.** A site whose first use is
  harmless but whose second is a deref reads as a hit either way — the count is
  of SITES, not of proven derefs.
* **`native-collections` being at zero is about this shape, AT DEPTH 2, only.**
  It said "zero" at depth 1 too, and depth 2 found two more (§7). It says nothing
  about receivers passed to functions that already take `&mut`, or about the
  other GC hazards this directory records.
* **Candidates are keyed by function NAME** (§7.1); 66 names are multiply
  defined and those rows are marked `AMBIG`.

## 7. CORRECTION — "zero" was depth-1, and depth 2 found two more

§5 listed the depth bound as a limitation and then §3 asserted a bare "zero"
anyway. Running the sweep the way §5 said it should be run:

| | depth 1 | depth 2 |
|---|---|---|
| allocating | 5,147 | 6,104 |
| matching the shape | 131 | 146 |
| **with reusing call sites** | **17 fn / 44 sites** | **20 fn / 49 sites** |

The three extra are `chm_refresh_real_table` (3 sites),
`materialize_lazy_stream` (1) — **both `native-collections`** — and
`populate_format_data_en` (1, `native-builtins`).

`chm_refresh_real_table` is the interesting one and is the same species as
`tm_migrate_fast_to_array`: `chm_publish_real_table` pins `this` for its whole
body, **releases the pin before returning, and never tells the caller**, while
the body allocates (its own comment: *"Class resolution can LOAD a class, which
allocates"*). All three call sites then dereference immediately —
`make_key_set_view`, `chm_collect_all_values`, `chm_collect_all_entries`.

Both `native-collections` entries are fixed with the same `&mut` pattern.
**At depth 2 the crate is now genuinely at zero** — 18 fn / 45 sites remain, all
in `native-builtins` (16), `native-io` (1) and `native-awt` (1) — and the
committed baseline is the depth-2 one.

### 7.1 A third limitation, found and now surfaced

Chasing why `native-io` moved from 2 rows to 1 between depths — impossible for a
superset — exposed that **candidates are keyed by function NAME**, because this
scan has no module resolution. **66 of the tree's 16,347 native fn names are
defined in more than one crate**, so those rows merge and the crate label is
whichever definition was indexed last.

Rather than bury that, the gate now prints `AMBIG(n defs)` on such a row. Of the
18 in the baseline exactly one is ambiguous (`populate_format_data_en`).

## 8. The gate is a CI job

`.github/workflows/stale-receiver-audit.yml`, modelled on the untyped-alloc
ratchet: green is the normal state, red means the population grew, no build and
no JDK. It runs `--selftest` FIRST — check the judge before it judges the tree —
and then the depth-2 audit.

## 6. NOMINATIONS

* **N1 — the 14 `native-builtins` sites want their owner.** Start with
  `capture_inheritable_tl_at_construction` (10) and `p54_huc_do_request` (6);
  §2 verified both reach allocation. `scripts/stale-receiver-audit.py --detail`
  prints every site with the line that decides it.
* **N2 — 2 in `native-io`** (`ws_require_open`) **and 1 in `native-awt`**
  (`sync_raster_pixels`).
* ~~**N3 — run the gate at `--depth 2`.**~~ **DONE — §7.** It found three more,
  two of them in the crate this record had just called clean. The baseline is
  now depth 2.
* ~~**N4 — wire the gate into CI.**~~ **DONE — §8.**
* **N5 — depth 3 is still unmeasured.** Depth 2 found two real defects that
  depth 1 could not see, so "the baseline is a floor" is now a measured claim
  rather than a caveat. Someone should find where it converges.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-12` — the stale-receiver shape swept across **all seven native
  crates**: 16,768 fns, 131 match the shape, **17 have call sites that reuse the
  receiver** (44 sites — 14 `native-builtins`, 2 `native-io`, 1 `native-awt`),
  handed over unfixed with a reproduction-free warning. **One fixed:**
  `tm_migrate_fast_to_array`, which already reassigned its own `this` and threw
  the refresh away at the return, and whose two call sites were split one
  protected / one not. `native-collections` is now at zero. New gate
  `scripts/stale-receiver-audit.py` + baseline, failure path EXERCISED, and its
  own first run reported three candidates that were `ctx.alloc_object` in a DOC
  COMMENT. MEASURED.
