# WORKER-5 NOTE 11 — the N4 audit: of seventeen candidate funnels, ONE still had the shape, and the JMX failure that showed up alongside it is somebody else's

**Status: MEASURED.** Lane WORKER-5, 2026-08-22. Answers `WORKER-5-NOTE-10` N4.

`WORKER-5-NOTE-10` §7 fixed `tm_sync_native_state`: a funnel that ALLOCATES,
takes its receiver BY VALUE, and returns `()`, so its callers keep using an
address a moving collector may already have invalidated. N4 asked whether any
other funnel has that shape. This is that audit.

---

## 1. The shape, stated so it can be searched for

Three properties, and the third is the load-bearing one:

1. takes a receiver `ObjectRef` **by value**;
2. can **allocate** (so a moving collector may relocate that receiver);
3. returns `()` or `Result<(), _>` — so it **cannot hand the refreshed
   reference back**, and every caller is left holding a possibly-stale address.

A funnel that returns `ObjectRef` is already safe by construction: the caller
cannot ignore the new value without ignoring the result.

## 2. The sweep

Seventeen functions in `native-collections/src/lib.rs` match (1)+(2)+(3).
Matching the shape is not the same as having the bug — the bug needs a caller
that **reuses the receiver after the call**. Five had such callers:

| funnel | call sites that reuse the receiver | verdict |
|---|---|---|
| **`resync_view_set`** | **8 of 10** | **THE SHAPE — fixed here** |
| `tm_publish_real_root` | 1 of 3 | already re-reads through a pin on the next line |
| `lhm_resize` | 1 of 1 | already re-reads through a pin on the next line |
| `pbq_seed_real_lock` | 1 of 1 | **false positive** — the "reuse" is a doc comment |
| `ts_publish_real_backing_map` | 1 of 1 | **false positive** — a doc comment |

And the two funnels most likely to have been siblings are already correct, which
is worth recording because it is why the audit is small:
`resync_ts_view` returns `Result<ObjectRef, _>` and **all twelve** callers write
`let this = …`; `resync_values_view` likewise, **all seven**.

## 3. `resync_view_set`, and why it is fixed anyway

It allocates (`alloc_ref_array` for the bucket table, `try_alloc_synthetic` per
entry) and its ten callers are the `native_hs_*` family, eight of which go
straight on to `hs_backing_map(ctx, this)` — the same side-table lookup, keyed
the same way, that produced `size() == 0` for TreeMap. Two
(`native_hs_iterator`, `native_hs_remove_if`) already re-read through a pin and
were never exposed.

**It is LATENT: no reproduction.** A `HashMap` keySet and entrySet, `size()`d
five times each under the same `--Xmx 64m --nojit` pressure that makes the
TreeMap case fail 100%, answered `400` every time across twelve runs — the
early returns above the allocating path usually fire first. A `TreeMap` keySet
does not route here at all.

It is fixed regardless, and the reasoning is worth being explicit about:

* it is the **same shape** that just cost this lane a day, in the same file;
* the fix is mechanical and the pattern is now proven;
* `&mut` makes the eight sites **compile errors**, so the audit does not have to
  be repeated by grep next time.

The wrapper pins, calls `resync_view_set_inner`, and writes the refreshed
receiver back. rustc reported 11 errors, then 10 mutability suggestions applied
from its own JSON; grep confirms zero old-shape call sites remain.

## 4. MEASURED — no regression

Three arms on a binary carrying the fix (`cratonvm-w5n4.exe`):

```text
  SUITE=all CRATONVM_ARGS=--jdk-only    106 / 107      RJdkJmx  (see §5)
  SUITE=all                             107 / 107
  SUITE=core                             67 /  67
```

`RTreeRangeGc` stays fixed.

## 5. `RJdkJmx` fails, and it is NOT this change — controlled, not argued

`RJdkJmx` passed on the previous binary and fails on this one, which is exactly
the shape of a regression this lane would have caused. It is not:

```text
  cratonvm-w5ctl   (this change REVERTED, same tree)    RJdkJmx FAIL
  cratonvm-w5n4    (this change IN)                     RJdkJmx FAIL
```

**Same tree, same commit, the only difference the change under test — and both
fail.** The control binary was built specifically for this question rather than
inferring it from the diff.

What actually changed: the merge that preceded this work brought
`native-builtins/src/jmx.rs` among 27 Rust files, and the earlier binary
predates it. The failure is `rc=1: unclassified: CK RJdkJmx
objectName=cratonvm.test:name=alpha,type=Counter` — the vector prints its first
CK line and then dies, so it is a real defect and not a harness flag.

**Not this lane's surface, and not adopted here.** It is filed for whoever owns
`jmx.rs`; the reproduction is `ONLY=RJdkJmx` under `--jdk-only`, deterministic
on both binaries.

## 6. What this audit does NOT establish

* **It is scoped to `native-collections/src/lib.rs`.** The same shape can exist
  in `native-builtins`, `native-io` and the rest; nothing here looked.
* **(2) is decided by a regex over the body** (`alloc_*`, `ctx.invoke`,
  `tm_collect_pairs`, …). A funnel that allocates through a helper the regex
  does not name would be missed, and a funnel whose allocating branch is
  unreachable is a false positive — which is why §2's five candidates were each
  read by hand rather than counted.
* **`resync_view_set`'s fix is unverified against a failing case**, because
  there is no failing case (§3). What IS verified is that it changes nothing:
  107/107 and 67/67.
* **The other four candidates were judged by reading the next line.** Each was
  found already-pinned or a comment; none was measured.

## 7. NOMINATIONS

* **N1 — `RJdkJmx` needs the `jmx.rs` owner** (§5). Control evidence included.
* ~~**N2 — extend the audit to the other native crates.**~~ **DONE —
  `WORKER-5-NOTE-12`.** All seven crates: 131 match the shape, **17 have
  reusing call sites** (14 `native-builtins`, 2 `native-io`, 1 `native-awt`),
  handed over unfixed. One more was fixed —
  `tm_migrate_fast_to_array` — and the script IS committed now, as
  `scripts/stale-receiver-audit.py` with a baseline and an exercised failure
  path.
* **N3 — prefer `&mut ObjectRef` (or a returned `ObjectRef`) for any funnel that
  allocates.** Three of the four correct funnels found here already do, and the
  one that did not is the one that had the bug. That is a convention worth
  stating in the crate's module docs.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-11` — the `NOTE-10` N4 audit. Of **seventeen** funnels in
  `native-collections` matching "allocates + receiver by value + cannot return
  it", **five** have callers that reuse the receiver and **one** —
  `resync_view_set`, 8 of 10 sites — still had the bug; the other four are
  already pinned or doc-comment false positives, and `resync_ts_view` /
  `resync_values_view` were already correct at all 19 sites. Fixed with the
  same compiler-enforced `&mut` pattern, **LATENT — no reproduction**, no
  regression (107/107, 67/67). Also: **`RJdkJmx` fails and it is NOT this
  change** — a control binary with the change reverted fails identically; it
  arrived with another lane's `jmx.rs`. MEASURED.
