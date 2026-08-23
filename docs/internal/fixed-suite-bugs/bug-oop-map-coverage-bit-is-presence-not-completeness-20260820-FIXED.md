# `fully_oop_covered` asserts a map EXISTS, not that it lists every live oop

## Status

**FIXED 2026-08-21, RETIRED 2026-08-22** -- re-verified on `dev` + 118 further
commits; see the closing section. **FIXED 2026-08-21** — the suppression this bit licenses is now **opt-in**
(`CRATONVM_GC_PRECISE_ONLY_ROOTS=1`), because the bit cannot be made sound at a
price worth paying and it was measured to be worth ~0.1 % of collections. The
runtime oracle named in the contract now gates the suppression when it is on,
and it can now *see* the cycles it gates — which it could not before, for a
structural reason nobody had noticed.

Two earlier rounds closed the codegen half (`30370b165`, `3d430ea69`), and a
third (`53876976a`) split the per-cycle PROOF from the SUPPRESSION so the proof
is computed on every collector rather than short-circuited away on two of them.
This round closes the last piece: the suppression itself.

## What the bit is spent on

`CompiledMethod::fully_oop_covered` is the codegen half of the proof that lets
`memory::roots::collect_roots` skip the conservative JIT root scan
(`moving_young_precise_only`). Skipping it is not a small economy: the
conservative scan is the only producer of G1's pin set and the only thing that
marks a JIT-held oop the maps do not name, so a collector that skips it on a
false proof has no backstop at all.

## What the bit actually checks

```rust
cm.fully_oop_covered = compiler.precise_maps
    && compiler.sp_id_slot_off != 0
    && compiler.inline_sites.is_empty()
    && compiler.safepoint_pcs.is_subset(&compiler.mapped_safepoint_pcs);
```

`safepoint_pcs ⊆ mapped_safepoint_pcs` is a **presence** test: every GC-capable
safepoint recorded *an* entry. It says nothing about whether the entry lists
every live oop at that safepoint.

## The contract was written down, and it could not have been honoured

`jit/src/x64/driver.rs`, immediately above the assignment:

> It is a NECESSARY codegen precondition; the runtime
> `CRATONVM_DBG_VERIFY_OOP_MAPS` oracle (Stage G0) is the SUFFICIENT proof that
> must gate the actual backstop suppression before the moving path relies on it.

The previous round of this page recorded that the oracle "gates nothing" —
`grep` for its counters outside its own module returns no consumers. That is
true and it understates the problem. **The oracle could not have gated it**,
because of where it runs:

```text
collect_roots
  └── if !moving_young_precise_only {          <-- the suppression
        scan_active_jit_frames                 <-- skipped when suppressing
          └── scan_one_frame_precise
                └── verify_precise_covers_conservative   <-- THE ORACLE
```

The oracle lives inside the scan the suppression skips. **On every cycle that
spent the bit, the instrument designated to check it was not running.** The
relationship is not "the wire was never connected" but "the wire, if connected,
would have measured the wrong cycles."

That retro-actively qualifies every number this page has ever reported. The
`while_covered=0` table in the previous round — three collectors, ~5 880 frames,
~90 700 verifiable words — was collected entirely on cycles that did **not**
take the suppression. It is evidence that the maps are good on the ordinary
path. It has never been evidence about the path the bit licenses.

## Measured 2026-08-21, on `dev@ee4cdf528`

`PolynomialTest` (`bc-java` ntru), `--Xmx 1g`, three collectors,
`CRATONVM_DBG_JIT_ROOTSCAN=1` + `CRATONVM_DBG_VERIFY_OOP_MAPS=1`:

| collector | collections | `precise_only=true` | `never_mapped` | `while_covered` | result |
|---|---:|---:|---:|---:|---|
| generational | 6 | **1** | 0 | 0 | FAIL |
| G1 | 1 | 0 | 0 | 0 | OK |
| ZGC | 3 | 0 | 4 | 0 | OK |

The staged-argument fix still holds: `while_covered` is 0 everywhere. ZGC's four
`never_mapped` hits are on frames that correctly report `covered=false`.

### The generational failure is NOT this bug — tested, not assumed

The table above is suggestive in the worst way: the only collector that takes
the suppression is the only one that fails, with
`IllegalFormatConversionException: d != java.lang.Object` — the exact signature
of a reference read back stale after the collector moved it, i.e. what a missing
root looks like.

It is a coincidence. A kill switch was added so the question could be settled
inside **one binary**, and ABBA-interleaved, three reps per arm:

| arm | `precise_only=true` per run | PASS | FAIL |
|---|---:|---:|---:|
| suppression ON (then-default) | 2, 2, 6 | 2 | 1 |
| suppression OFF | 0, 0, 0 | 2 | 1 |

Identical distributions. Removing the suppression entirely does not fix the
failure, so the failure is not the suppression's. It belongs to
`bug-generational-ntru-unpinned-jit-reference-20260821.md`, which reports the
same signature on pristine `dev` and was already open.

Worth recording separately: that page calls the failure **deterministic**
("FAIL 3/3"). Across the six runs here it was **2 of 6**, on a host whose load
moved between 5 and 42. It is load-sensitive, not deterministic, and an A/B that
assumes determinism will read noise as a result.

### The suppression is worth ~0.1 % of collections

`CRATONVM_GC_STRESS` forces a collection every N bytes, which turns "wait for a
rare pause shape" into a schedule that can be counted. Same workload, same
binary:

| stress step | collections | `precise_only=true` | share |
|---|---:|---:|---:|
| none | 10 | 0 | 0 % |
| 64 MB | 3 735 | 0 | 0 % |
| 16 MB | 14 420 | 2 | 0.014 % |
| 1 MB | 46 135 | 31 | 0.067 % |
| 1 MB (longer) | 70 144 | 84 | 0.120 % |

**The other 99.9 % of collections already run the conservative scan**, so
whatever the branch saves is bounded by that share.

**Re-measured after merging 220 dev commits**, because two of them change the
premise directly: `f4d697453` makes the optimizing tier compute
`fully_oop_covered` at all (so more methods can carry it), and `53876976a`
splits the per-cycle proof from the suppression so the proof runs on every
collector. Either could have raised engagement enough to change the trade.

| tree | collections | `precise_only=true` | share |
|---|---:|---:|---:|
| `ee4cdf528` (pre-merge), 16 MB stress | 14 420 | 2 | 0.014 % |
| merged with `2b034da6d`, 16 MB stress | **70 066** | **6** | **0.009 %** |
| merged, same stress, default (opt-in off) | 3 539 | **0** | 0 % |

Lower, not higher, on an order of magnitude more collections. The default flip
is better supported after the merge than before it. This is the number that
decides the trade, and it is an engagement count — it needed no quiet host,
which is fortunate, because the host spent this session between load 5 and 168.

## Follow-up 2026-08-21: the verdict is now COMPUTED on every collector

This record's §"What this does NOT establish" notes that true generational
"reports `incomplete=true` for other obligations on every collection, so it
never takes the suppression", and that "the two collectors where the proof *did*
pass are G1 and ZGC". The second half had a simpler explanation than it looked:
**the proof was never run on a G1 or ZGC cycle at all.**
`memory::roots::collect_roots` computed it inside a short-circuiting `&&` chain
whose second term was `heap.is_generational() || g1_precise_only_roots`, so
`refresh_moving_young_coverage_for_collection()` was skipped and the published
verdict stayed at the `false` — meaning *complete* — that
`begin_moving_young_coverage_cycle` had reset it to. It did not pass; nobody
asked.

Two changes, both landed:

* The proof and the conservative-scan SUPPRESSION are now separate expressions.
  The proof mentions no collector and runs on every cycle; the suppression stays
  generational-only, which is what
  `bug-g1-evacuates-live-jit-reference-20260819.md` asked for.
* `moving_young_unpublished_frame_oop_present`'s residency test asks
  `gen_heap::addr_is_movable` — the union of the generational young table with a
  new `MOVABLE_BOUNDS` table a collector fills to say what its relocating phase
  may move. `JIT_REGION_BOUNDS` was deliberately NOT filled: its second,
  load-bearing job is the inline-reference-store write-barrier gate that G1 and
  ZGC answer by leaving it empty.

Measured on one H2 class, `TestKillProcessWhileWriting`, per collection:

| collector | before | after |
|---|---|---|
| G1 | `incomplete=false` 722 499 / `true` 1 169 | `false` **0** / `true` 886 790, all `young-bounds-unpublished-verifier-vacuous` |
| ZGC | verdict never computed | proof ran on 263 cycles, passed 21 |

G1's answer is now correct rather than clean: it publishes neither table, so the
verifier honestly reports that it cannot classify. Its behaviour is unchanged —
`refuse_evacuation` is gated on `CRATONVM_G1_COVERAGE_PIN`, default off — but
`record_g1_pause_coverage` stops measuring a vacuous verdict.

ZGC acts on it: `relocate_stw` now compacts when the proof holds. See
`known-issues/h2/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`.

## The repair

**The suppression is opt-in.** `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` restores it;
the default is now to always run the conservative scan. A 0.1 %-engagement
optimisation is not worth a soundness argument that cannot be obtained, and the
flag keeps the old behaviour one binary away for anyone who wants to measure it.

**The oracle now gates, and can now see.** When the suppression is opted back in
AND the oracle is enabled, `collect_roots` verifies *before* it suppresses:
`verify_active_coverage_into` walks the same frames the backstop would and asks
the oracle about them. A refutation withdraws the suppression for that cycle and
latches for the process — the refutation is a statement about compiled code that
is still in the code cache, not about a moment, so a later cycle that happens
not to re-observe it has learned nothing new. New
`incomplete_reason::COVERAGE_ORACLE_REFUTED`, the only reason code produced by
*checking an answer* rather than by failing to establish a precondition.

Verifying costs the same frame walk as the scan it would skip — which is the
honest reason the gate cannot be made default-on, and a further argument that
the suppression is not worth having.

**The gate is proved to fire.** A gate whose branch has never executed is
indistinguishable from a broken one, and `while_covered` is 0 on every workload,
so nothing in the tree will exercise it. `CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE=1`
forces a refutation without a real unmapped oop:

| arm (`--Xmx 1g`, generational, `CRATONVM_GC_STRESS`) | collections | `precise_only=true` | `REFUTED` |
|---|---:|---:|---:|
| default (suppression off) | 21 517 | **0** | 0 |
| `PRECISE_ONLY_ROOTS=1` | 35 384 | **5** | 0 |
| `PRECISE_ONLY_ROOTS=1` + `FORCE_REFUTE=1` | 6 653 | **0** | **1** |

Row 2 is the control that makes row 3 mean something: the same configuration
without the forced refutation takes the branch 5 times, so row 3's zero is the
gate withdrawing a suppression that would otherwise have happened, not a run
that never had one to withdraw. (An earlier attempt at this proof produced
`precise_only=0` in *both* arms and proved nothing — the workload simply never
reached a suppression that run. `CRATONVM_GC_STRESS` is what makes the branch
frequent enough to be a control.)

```text
[VERIFY-OOP-MAPS] REFUTED: a frame asserting fully_oop_covered holds an in-band
oop no map names. Conservative JIT backstop is now forced ON for the rest of
this process.
[jitroots] precise_only=false moving_young=true incomplete=true ...
```

The message prints once — the latch is one-way — and every subsequent
`[jitroots]` line carries `precise_only=false incomplete=true`.

## What this still does not establish

* **No unmapped oop was found on a suppressed cycle.** The gate is a backstop
  built because the proof is unobtainable, not because a violation was caught.
* **The oracle's false-positive mode is unchanged**: a primitive whose bits land
  on a live object header still reads as an unmapped oop. With the gate armed,
  that now costs a withdrawn suppression rather than a log line — the safe
  direction, and at 0.1 % engagement it costs essentially nothing.
* The three unconditional drops in `emit_oop_map_for_safepoint` still fail closed
  from `30370b165`. That hardening never fired on any workload measured.

## Relationship to the other records

* `bug-g1-evacuates-live-jit-reference-20260819.md` — the same defect from the
  consumer end. Its fix stopped G1 depending on the bit; this record is the bit.
* `bug-generational-ntru-unpinned-jit-reference-20260821.md` — the failure that
  looks like this one and is not, per the A/B above. Its determinism claim needs
  re-checking.
* `feature-designs/zgc-jit-load-barrier.md` — owns the membership-walk cost that
  the same frames pay on the other collector.

## Reproducing

```bash
# engagement: how often is the bit actually spent?
CRATONVM_DBG_JIT_ROOTSCAN=1 CRATONVM_GC_STRESS=1048576 \
  <cratonvm> -XX:+UseGenerationalGC --Xmx 1g ... 2>&1 \
  | grep -c 'precise_only=true'

# the gate, forced (needs the opt-in, or there is no suppression to withdraw)
CRATONVM_GC_PRECISE_ONLY_ROOTS=1 CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE=1 \
CRATONVM_DBG_JIT_ROOTSCAN=1 CRATONVM_GC_STRESS=16777216 <cratonvm> ...
```

## Retirement 2026-08-22: re-verified, and the last open question is answered

Everything above was measured on `dev@ee4cdf528` plus a 220-commit merge. This
section re-takes the two measurements that decide whether the repair still holds,
on `dev` + 118 further commits, one binary, and closes the one item this page
left pointing at another record.

### Engagement and the gate, re-measured

`PolynomialTest`, `-XX:+UseGenerationalGC --Xmx 1g`,
`CRATONVM_GC_STRESS=16777216`, `CRATONVM_DBG_JIT_ROOTSCAN=1`. Each arm capped at
420 s of wall clock — the metric is a SHARE, so a truncated workload answers it,
and the untruncated run writes ~64 MB of `[jitroots]` lines:

| arm | collections | `precise_only=true` | `REFUTED` |
|---|---:|---:|---:|
| default (suppression opt-in OFF) | 36 559 | **0** | 0 |
| `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` | 37 047 | **5** | 0 |
| `PRECISE_ONLY_ROOTS=1` + `DBG_OOP_ORACLE_FORCE_REFUTE=1` | 27 499 | **0** | **1** |

Three things, all as this page reports them:

* **The default never takes the suppression.** Zero in 36 559 collections, so the
  conservative JIT backstop runs on every one of them and the soundness argument
  the bit could not supply is not needed.
* **The opt-in engagement is still ~0.01 %** — 5 in 37 047, against the 6 in
  70 066 recorded above. The trade the default flip rests on has not moved.
* **The gate still fires, and row 2 is still the control that makes row 3 mean
  something.** Same configuration without the forced refutation takes the branch
  5 times; with it, 0, and one `COVERAGE_ORACLE_REFUTED`. Row 3's zero is a
  withdrawn suppression, not an absent one.

### The one referral this page made is now closed

§"The generational failure is NOT this bug" ends: "It belongs to
`bug-generational-ntru-unpinned-jit-reference-20260821.md`, which reports the
same signature on pristine `dev` and was already open," and adds "that page calls
the failure **deterministic** … It is load-sensitive, not deterministic."

Both halves held up. The A/B here — suppression ON and OFF, identical
distributions — was right that the failure is not the suppression's, and the
determinism correction was right too. The ntru failure was `String.format`
reclaiming its own varargs array across `DecimalFormatSymbols.getInstance()`;
it is fixed, and it is neither a JIT-root nor a relocation defect. See
`bug-generational-ntru-unpinned-jit-reference-20260821-FIXED.md`.

### What is still open, and where it lives now

The three items under "What this still does not establish" are unchanged and are
statements of limit rather than work:

* no unmapped oop has ever been found on a suppressed cycle;
* the oracle's primitive-that-looks-like-an-oop false positive is unchanged, and
  with the gate armed costs a withdrawn suppression rather than a wrong answer;
* the three unconditional drops in `emit_oop_map_for_safepoint` still fail closed
  and have never fired.

One NEW item came out of the same partition while the ntru failure was being
chased, and it is a separate page rather than a residual here: the words
`band_slot_is_verifiable` SKIPS are neither verified nor rewritten, and after a
moving young collection a compiled frame's callee-saved GPR image can still name
a moved-from address. Measured, no failure attributed, repair available behind
`CRATONVM_REGISTER_IMAGE_REMAP=1` (default off). See
`moving-young-leaves-a-callee-saved-register-image-unrewritten-20260822.md`.
