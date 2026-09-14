# 2026-08-12 — retirement audit: 4 records moved out, 1 held back

**What this is.** `W7-55-record-reconciliation.md` §4 nominated **five** records
as open purely as bookkeeping — *"nothing live in them at all"* — and
deliberately did not move them, on the grounds that retirement is the
orchestrator's call. This audit re-adjudicated all five, **row by row and not on
the headline**, and moved four with `git mv` so history follows and the move is
reversible with a second `git mv`. **Nothing was deleted.** Each moved record
carries a banner at its top saying what discharged it and on what evidence.

**One of the five was NOT retired.** `W6-2` carries a live divergence, and the
one that stopped it is not the item W7-55 argued about. See §3.

**Method.** `git log -S` on a literal from the patch body, plus reading the
current source; `git merge-base --is-ancestor` before believing any commit hash;
and — where a record left a specific command open — running that command. Every
commit cited by W7-55 for these five was checked and is an ancestor of HEAD.

**The counter-rule this directory exists to enforce.** Do not retire on a green
headline. `W4-2`'s urgent-looking rows were all stale while its quiet one was
live; a source-only audit in this campaign produced the same shape again. So
each record below was read to the end, and the *quiet* rows are the ones written
up.

---

## Moved — 4 to `retired/`

None of the four is a defect record. Two are measurement records, one is an
instrument record, and one is a wiring record whose work had landed.

| Record | Now | The rows that had to be adjudicated, and their verdicts |
|---|---|---|
| `W7-4-differential-probe-widening-round-2.md` | `retired/jdk-only-W7-4-differential-probe-widening-round-2-RETIRED-20260812.md` | Deliverable (*"run the CratonVM side"*) **superseded** — W7-32 ran it, W7-33/36/37/40/42 acted on it. Widening **landed**: all 15 sections in `probes/ShadowDifferentialProbe.java`, both hazard fixes (`drainBounded` `:2878`; the two unbounded caps `:2845-2870`), and the `thrownDetail` discipline (`:2812`). Seven "families considered and rejected" are **argued refusals**. **Its 540-line oracle is now stale and actively harmful** — the probe gained a manifest ledger and the transcript is 864 lines / `PROBE-MANIFEST-DIGEST=22732607802c59c2`. That is a reason to retire, not to keep. |
| `W7-11-strict-baseline-remeasured.md` | `retired/jdk-only-W7-11-strict-baseline-remeasured-RETIRED-20260812.md` | Closed at 68/0 the day it was written. Its four named defects all **landed**: `46bb0ad2e` (common-pool factory from the image), `8b4443fc6`+`beb8acee7` (`HashMap$KeyItr`), `5266bf8c7` (annotation carrier through the VM-internal door), `87ab40daf` (MH combinator carriers). **Quiet row:** its `cratonvm/internal/Unmodifiable*` link was filed as *"plausible and not yet proven"* and the four closures name four unrelated causes — it was never the mechanism, and five of the six names it lists are gone from `native-api/src/no_image_receiver.rs`. |
| `W7-28-preview-classfile-gating.md` | `retired/jdk-only-W7-28-preview-classfile-gating-RETIRED-20260812.md` | **The worst status line in the directory** — *"the switch that turns it off is NOT WIRED"* over four applied parts. A/B **landed** `de9bedeef`, C **landed** `6ce65f98a`, and C3's *"do not invent the placeholder — measure it first"* caveat was **honoured** (the Adoptium 25.0.3.9 measurement is written into `classloading/src/class_manager.rs:5260-5278`). **D was settled by running this record's own command** — see §2. §3.5's refusal to add a `CRATONVM_*` twin is an argued refusal and is held. |
| `W7-32-round-2-differential-run.md` | `retired/jdk-only-W7-32-round-2-differential-run-RETIRED-20260812.md` | Pure measurement (96 divergences). Its one prescriptive row — *"fix the two throws first, then re-take the diff"* — **discharged** by W7-33 (`aab87e003`); re-taken with 0 sections died. All 96 rows owned by W7-33/34/36/37/41/44/65. |

### A correction to W7-55, carried here so it is not inherited

W7-55 justified retiring `W7-28` part **D** by citing the reader gate at
`reader/src/class_file_version.rs`. **That answers a question D never asked** —
D is about two shell scripts, not the reader. The conclusion survives only
because the measurement in §2 was actually taken. Cite §2, not W7-55, for D.

---

## 2. The one row that was settled by running something

`W7-28` part D asked whether the two TornadoVM benchmark class files are
actually preview-stamped, and named the command. It had never been run. Run
2026-08-12:

```
$ od -An -tx1 -N8 bench-tornado/PolyEvalTornado.class
 ca fe ba be 00 00 00 45
$ od -An -tx1 -N8 bench-tornado/VectorAddTornado.class
 ca fe ba be 00 00 00 45
```

`minor = 0x0000`, `major = 0x45 = 69`. **Neither is preview-stamped**, so
neither can reach the new arm — and independently, neither is ever run by
CratonVM: `bench-tornado/run.sh:44` execs `tornado`, and the CratonVM arms of
`scripts/internal/bench-poly-4way.sh` (`:52`, `:62`, `:72`) run
`CpuPolyBench`/`GpuPolyBench` from `apps/gpu-bench/classes`, which nothing
compiles with `--enable-preview` (repo-wide, that flag appears only at
`bench-tornado/run.sh:37` and `scripts/internal/bench-poly-4way.sh:85`).

D is closed on measurement rather than on reasoning. It cost one command.

---

## 3. `W6-2` was NOT retired, and the reason is a row nobody had recorded

W7-55 §4 argued `W6-2-module-serviceloader-provider-factory.md` was empty, on
the grounds that its two *"deliberately NOT done"* items are argued refusals
rather than unfinished work. Both of those items were re-read. But the row that
holds this record back is neither of them, and neither W7-55 nor
`RETIREMENT-20260811.md` mentions it:

**The factory-return-type subtype check this record says it added is applied on
ONE of the two provider paths.** `service_accepts_type`
(`native-builtins/src/service_loader.rs:1677`) has exactly one caller, at
`:1897`, inside the **iterator** path (`native_sl_iterator`), where a factory
whose return type is not a subtype of the service correctly raises
`ServiceConfigurationError`. The **stream** path (`native_sl_stream`, the
factory block at `:2401-2420`) calls `factory_return_type` and uses the result
only to build the wrapper — it never asks `service_accepts_type` and never
raises. So:

```java
ServiceLoader.load(Svc.class).iterator()   // illegal provider -> ServiceConfigurationError  (correct)
ServiceLoader.load(Svc.class).stream()     // illegal provider -> quietly handed out          (divergence)
```

**Why 44/44 does not see it.** The fixture's factory provider,
`FactoryGreeter.provider()`, returns `Greeter` — a *correct* subtype. The
vector exercises only the positive case, so the missing negative check on the
stream path cannot go red. This is the vacuous-green shape: a check that is
present on the path the test walks and absent on the path it does not.

**Not fixed here, and why.** The repair is to mirror the `:1881-1910` block into
the stream path, and that block is pinning-sensitive — it re-reads `sl` and the
return-type mirror through `read_native_pin` *after* the allocating
`factory_return_type` call, in a specific order that a comment at `:1886-1889`
spells out. Writing that blind, in a lane that cannot compile, is how an
uncompiled GC-pin edit ships. It also changes `Compatible`, which is
contractually frozen — permissible here because raising
`ServiceConfigurationError` is genuine HotSpot parity, but only with a
measurement behind it.

**The measurement that closes it.** Add a fourth provider to
`regression-suite/modules/cratonvm.jdkonly.svc` whose `provider()` returns a
type that is *not* a `Greeter`, assert `ServiceConfigurationError` from both
`iterator()` and `stream()` in `RJdkModule.moduleServices()`, then:

```
cratonvm --java-home "<jdk-25>" --module-path regression-suite/build-modules \
    --add-modules cratonvm.jdkonly.svc -cp regression-suite/build RJdkModule
```

on both arms, with a HotSpot control. **The `--module-path`/`--add-modules`
flags are required** — omitting them produces a harness error that has already
been misread once in this campaign as a VM defect.

**The item W7-55 argued about, for completeness.** The constructor-form
provider subtype check (`W6-2` §"Deliberately NOT done") is still absent —
confirmed, there is no `service_accepts_type` call on either constructor path
(`:2038`, `:2465` grant the reflective override and go straight to
`newInstance`). Its stated reason — *"this lane cannot measure that"* — is a
deferral for want of a measurement rather than a refusal on the merits, so it is
carried as a live row too, not written off. The measurement is the same census:
`isAssignableFrom` over the boot modules' `provides` clauses.

---

## 4. What this audit did not re-audit

The thirty moves in `RETIREMENT-20260811.md` were not re-opened. Its three
stale *kept-row reasons* (W4-1, W4-2, W6-8) were corrected in place by W7-55 and
are unchanged here.

The four records this directory reserves for other running lanes
(`W4-4`, `W6-5`, `W7-41`, `W7-43`) were not touched.
