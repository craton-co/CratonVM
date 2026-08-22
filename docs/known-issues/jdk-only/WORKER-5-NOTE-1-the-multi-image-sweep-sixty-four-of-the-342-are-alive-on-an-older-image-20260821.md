# WORKER-5 NOTE 1 — the method-granular multi-image sweep: 64 of `H25-1`'s 342 are ALIVE on an older image, and 110 more rows are dead on 17/21 but alive on 25

**Status: MEASURED.** Lane WORKER-5, 2026-08-21. Nine JDK images, one
strict-mode registry census from `C:/craton/cratonvm-r11.exe`. No source change
to the VM; two new scripts and one committed data file.

> **CORRECTED 2026-08-22 — 42 rows this record first called retirable are
> NOT.** WORKER 3 implemented the same sweep independently (`409f5f630`) and
> disagreed. The cause is in §2.5, the finding is that **an index built from the
> method table alone cannot see a registration whose name is a FIELD**, and the
> committed TSV has been regenerated with a fifth verdict. The first version of
> this record, and its TSV, would have nominated those 42 for deletion.
>
> **Re-verified after merging H0's `dab993033`** (which brought `ecd4f56e1`, the
> `java/util/Comparator` guard, into `native-collections/src/lib.rs`). The whole
> sweep was re-run against an `r11` census: **10,378 registrations, 999 / 342 /
> 9,037, and verdicts 8812 / 386 / 305 / 875 — identical** (those are the
> pre-correction verdicts; §2.5 then split 42 of the 305 out as
> `field-shaped`), canary still
> `cross-version`. The committed TSV differs from the `r10` one **only in
> `registered_by` line numbers**, which `ecd4f56e1` shifted by adding 36 lines;
> stripping the line numbers makes the two byte-identical, 692 rows each. The
> committed file is the `r11` one, so its line numbers point at the merged tree.

`H25-1` N1 asked for this and called it a **precondition, not a follow-up**:
until it ran, *no row in the 342 could be deleted by anyone.* It has run.

---

## 0. The one sentence that mattered

`H25-1` measured **342 strict-mode registrations naming a method no JDK 25 image
declares anywhere on the receiver's hierarchy** — and then, in its own §1.6,
retracted the work list:

> `java/lang/StringUTF16.isBigEndian()Z` is one of the 342, and
> `lang_string.rs:12422` carries a 56-line comment whose heading is
> *"On JDK 25 this registration never fires, and that is not a defect"*, because
> **JDK 17/21 DO declare it.**

So 342 was a ONE-IMAGE UPPER BOUND. This host carried exactly one JDK, so
`H25-1` could not separate the deliberate rows from the dead ones.

## 1. What was run

Nine images, all Temurin, on the Azure host at `/data/jdkimages`. The JDK **17**
arm did not exist before today and was fetched for this sweep; the other six
were already there from the class-granular sweep.

| release | linux | windows | macOS |
|---|---|---|---|
| 17 | 17.0.20.1 | 17.0.20 | 17.0.20.1 |
| 21 | 21.0.12 | 21.0.12 | 21.0.12 |
| 25 | 25.0.4 | 25.0.4 | 25.0.4 |

Census: `--jdk-only --dump-native-registry --explain-jdk-only`, schema 5,
**10,378 registrations**, `image_adjudication: true`. Taken on `cratonvm-r10.exe`
and re-taken identically on `cratonvm-r11.exe` after the merge (see the banner).

**The census reproduces `H25-1` exactly** on a newer binary — 999
`no-image-class` / **342** method-dead / 9,037 other, **314** of the 342 owning
their slot, 342/342 at `invocations: 0`, **102** distinct receiver classes, and
a byte-identical by-file table (30 `lib.rs`, 29 `shared_secrets_bridge.rs`,
22 `native-io/lib.rs`, …). That is the control for everything below: the
population is the same one `H25-1` measured, so the difference in verdict is the
IMAGES, not the binary.

## 2. The result

```text
                     ONE image (jdk25-win)      NINE images
  live                       —                      8812
  cross-version              —                       386   <- DO NOT DELETE
  field-shaped               —                        42   <- DO NOT DELETE (§2.5)
  dead-everywhere            —                       263   <- retirable
  no-image-class            999                       875
  "declared nowhere"        342                        —
```

`dead-everywhere` read **305** before §2.5's correction; 42 of those were
`field-shaped`.

Cross-tabbed, which is the table that answers `H25-1`:

| one image (jdk25-win) | nine images | rows |
|---|---|---:|
| declared-on-25 | live | 8729 |
| no-image-class | no-image-class | 875 |
| **declared-on-25** | **cross-version** | **308** |
| **dead-on-25** | **dead-everywhere** | **278** |
| no-image-class | live | 83 |
| **dead-on-25** | **cross-version** | **64** |
| no-image-class | dead-everywhere | 27 |
| no-image-class | cross-version | 14 |

### 2.1 The answer to N1

> **Of `H25-1`'s 342: 236 are dead as a method on all nine images, 64 are SAVED
> by an older image, and 42 are not methods at all.**

| of the 342 | rows | may it be retired? |
|---|---:|---|
| `dead-everywhere` / truly-gone | 162 | yes |
| `dead-everywhere` / near-miss | 74 | yes as a *method*, but see `H25-1` N3a |
| `cross-version` | **64** | **NO** — an older image declares it |
| `field-shaped` | **42** | **NO** — the name is a FIELD (§2.5) |

**31% of the population `H25-1` flagged must not be deleted** — 64 because of the
images, 42 because of the member kind. None of the 342 is `live` or
`no-image-class`.

Four of the six ★ rows `H25-1` §1.4 called "removed from the JDK, not
deprecated" are among the 64:

| triple | declared by |
|---|---|
| `java/lang/Thread.countStackFrames()I` | 17, 21 |
| `java/lang/Thread.resume0()V` | 17 |
| `java/lang/Thread.stop0(Ljava/lang/Object;)V` | 17 |
| `java/lang/Thread.suspend0()V` | 17 |
| `java/lang/StringUTF16.isBigEndian()Z` | 17, 21 ← the canary |

Only **`Thread.destroy()V`** and **`System.runFinalizersOnExit(Z)V`** of that
list are dead on all nine. `H25-1`'s sentence "removed from the JDK" is true of
JDK 25 and false of JDK 17 for four of the six.

The 64 by owning file: `lib.rs` 12, `security_manager.rs` 7,
`native-io/file_channel.rs` 7, `native-io/nio_native.rs` 7,
`unsafe_natives_ext.rs` 6, `deprecated_lang.rs` 4, `shared_secrets_bridge.rs` 4,
then 12 files at 1–2. **Every lane with a file in that list is affected.**

### 2.2 A second block nobody had asked about

**308 registrations that JDK 25 DOES declare are absent from 17 or 21.** They
were never in the 342 — `H25-1` could not see them, because on its one image
they were in good standing. Split by which releases declare them:

| declared by | rows | reading |
|---|---:|---|
| 21 + 25 | 193 | gone on 17 |
| **25 only** | **110** | **inert on both older images** |
| 17+21+25 | 34 | a PLATFORM split, not a version one |
| 17 + 21 | 32 | dropped in 25 — the `isBigEndian` shape |
| 17 only | 12 | |
| 21 only | 5 | |

The **110 declared only by JDK 25** are the interesting half: if CratonVM is
meant to run a 17 or 21 image, those registrations do nothing there, and nothing
reports it. That is `H25-1` N3a's near-miss argument in the version dimension
instead of the descriptor one.

### 2.3 The `dead-everywhere` 263, which is the actual work list

183 **truly-gone** (no image declares that method NAME on the hierarchy) and 80
**near-miss** (some image declares the name, no overload matches the
descriptor). Before §2.5's correction this read 305 = 225 + 80; the 42
`field-shaped` rows all came out of the truly-gone half, which is exactly where
they would: a field name is not a method name anywhere.

By owning file: `shared_secrets_bridge.rs` 26, `native-io/lib.rs` 22,
`unsafe_natives.rs` 19, `lib.rs` 18, `locale_bootstrap.rs` 18,
`native-collections/lib.rs` 17, `plain_socket.rs` 17, `nio_native.rs` 16,
`socket_channel.rs` 14, `async_socket.rs` 13. By lane ownership: **82 in
`native-io/` (W4), 17 in `native-collections/src/lib.rs` (W2), 3 in
`lang_string.rs` and 2 in `deprecated_lang.rs` (W3)**.

The near-misses concentrate in `streams.rs` (10),
`native-collections/lib.rs` (8), `nio_native.rs` (7), `panama.rs` (5).

### 2.4 The 124-row `no-image-class` difference is platform, not version

999 → 875. The 124 rows sit on 14 classes another image declares, and every one
is a platform class: `java/io/UnixFileSystem` (34),
`sun/nio/ch/UnixFileDispatcherImpl` (31), `jdk/net/LinuxSocketOptions` (15),
`sun/nio/ch/EPollSelectorProvider` (9), `sun/nio/fs/UnixFileAttributes` (9),
`sun/nio/ch/KQueuePort` (4), … That is the exact hazard
`no_image_receiver.rs` already documents, and it corroborates the class-granular
table rather than contradicting it.

### 2.5 CORRECTION — two implementations, and the disagreement was the finding

WORKER 3 built the same sweep independently and at the same path
(`409f5f630`, `scripts/jdk-only-no-image-methods.py`). It asks
`javap -p -s --system` per class where this one builds an in-process index per
image. Their numbers did not match mine, and **theirs had a category mine could
not produce: a registration whose name is a FIELD.**

That is not a taxonomy quibble. This sweep indexed the **method table** and
skipped the field table, so a registration naming a field found nothing on the
hierarchy and came out `dead-everywhere` — **into a committed TSV whose whole
purpose is to tell W3 and W4 what is safe to delete.** 42 rows.

The index now records field names, and `field-shaped` is a fifth verdict that
says DO NOT DELETE. Reconciled over the same population — the rows a JDK-25-only
adjudication calls "declared nowhere":

| this lane (index) | rows | WORKER 3 (javap) | rows |
|---|---:|---|---:|
| `dead-everywhere` / truly-gone | 162 | `DEAD_EVERYWHERE` | 192 |
| `dead-everywhere` / near-miss | 74 | `NEAR_MISS` | 63 |
| `cross-version` | 64 | `PARTIAL` | 62 |
| `field-shaped` | 42 | `LIVE (field-shaped)` | 38 |
| **population** | **342** | | **355** |

The two **agree on the load-bearing categories**: ~62–64 rows saved by an older
image, ~38–42 that are fields, and ~250 of 350 not declared with that
descriptor anywhere. The residual gaps are two:

* **The populations differ, 342 vs 355.** Different censuses; not reconciled
  here, and worth 13 rows on its own.
* **The truly-gone / near-miss boundary sits differently** (162+74 vs 192+63).
  This sweep asks whether the NAME appears anywhere on the *hierarchy*; a name
  inherited from a supertype makes a row a near-miss here and can leave it
  truly-gone there. Neither is wrong; they answer slightly different questions
  and **a lane quoting one must not mix it with the other.**

**Both scripts are kept.** The javap one is authoritative about member kinds
for free and needs no index; the index one is fast enough to run over all
10,378 registrations, and carries the coverage refusals, the canary and the
committed TSV. Deleting either without re-running the other removes the only
cross-check this measurement has — and the cross-check is what caught the 42.

## 3. The instrument, and the canary that proves it

Two scripts, both with a `--selftest` that fails. The sweep is
`scripts/jdk-only-image-method-sweep.py` (renamed at merge time, because
WORKER 3's landed first at `jdk-only-no-image-methods.py`).

**`scripts/jdk-only-image-method-index.py`** builds one method index per image.
It reads `lib/modules` through `jimage` — MEASURED: a JDK 25 `jimage` lists a
JDK 21 **windows** image — and parses class files to the method table in
process. `javap` over 27,000 classes is minutes per image for the same answer.
Its selftest exercises the two constant-pool shapes a naive loop gets wrong
(Utf8's variable length, `Long`'s stolen second slot), the skipped-attribute
walk, three malformed inputs, and all three archive layouts. **0 unparsed class
files across all nine images**, 26,559–28,059 classes each.

**`scripts/jdk-only-image-method-sweep.py`** is the method-granular sibling of
`jdk-only-no-image-receivers.py`. It resolves each triple against the class, its
superclass chain and its transitive interfaces, per image, and it **REFUSES** a
narrow image set the way the sibling does — fewer than two indexes, a missing
platform, a single release, or nothing older than 17. It is **strict-mode only**
and refuses a compatible-mode census outright: a `--synthetic-jdk` carrier CAN
declare a method the real image does not (`flag≠mode drops it`).

**The canary.** `java/lang/StringUTF16.isBigEndian()Z` must classify
`cross-version` on every real run, because the tree documents it as a deliberate
keep. If it does not, the run **exits 1 and disowns its own output**:

```
canary java/lang/StringUTF16.isBigEndian()Z: cross-version
```

It fired correctly. A sweep whose hierarchy walk is broken, whose old image is
missing, or whose index is thin cannot pass that check, and every other row it
prints would be untrustworthy if it did.

`scripts/baselines/jdk-only-no-image-methods.tsv` is the 691 actionable rows,
committed so nobody has to re-run a nine-image sweep to quote one.

## 4. What this does NOT establish

* **"Unreachable" is still ARGUED from resolution semantics, not probed.** No
  vector calls `Thread.destroy()` and asserts `NoSuchMethodError`. `H25-1` §4
  said this and it is still true.
* **Nine images is not every image.** Temurin only; no 17.0.x/21.0.x other than
  the ones named; no aarch64 build of anything (the mac arm is x64). A row
  declared only by an OpenJ9 or a Zulu image would read `dead-everywhere` here.
  The sweep's coverage check enforces three platforms and a release ≥3 apart;
  it cannot enforce a vendor.
* **`dead-everywhere` is not the same as "safe to delete".** `H22`/`H25-2`'s
  duplicate-registration trap is orthogonal: **162 triples are registered more
  than once and only the `owns_slot: true` one is reachable, so retiring the
  winner PROMOTES the loser.** Check `--dump-native-registry` before and after
  every deletion. This sweep says a row cannot be reached from an image; it says
  nothing about what takes its place.
* **The `--synthetic-jdk` mode is out of scope.** Some of these stubs may be
  load-bearing there, where a fabricated carrier CAN declare a method no real
  image does. Any gate built on this must be strict-mode-only.
* **The census is from a prebuilt binary, not from one built at this tree's
  HEAD.** The registration set is a property of the VM source; the reproduction
  in §1 of `H25-1`'s exact partition, and the identical re-take on `r11` after
  the merge, are the evidence that it is the same population. But a registrar
  added after `r11` was built (2026-08-21 18:42) is not in this sweep, and
  nothing here was measured on a binary compiled from the merged tree itself.
* **`H25-1` §2.1's hole is untouched.** The mechanism that drops the five
  `java/lang/Compiler` registrations from the strict dump is still not traced to
  a line. This record adds a reason to want it: N2 wants a method-granular gate
  in the same place.

## 5. NOMINATIONS

* **N1 — the block on the 342 is LIFTED, with a list.** W3 and W4 may act on
  `scripts/baselines/jdk-only-no-image-methods.tsv`. Rows marked
  `dead-everywhere` are retirable subject to the duplicate-registration check;
  rows marked `cross-version` **or `field-shaped` must not be touched**, and the
  `declared_by` column names the images that keep a cross-version row alive.
  **Anyone who pulled the pre-2026-08-22 TSV must re-pull it** — it marked 42
  `field-shaped` rows as `dead-everywhere`.
* **N2 — expect a census delta of ZERO and write it down first.** `H25-1` §3:
  these rows are never dispatched, so retiring them clears nothing from the
  1402. **A zero is the PASS.**
* **N3 — the 110 rows declared only by JDK 25 deserve a decision, not a
  deletion** (§2.2). They are the mirror image of `isBigEndian`: correct on the
  image this host runs and inert on the two it does not. Either the project
  supports 17/21 and they are a gap, or it does not and the `isBigEndian`
  comment is the thing that is stale. Nobody has stated which.
* **N4 — wire the sweep into `H25-1` N2's method-granular strict-mode gate.**
  The TSV is exactly the table such a gate needs, and the canary is exactly the
  check that keeps the table honest.
* **N5 — the 80 near-misses still need `H25-1` N3a's registration-time
  descriptor diff.** A near-miss is a live bug report: somebody wrote an
  interception that has never executed. This sweep proves they are near-misses
  on ALL NINE images, which is the part `H25-1` could not claim.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-1` — the method-granular nine-image sweep. **Of `H25-1`'s 342:
  64 are declared by JDK 17 or 21, 42 name a FIELD, and neither may be deleted;
  236 are dead as a method.** 308 further rows are declared on 25 and absent
  from 17/21, 110 of them on 25 alone. §2.5 reconciles this against WORKER 3's
  independent javap sweep — **the disagreement between the two is what caught
  the 42**, which this record's first TSV had marked retirable. Instruments:
  `scripts/jdk-only-image-method-index.py`,
  `scripts/jdk-only-image-method-sweep.py`,
  `scripts/baselines/jdk-only-no-image-methods.tsv`. MEASURED.
