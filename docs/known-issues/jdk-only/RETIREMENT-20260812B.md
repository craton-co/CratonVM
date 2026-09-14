# 2026-08-12 (second pass) — retirement audit: 8 records moved out, 2 held back

**What this is.** The 34-lane campaign closed with the strict and Compatible
arms both at **69 passed / 2 failed** (`C:/craton/close-strict.log`,
`C:/craton/close-compat.log`), the only failure being `RExceptions` — which is
deliberate and is `W7-37`'s live row, not a candidate for anything here. Ten
records carried an explicit retire verdict from the lane that adjudicated them.
This audit re-adjudicated all ten **against the tree and against running
binaries**, and moved eight. Two are held back, and one of those is held back on
a defect this pass measured for the first time.

**Nothing was deleted.** Each moved record carries a banner naming what
discharged it. Every move is a `git mv`, so history follows and a second
`git mv` reverses it. The command list is §2.

**The four rules this directory enforces, applied here.**

1. *Do not retire on a green headline.* Every record below was read to its end
   and its **quiet** rows adjudicated; two records failed that test and are in
   §3.
2. *A green vector closes a headline, not a record.* `RJdkLogging` PASS is not
   the evidence for `W7-91`/`W7-92` — the `CK` lines behind it are, and they are
   quoted with the pristine-dev control beside them.
3. *`probes/` is NEVER run by `run.sh` at any `SUITE=` value.* Where a record's
   only instrument was a probe, this pass **ran the probe itself** against both
   binaries rather than accepting a suite green in its place. That is `W7-13`,
   `W7-31` and `W7-92`.
4. *Retirement is the orchestrator's call and must carry per-record evidence.*
   §1 is one section per record; §2 is the command list for the orchestrator to
   run.

**Instruments used.** `scratchpad/bin/cratonvm-f8.exe` (the wave binary, all
fixes) and `scratchpad/bin/cratonvm-control-44044c7e2.exe` (pristine `dev`),
A/B'd against HotSpot Adoptium 25.0.3.9 at
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`. Where a number is quoted
below without a control column, the control was run and agreed.

---

## 0. Where retired records go, verified before anything moved

**`retired/` in the internal record tree, named
`jdk-only-<slug>-RETIRED-<date>.md`.** The directory exists, is populated
(46 files) and is tracked — the four records `RETIREMENT-20260812.md` moved are
there today
(`retired/jdk-only-W7-4-differential-probe-widening-round-2-RETIRED-20260812.md`
and its three siblings) and the worktree is clean, which an untracked add would
not be. README's phrase *"the internal record tree"* means this path.

**It is subject to a history rewrite, and that does not change the
destination.** An orchestrator effort has been removing the internal record
tree from a fresh public history since 2026-07-27 (durable design docs were
relocated to `docs/architecture/`, `docs/known-gaps/` and `docs/` top level;
bug-hunting logs, dated results and handoffs go with the folder). The rule that
follows is
**not** "pick another destination" — retiring a known-issue write-up *into*
the internal record tree is what that folder is for, and every retirement pass
in this directory has used it. The rule is about **citations**:

* never write an internal-record-tree path into a `.rs` comment or into a doc
  that survives under `docs/known-issues/` — refer to the retired write-up **by
  name** ("the retired `W7-13-strict-mh-insert-wrapper` write-up");
* put the durable facts — root cause, repro, residual — into the surviving
  record or the source comment *before* the move, not into the file being moved.

This audit follows both: §1 states each record's carry-forward sentence here, in
a file that stays, and the two residuals that needed a live home were
transplanted into live records (§1.2, §3.2) rather than left in a file that is
leaving.

---

## 1. Moved — 8 records

### 1.1 `W7-59-layout-detector-coverage.md` → RETIRED

**Verdict inherited:** RETIRE-RETIRED, *"what is left is a RUN, not a defect"*.
**Verified, and the verdict holds — with one correction.**

**Evidence, all against the tree:**

* the instrument is one implementation, `native-api/src/layout_alias.rs` (24 KB,
  present);
* `native-api/tests/layout_alias_coverage.rs` carries **13** `#[test]`s — the
  record says "twelve plus `census`", which reproduces exactly;
* `CRATONVM_DBG_LAYOUT_ALIAS` appears **zero** times under `regression-suite/`
  or `ci/`, so no scheduled run has ever produced a row of this census. That is
  the whole remainder, and it is a scheduling gap;
* every §8 row has a named live owner (W7-66 the OVER rows, W7-68 the UNDER
  register, W7-61 `SSLEngine`, W7-69 the read-side species, W7-72 §1 the slot-5
  `keys` clobber). No enumerated row is ownerless.

**The correction.** The banner's *"`register_selector` has zero occurrences
anywhere in the workspace"* is true of the **function** and false of the
**string**: three occurrences survive, all tombstone comments
(`native-io/src/lib.rs:18636`, `:18730`, `:21395`). The substance — the dead
code is gone, its `SelectionKey` 4-vs-1 and `HashSet` 2-vs-1 sites cannot
reappear in a census — holds.

**Measurement taken this pass, because an instrument nobody has run is an
instrument nobody has checked.** The census emits:

```
$ CRATONVM_DBG_LAYOUT_ALIAS=1 cratonvm-f8 --jdk-only --java-home <jdk25> -cp . MhFamilyProbe
[cratonvm] 1 per-flag variable(s) set directly; the supported spelling is now: CRATONVM_DBG=layout-alias
WARN cratonvm_native_api::layout_alias: native allocated slots against a class declaring NONE …
  class="<unresolved:ClassId(0)>" requested_fields=3 real_fields=0 direction="undeclared"
  site=jdk/internal/util/ClassFileDumper.<clinit>()V <- java/lang/invoke/MethodHandles$Lookup.<clinit>()V
```

So the flag is live and the row shape is the one W7-73 predicts. **Two things
for whoever schedules it:** the per-flag spelling is deprecated in favour of
`CRATONVM_DBG=layout-alias`, and the first row a twelve-line probe produces is
already an `undeclared`/`ClassId(0)` row — the census will not be short of
input.

**Carry forward:** the layout-alias census has never been scheduled and its
tables are a source-level upper bound; the flag works, the gap is
`regression-suite/` and `ci/`, and W7-69 carries the same row for the read side.

### 1.2 `W7-13-strict-mh-insert-wrapper.md` → RETIRED (FIXED)

**Verdict inherited:** both items closed, *"RETIRE-FIXED candidate pending a
rebuild of the headline carrier fix"*. **The rebuild has happened and this pass
ran the record's own falsifier — all three steps, not the one-bit vector.**

The record is explicit that *"a green `RJdkHandles` alone is not enough
evidence"*, because it stops at the first combinator. So `MhFamilyProbe` was
re-created from the record's own table (twelve combinators, catch per step) and
run three ways:

```
                       HotSpot 25   f8 --jdk-only   control --jdk-only
insertArguments             15            15               15
permuteArguments             7             7                7
filterArguments              4             4                4
guardWithTest                8             8                8
asCollector                  6             6                6
asSpreader                   3             3                3
filterReturnValue            4             4                4
foldArguments               11            11               11
collectArguments             6             6                6
catchException              42            42               42
dropArguments                3             3                3
asVarargsCollector           6             6                6
```

Twelve of twelve, identical to HotSpot, where the record measured **ten
`NoClassDefFoundError`s** on the pre-fix binary. The sharper instrument the
record names agrees: `--jdk-only-report` over that run holds **0** rows whose
`class` begins `__mh_` (13 `compatibility-class-requested` rows survive, none of
them this family — the largest populations are
`io/netty/internal/tcnative/NativeStaticallyReferencedJniMethods`,
`java/util/concurrent/locks/StampedLock` and the `cratonvm/internal/Unmodifiable*`
set, which belong to other records).

**Both of its "observed on the way, not fixed here" rows are also closed, and
they were measured rather than assumed** — this is the part of the record that
would have been buried by a suite green:

* `asCollector` under `--real-jdk` answers **6**, HotSpot's value, where the
  record measured a disagreement (`MH_KIND_COLLECT` boxing into a reference
  array for an `int[]` collector);
* `bindTo` on a leading `int` parameter now refuses with the exact HotSpot
  sentence — `java.lang.IllegalArgumentException: no leading reference
  parameter` — on **both** arms, where the record measured CratonVM accepting it
  and answering 5.

**The one row that was NOT closed has been transplanted, not dropped.** The
second, disagreeing slot map for `java/lang/invoke/MethodHandle` in
`classloader.rs` (`MH_BASE = 16` with a different field order from
`lang_invoke`'s, so slot 16 is a `String` in one map and an `Int` in the other)
is now stated inline in `W7-19-methodhandles-compatible-residuals.md` §5.2.1,
which previously carried it only as a pointer at this record. W7-19 is live and
is the MethodHandle-layout record; the row wants a `--dump-native-registry` diff
and then a deletion, not a repair.

**Carry forward:** a carrier a VM invents for its own state goes through the
VM-internal door, never the compatibility stand-in door — and the split that
made this defect visible was **arity**, not semantics (the two combinators whose
state fits in one reference were green throughout).

### 1.3 `W7-40-differential-at-14.md` → SUPERSEDED

**Verdict inherited:** superseded by W7-42; its title says so. **Verified.**

The H1 reads *"W7-40 — SUPERSEDED. The differential is 9, not 14"* and the
banner names W7-42 and forbids working from the 14. What this pass checked is
the thing a supersession can quietly get wrong — **whether any of its rows
loses its owner on the way out**. It does not: all five genuine value
divergences are owned by live records —
`stream.reuseThrows` by `W7-65-stream-reuse-throws.md` (closed there), and
`format.*` ×5, `Enum.valueOfBadName`, `NumberFormat.currencyNegativeUS` and
`Random.nextGaussian` all appear in `W7-44-numberformat-enum-and-double-tostring.md`
and in `W7-42-differential-instrument-holes.md`. Nothing in the file is a
current measurement.

**Carry forward:** this record is *why* the number 14 circulates — five of its
fourteen rows were the instrument, not the VM; the live figure is 9, in W7-42,
and a fresh run must be diffed against W7-42's transcript and its
`PROBE-MANIFEST-DIGEST`, never against W7-4's retired 540-line oracle.

### 1.4 `W7-48-fjp-unapplied-patches.md` → RETIRED

**Verdict inherited:** RETIRE-RETIRED, **conditional on `W6-9` §7.5 getting a
README row**. **The condition is satisfied — confirmed, not assumed.**

README §2.2 carries the row today, in the `W6-9-complete-erases-the-abnormal-record.md`
entry: *"NEW ROW 2026-08-12 — its §7.5 named a divergence that had never been
indexed. `ForkJoinPool.invoke(task)` reads `fjp_state_get` while `join()` reads
`fjp_state_get_checked`, so on a **cancelled** task `pool.invoke` hands back the
cached result …"*. That was the one thing holding this record open.

**Its own deliverable is verified by a run.** The lane could not build; its
coverage — `completionRecord()` in `regression-suite/src/RJdkForkJoin.java`, the
three RED-before assertions — is in the tree at `:370` (`the abnormal record
survives a later complete()` `:388`, `a cancelled task is abnormal` `:410`,
`reinitialize() makes compute() RUN again` `:436`), the vector is in
`JDKONLY_CLASSES` (`regression-suite/run.sh:119`), and it passes in **both**
arms: `RJdkForkJoin PASS`, close-strict.log:68 and close-compat.log:68.

Its remaining §7 items all have live owners: the eager-fork Spring/H2 A/B is
`W3-4`'s (README §2.2), `W6-9` §7.5 and §7.3 are `W6-9`'s, and the
`duplicate_registration_gate.rs` CI wiring is `W7-30`'s (README §2.1).

**Carry forward:** *a record that prescribes a patch does not learn that the
patch landed* — nine of nine "unapplied" patches here were already in the tree,
and the status line, which is the only thing a reader checks, is the thing
nobody edits.

### 1.5 `W7-82-forname-duplicate-define.md` → RETIRED (FIXED)

**Verdict inherited:** RETIRE-FIXED candidate; residual closed by W7-87.
**Verified in the tree and by a run — and the closure is stronger than
"closed": W7-87 *subsumes* it.**

* W7-87 deleted W7-82's additive block and replaced the branch predicate
  outright — `classloader.rs:2707` now reads
  `… && (is_generated_proxy_name(internal_name) || !is_bare_url_class_loader(ctx, this))`,
  and W7-87 states that its group 8 asserts nothing W7-82 fixed was given up.
  So the fix for this headline is live, in a different shape than this record
  describes, which is itself a reason not to leave the record standing as a
  description of the tree.
* The vector is green in both arms: `RLoaderChurnDefine PASS`
  (close-strict.log:48, close-compat.log:48) — the vector this record extended
  with `repeatLookupIsACacheHit()` (+13 checks), which is the assertion that
  goes red on the defect.
* The named residual (a bare `URLClassLoader` reaching the built-in branch's
  global fallback, so `findLoadedClass` reports a class it never initiated) is
  **W7-87's subject line**, and W7-87 is live with 17 asymmetric consumers
  remaining.

**Carry forward:** the defect was never a re-define — the **cache probe ahead of
the define went blind**, and one blind probe defeated four separately-written
recovery arms that had the right behaviour written down.

### 1.6 `W7-38-crypto-trio-verified.md` → RETIRED (FIXED)

**Verdict inherited:** its last gated row (ChaCha20-Poly1305) was found already
implemented with the RFC 8439 §2.5.2 vector. **Verified — and the record's own
*live residual* is what actually closed.**

That residual was the **instrument**: `provider_chain.rs:1187` asserted
`RChaCha20Cipher` matched HotSpot byte-for-byte, `run.sh` listed it in
`CORE_CLASSES`, and the file was untracked, so `prune_missing` silently removed
it from every scheduled run — three places claiming green over an absent vector.
Now:

* `regression-suite/src/RChaCha20Cipher.java` is in the tree (17,950 bytes);
* it is in `CORE_CLASSES` (`regression-suite/run.sh:106`);
* it **runs and passes in both arms** — `RChaCha20Cipher PASS`,
  close-strict.log:43 and close-compat.log:43 — and it is not among the vectors
  the harness flags for publishing no check count, so it reached the harness's
  own counting gate;
* the RFC vector the record named as the deliverable is present as
  `native-builtins/src/chacha20.rs:459`, `fn rfc8439_2_5_2_poly1305()`.

The four remaining "fabrication gone, functionality not there" rows (Blowfish,
RC4/ARCFOUR, `HmacSHA224`, `keygen.Blowfish`) were closed by W7-39 through the
real SunJCE SPI, and W7-39 is live.

**Carry forward:** an equality test over two refusals is not a measurement —
this record's first probe reported `Blowfish.equalsRc4=true` when it meant
"neither ran", which is the campaign's dominant failure shape appearing inside
the instrument built to detect it.

### 1.7 `W7-67-host-default-locale.md` → RETIRED (FIXED)

**Verdict inherited:** RETIRE-FIXED, rewritten down to its one residual
sentence. **Verified in the tree, and the residual has a home in the source.**

* the Windows derivation landed: `vm/src/vm/vm_init.rs:419` documents the
  `java_props_md.c` mirror and `:490`/`:520` declare and call
  `GetUserDefaultUILanguage()`;
* the three-slot `Locale.Category` cache is rooted:
  `gc_scan_locale_roots` at `native-builtins/src/lib.rs:26184` with its remap
  companion at `:26194`, delegating to `locale_bootstrap`;
* the residual — a category whose *script* or *variant* differs from the base is
  not representable in the synthetic `Locale`'s `(language, country, tag)` side
  table — is written at `locale_bootstrap.rs:205-210`, in
  `resolve_default_locale_for`'s own doc comment, which cites this record by
  name. That is the convention: durable fact in the source, write-up retired.

Its two superseded expectations are correctly labelled inside it (stages 2 and 3
went to W7-80; the `%t` half to W7-91) and both successors are live.

**Carry forward:** a hardcoded value documented as a fallback is how a hardcoded
value survives review — `derive_locale()`'s final line `("en", "US")` was
reached on **every** Windows host, unconditionally, and the doc comment said so
out loud.

### 1.8 `W7-92-system-timezone-answers-utc.md` → RETIRED (FIXED)

**Verdict inherited:** filed and fixed this campaign; `RJdkLogging` passes in
both arms. **Verified — and the `PASS` is deliberately not the evidence.** Its
own §6 says *"do not pin 179"*, so the row was re-measured directly, with the
control beside it:

```
CK RJdkLogging streamBytes / handlerLevelGate bytes / defaultZoneRawOffsetMs
HotSpot 25            177   88   -10800000        PASS RJdkLogging (79 checks)
f8       --jdk-only   177   88   -10800000        PASS RJdkLogging (79 checks)
control  --jdk-only   175   87            0       PASS (own assertions; the cross-VM diff is the judge)
```

`defaultZoneRawOffsetMs` is this record's designed witness and it moves
`0 → -10800000`, matching HotSpot exactly. **The byte counts alone could not
have proved it at this hour**, and that vindicates the record's refusal to pin a
number: the model is `2M + 2H + 167`, and at 14:xx local the 12-hour field is one
digit on *both* VMs, so `H=1` for the broken VM and the fixed one alike — the
whole `175 → 177` movement here is `M`, W7-91's month name. A lane that had
pinned 179 would have read this run as a failure.

**This pass settled the one open question the record left for a run.** §7 asks
which of two routes carries the answer in `--jdk-only`, and says the
`ZoneInfoFile` `<clinit>` comment should be retired if the named id resolves:

```
                       TimeZone.getDefault().getID()   getRawOffset()   ZoneId.systemDefault()   user.timezone
HotSpot 25             America/Buenos_Aires             -10800000        America/Buenos_Aires     America/Buenos_Aires
f8 --jdk-only          America/Buenos_Aires             -10800000        America/Buenos_Aires     America/Buenos_Aires
f8 --real-jdk          America/Buenos_Aires             -10800000        America/Buenos_Aires     America/Buenos_Aires
control --jdk-only     UTC                                      0        UTC                      (empty)
```

**The named IANA id resolves.** So it is the `getTimeZone(zoneID, false)` route,
not the `getSystemGMTOffsetID` fallback, and the Round-13-era `lib.rs` comment
claiming `ZoneInfoFile`'s `<clinit>` cannot load `tzdb.dat` in strict mode is
**stale and should be retired by whoever owns that file**. The `user.timezone`
memoisation the fix performs is visible in the same row.

**Carry forward:** four producers on two dispatch routes — a fix to the
`--jdk-only` producer alone would have left Compatible red, and the vector
deliberately reads `getRawOffset()` rather than the zone id, because the
region-row narrowing in §4 can legitimately give a different *name* for the same
*time*.

---

## 2. The `git mv` command list

Run from the repository root. Eight moves, one per record; the destination
directory already exists and is tracked.

```sh
git mv docs/known-issues/jdk-only/W7-59-layout-detector-coverage.md \
       retired/jdk-only-W7-59-layout-detector-coverage-RETIRED-20260812.md

git mv docs/known-issues/jdk-only/W7-13-strict-mh-insert-wrapper.md \
       retired/jdk-only-W7-13-strict-mh-insert-wrapper-RETIRED-20260812.md

git mv docs/known-issues/jdk-only/W7-40-differential-at-14.md \
       retired/jdk-only-W7-40-differential-at-14-SUPERSEDED-20260812.md

git mv docs/known-issues/jdk-only/W7-48-fjp-unapplied-patches.md \
       retired/jdk-only-W7-48-fjp-unapplied-patches-RETIRED-20260812.md

git mv docs/known-issues/jdk-only/W7-82-forname-duplicate-define.md \
       retired/jdk-only-W7-82-forname-duplicate-define-RETIRED-20260812.md

git mv docs/known-issues/jdk-only/W7-38-crypto-trio-verified.md \
       retired/jdk-only-W7-38-crypto-trio-verified-RETIRED-20260812.md

git mv docs/known-issues/jdk-only/W7-67-host-default-locale.md \
       retired/jdk-only-W7-67-host-default-locale-RETIRED-20260812.md

git mv docs/known-issues/jdk-only/W7-92-system-timezone-answers-utc.md \
       retired/jdk-only-W7-92-system-timezone-answers-utc-RETIRED-20260812.md
```

`W7-40` takes `SUPERSEDED` rather than `RETIRED`, matching the
`jdk-only-jul-logrecord-infercaller-SUPERSEDED-20260812.md` precedent: it is not
finished work, it is a wrong number kept for navigation.

Each moved file already carries its RETIRED banner, so the move needs no second
edit. README's §2.0 rows for all eight are in place before the move, which is
the order that leaves no window where the index points at nothing.

---

## 3. NOT retired, and why — this is the load-bearing half

### 3.1 `W7-31-enable-preview-wiring.md` — HELD. Its own falsifier found a live row.

The verdict handed over was ADJUDICATED-CLOSED / WILL NOT FIX, with five reasons
and a reopen condition, and *"what this record still needs is a build, not a
patch"*. The build exists now, so this pass **ran all three of its falsifiers**.
Two pass. The third exposed a defect.

**Falsifier 1 — the flag and the gate. PASSES.** A plain `69.0` class with
bytes 4..5 overwritten to `FF FF`:

```
HotSpot                    -> UnsupportedClassVersionError: Preview features are not enabled for Q (class file version 69.65535). Try running with '--enable-preview'
HotSpot --enable-preview   -> ran-Q
f8                         -> linkage error: Preview features are not enabled for Q (class file version 69.65535). Try running with '--enable-preview'
f8 --enable-preview        -> ran-Q
```

The flag parses (it was `error: unexpected argument` on the pre-change binary),
the gate refuses, and the sentence is HotSpot's word for word.

**Falsifier 2 — the two bits must agree, on BOTH arms. PASSES.**
`jdk.internal.misc.PreviewFeatures.isEnabled` reflected through
`--add-exports=java.base/jdk.internal.misc=ALL-UNNAMED`: `false` without the
flag and `true` with it, on HotSpot, on `f8 --real-jdk` and on `f8 --jdk-only`.
The record is right that one arm agreeing proves nothing; both arms agree.

**Falsifier 3 — a nameless define must say `<Unknown>`. PASSES ON THE TEXT AND
FAILS ON THE TYPE.** `ClassLoader.defineClass(null, b, 0, b.length)` over the
same 69.65535 bytes:

```
HotSpot : java.lang.UnsupportedClassVersionError: Preview features are not enabled for <Unknown> (class file version 69.65535). Try running with '--enable-preview'
f8      : java.lang.ClassFormatError: : defineClass1: Linkage(UnsupportedClassVersionError { class_name: "", message: "Preview features are not enabled for <Unknown> (class file version 69.65535). Try running with '--enable-preview'" })
```

Identical on `--jdk-only`, on `--real-jdk`, and on the pristine-dev control — so
this is **pre-existing, not this wave's**, and it is not the `<Unknown>`-versus-
`""` question §6 declined. It is the record's part **C** unfinished on the road
part C was written for: the typed `LinkageError::UnsupportedClassVersionError`
survives on the main-class path and is re-wrapped on the `ClassLoader.defineClass`
path. `native_classloader_define_class1`
(`native-builtins/src/lang_system.rs:5011`) ends every failure with
`Err(define_class_format_error(&name, "defineClass1", msg))`, which flattens a
typed linkage error into `ClassFormatError` **and leaks a Rust `Debug` rendering
into a Java exception message**. A caller catching `UnsupportedClassVersionError`
— which is what a container does when it probes whether it can load a bundle —
does not catch it here.

That is a live row inside a record whose banner says nothing is left, which is
the exact shape this directory exists to catch. `W7-31` stays, with the
measurement written into it. **It does not want a new record number**: two other
lanes are running and would collide on `W7-94`.

### 3.2 `W7-91-format-date-symbols-hardcoded-english.md` — HELD, on a row its own author flagged.

Its headline **is** discharged, and by measurement rather than by a `PASS`: the
month name is the whole of the `175 → 177` movement in §1.8's table, and
`CK RStrings` carries the seven new checks. Its §4 (the hour) went to W7-92 and
is closed. Its §8 falsifier — *"`streamBytes` must move 175 → 177 against
HotSpot's 179"* — is **superseded**: both VMs read 177 in the same session,
because W7-92 landed too and because `H` is one digit for everyone at this hour.
That correction is now written into the record.

**What holds it open is §5, "The numeric half, deliberately not moved."**
`String.format("%,.2f", x)` with no `Locale` still localizes against ROOT where
a real `Formatter` uses `Locale.getDefault(FORMAT)`. The record calls it *"the
same one-line question, answered differently on purpose"* and says it *"probably
wants taking — but it wants its own measurement."*

`RETIREMENT-20260812.md` §3 already ruled on exactly this shape, for `W6-2`'s
constructor-form check: *"a deferral for want of a measurement rather than a
refusal on the merits, so it is carried as a live row too, not written off."*
This is the same shape, and unlike `W7-67`'s residual it is **not** recorded at
its source site — nothing in `lang_string.rs` states the asymmetry — so
retirement would drop it. Held, and rewritten down to that one residual so the
record is one page about one live thing.

Its natural successor if anyone wants it closed rather than carried is
`W7-34-formatter-family-residuals.md`, which owns the `Formatter`-receiver
locale and is live in README §2.2.

### 3.3 Do NOT retire these, stated so nobody tries

Re-stated from the campaign's own adjudications; none was re-opened here.

| Record | Why it stays |
|---|---|
| `W7-37-differential-throwable-and-vm.md` | Its `aastore` half is the one live red. `jit/src/x64/bytecode_walk.rs:1778` lowers `aastore` inline and never calls `self.helpers.aastore`, so `jit_aastore` is dead code on x64 and the JIT performs a store the interpreter refuses. Patch written in its Part 4, deferred pending a store-heavy A/B. **`RExceptions` is the suite's only failure in both arms and this is why.** |
| `W7-22-shadow-retirement-logging-and-time.md` | Needs a Linux re-freeze: three artefacts are keyed `25/linux` and both gate scripts exit 2 ("REFUSING") on this host. |
| `W5-1-loadlibrary-allowlist-too-wide.md`, `W6-6-nativelibraries-load-fabricated-success.md` | Need a Linux A/B before the `BootLoader.loadLibrary` arming can be measured. |
| `W7-20-refusal-laundered-into-wrong-answer.md`, `W7-62-ratchets-and-dead-code.md` | The stub ratchet is FIRING, 1253 → 1261, with six natives now attributed to JUL retirements, see W7-62. A ratchet that fires is not a record you retire. |
| `W7-93-stackwalker-option-constants-null.md` | Open items remain. |
| `W2-3-module-descriptor-answers-empty-sets.md` | The bridge never landed; all four parts are unwritten. |
| `W7-53-blocking-close-family.md`, `W7-61-sslengine-layout-and-tls-blocking.md` | The four TLS sites have a corrected design and no implementation. |

---

## 4. Corrections and findings this pass produced, for records that stay

Small, and each one is a number or a claim that a later lane would otherwise
inherit.

* **The closing logs print no check counts.** `close-strict.log` and
  `close-compat.log` carry `PASS`/`FAIL` per vector and nothing else, and the
  two files are byte-identical apart from their timestamps. So **no record whose
  falsifier is `PASS X (N checks)` is fully discharged by those logs** — the
  `PASS` is, the count is not. Where a count mattered above it was re-measured
  by running the vector directly.
* **`W7-85-serviceloader-stream-validation.md` — its vector's size is stale in
  two places.** README §2.3 and the record say `RJdkModule` runs **104/104**;
  measured today it is **163 checks** on HotSpot and on `f8 --jdk-only`,
  identical, with the record's own falsifying line matching exactly:
  `CK RJdkModule rejected=Rejected iterator=ServiceConfigurationError
  stream=ServiceConfigurationError cause=none stillGood=[module-factory,
  module-hello]`. That is the fix verified — the row README §2.3 says is *"only
  the fix"* unverified — but the record is not retired here: it was not on the
  candidate list, and this pass did not read its other rows.
* **`W4-2-unnamed-accessor-bypasses-encapsulation.md` is verified by a run.**
  README §2.2 records it FIXED IN SOURCE, unrun. The control binary dies in
  `RJdkModule` with `AssertionError: Exported[] must report its component's
  module, got module java.base` (`RJdkModule.java:807`, `arrayModules`), and
  `f8` passes the same vector. Row updated in place.
* **`CRATONVM_DBG_LAYOUT_ALIAS` is a deprecated spelling.** The binary answers
  `the supported spelling is now: CRATONVM_DBG=layout-alias`. Every record that
  prescribes the old form (W4-4, W7-49, W7-58, W7-66, W7-69, and the retired
  W7-59) prescribes a form that still works but warns.
* **`W7-92` settled a `lib.rs` comment.** The Round-13-era claim that
  `ZoneInfoFile`'s `<clinit>` cannot load `tzdb.dat` under `--jdk-only` is
  refuted by §1.8's zone-id table and should be retired by that file's owner.

---

## 5. What this audit did not re-audit

The thirty moves in `RETIREMENT-20260811.md` and the six in
`RETIREMENT-20260812.md` were not re-opened. `W6-2` stays out, on
`RETIREMENT-20260812.md` §3's reasoning, which this pass did not re-test.

README §2.3 was **not** swept record by record. One §2.3 record (`W7-85`) was
adjudicated because its README row states that only the fix is unverified and
names the vector; the rest of §2.3 needs the same treatment one record at a
time, and the general finding in §4 — that a suite log without counts discharges
a `PASS` and not a count — is the constraint any such sweep runs into.

The four records reserved for other running lanes (`W4-4`, `W6-5`, `W7-41`,
`W7-43`) were not touched, and neither was anything the stub-census or
app-readiness lanes own.
