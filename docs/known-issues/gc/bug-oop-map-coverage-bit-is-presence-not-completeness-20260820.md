# `fully_oop_covered` asserts a map EXISTS, not that it lists every live oop

## Status

**FIXED 2026-08-21** — the suppression this bit licenses is now **opt-in**
(`CRATONVM_GC_PRECISE_ONLY_ROOTS=1`), because the bit cannot be made sound at a
price worth paying and it was measured to be worth ~0.1 % of collections. The
runtime oracle named in the contract now gates the suppression when it is on,
and it can now *see* the cycles it gates — which it could not before, for a
structural reason nobody had noticed.

Two earlier rounds closed the codegen half (`30370b165`, `3d430ea69`); this
round closes the consumer half.

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
whatever the branch saves is bounded by that share. This is the number that
decides the trade, and it is an engagement count — it needed no quiet host,
which is fortunate, because the host spent this session between load 5 and 168.

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
