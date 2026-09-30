# After a compiled callee's `OutOfMemoryError` is caught, the heap stays full (`Gcd1PinnedCalleeOomeProbe`, DEFAULT arm)

> **STATUS (2026-09-29, gce ve2): OPEN -- 0/8 on base and on e2 under host load (A/B in ve2-verdicts addendum); pre-existing, no escape line, so retention, not a door.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/o): OPEN -- one more candidate FIXED IN CODE, and the decisive rows.** e1 ran Gen 3/3 (base 2/3). The escape path of e1/o's block (an `OutOfMemoryError` escaping `chainCatch`'s checks) has a concrete mechanism now: the latched GC-overhead exit threw without trying the allocation its deciding major had just made room for (`gce-e2o-latched-overhead-exit-throws-without-an-attempt-FIXED-20260929.md`, fixed for the interpreter and native doors, JIT half a cross-lane hunk) -- `cleared()`'s `System.gc()` path and `usable()`'s arrays both meet a latched streak right after the catch. **Rows (Generational, e2 binary, keep stderr):**
> ```
> for r in 1 2 3 4 5 6; do p pcallee_d_$r 300 "CRATONVM_GC_STATS=1 CRATONVM_DBG_GC_OVERHEAD=1" "$G -Xmx64m" Gcd1PinnedCalleeOomeProbe; done
> ```
> **Pass:** stdout = HS (`callee-oome caught=true` / `cleared=true` / `usable=true` / `PASS`) 6/6. **On a failing row** classify from its stderr: an `[probe] OutOfMemoryError escaped chainCatch: phase=...` line = a door (send the stack and the `[GC] oome_ladder:` line, `ladder_latched_grace*`, to lane o); no such line = retention (the census run of the d7/u block below). **Retire when** 6/6 = HS with no escape line.

> **STATUS (2026-09-29, gce e1/x): KEEP -- 3/3 on e1, but nothing changed on the path and the rate is a flake's.** `gen_pcallee_1..3` (`verify-e1/ve1`, Generational `-Xmx64m`) = HotSpot 3/3 on e1; base 2/3, and the base's triage controls 2/3 (`opc_pcallee_ctl`) and 0/1 (`gen_ru1_pcallee`). e1/o changed only the probe's stderr. `oldsz_true_root_fallbacks` and the new `escaped chainCatch` line are not in the retained output. G1 (`cleared=false`) and ZGC (stdout stops after `caught=true`) fail 3/3 on both binaries; those halves are out of scope. **Remaining:** e1/o's 6-run diagnostic with stderr; retire at 3/3 with `oldsz_true_root_fallbacks=0` once a failing run is classified (retention or an escaping OOME).

> **STATUS (2026-09-29, gce e1/o): OPEN, NARROWED by reading -- the failing line `cleared=false usable=false` is printed by TWO different paths, and the probe now says which on stderr.**
> - `main`'s `catch (OutOfMemoryError escaped)` sets BOTH flags false. So `cleared=false usable=false` is either retention (the chain survived `cleared()`'s three `System.gc()`s and `usable()` then failed its own allocation, caught inside it) or an `OutOfMemoryError` that ESCAPED `chainCatch` -- which can only come from its handler or from `cleared()` (`System.gc()`, `ref.get()`), since `usable()` catches its own. The escape shape is an allocation-door verdict (this lane), not retention, and would explain a failing run whose true-root majors (15, 0 fallbacks) found no holder.
> - **Landed:** `tools/bench/Gcd1PinnedCalleeOomeProbe.java` records the check in progress (`phase`) and, on the escape path only, prints `[probe] OutOfMemoryError escaped chainCatch: phase=<grow|cleared> caught=<bool> message=...` and the stack trace to STDERR. Stdout is byte-for-byte unchanged (HotSpot's four lines).
> - **Run (orchestrator, Linux release):** `for i in $(seq 6); do CRATONVM_DBG_GC_OVERHEAD=1 timeout 300 cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench Gcd1PinnedCalleeOomeProbe 2>pc.$i.err; echo rc=$?; grep -h 'escaped chainCatch' pc.$i.err; done`. Expected: HotSpot's four lines; on a failing run, NO `escaped chainCatch` line means retention (next step the census as below); a `phase=cleared` line means the error came from `System.gc()`'s native path or the handler's deopt -- send the stack to this lane with the `[GC_OVERHEAD]` tail.

> **STATUS (2026-09-28, orchestrator, wave d9 verification): NARROWED, still OPEN; Generational-only, parked (the user closed Generational improvement work).** Linux release, round branch `bd19227a3` (wave d9) side by side with its base `d84029110` under the same load: `Gcd1PinnedCalleeOomeProbe -Xmx64m` 2/3 (`pcallee_1..3`; base 1/3). The failing run printed `cleared=false usable=false` with `oldsz_true_root_majors=15` and every true-root fallback reason 0 (`oldsz_true_root_fallbacks=0`), so d9/a's seed ran and the remaining holder is NOT the legacy young seed. Next step if reopened: the census on a failing run (`CRATONVM_DBG=gc-stats,oldmark-root-census,root-source`).

> **STATUS (2026-09-28, gcd d9/a): FIX LANDED for the named holder (the true-root major's fallback to the legacy young seed), default on; awaiting the probe.**
>
> - **The holder (d8/x, `307f0c6a2`):** 0 of 12 runs passed; every run had `oldsz_true_root_fallbacks` 13-16 against `oldsz_true_root_majors` 3-5, and the post-catch majors of `pcallee_census_1` named `holder#1 ... cat=seed/promotion-dest` and "young from-space reaches it (the major seeds from all of it)": the requested majors (the native funnel's `native-after-oome-major`, `jit-oome-second-major`) promoted objects in their young phase, so they fell back to the legacy seed, which roots every promotion destination and every young survivor.
> - **Landed (gcd d9/a, `gc/src/gen_heap.rs`):** a promoting requested major keeps the true-root seed. `sweep_old_gen_non_moving_impl` hands the pause's promotions to `old_gen_gc_inner` (`TrueRootAsk`); `TrueRootYoung::seed_word` resolves a pre-promotion young address to its destination (`promoted_index`), the root loop and the census skip the appended destinations (`promo_tail`), `TrueRootYoung::round` follows the `mirror_pin` / `metadata_pin` rows keyed by a marked destination's source, and a destination nothing reaches is freed and its source dropped from the returned pointer map (`TRUE_ROOT_SWEEP_OUT`, `run_non_moving_young_cycle`). A planned pinned compaction keeps the seed too (`seed_stretches` pins what a young stretch word names; `OldGen::compact_around_pins_watching` reports the dropped destinations). Every remaining fallback is counted per reason (`OldGen::note_true_root_fallback`, `oldsz_true_root_fb_<reason>` once lane b prints them; `RUST_LOG=cratonvm::gc=info` prints `reason=` today). `=0` switch: `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0` (restores d8 exactly); `CRATONVM_GC_FULL_GC_TRUE_ROOTS=0` turns the whole seed off.
> - **Unit tests:** `cargo test -j 5 -p cratonvm-gc --lib gcd_d9a_` -> 8 passed (6 in `gen_heap::tests`, 2 in `old_gen::tests`); `cargo test -j 5 -p cratonvm-gc --lib gcd_d4n_` and `gcd_d5r_` unchanged; `a_promote_pressure_arm_is_honoured_by_the_non_moving_sweep_that_follows_it` (its promoted survivor now goes through the resolution) unchanged.
> - **Run (orchestrator, Linux release, JIT on):**
>   ```
>   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
>   for i in 1 2 3; do CRATONVM_DBG=gc-stats timeout 300 cratonvm $P Gcd1PinnedCalleeOomeProbe 2>pc.$i.err; echo rc=$?; grep -h 'oldsz_true_root' pc.$i.err | tr ' ' '\n' | grep true_root; done
>   for i in 1 2 3; do CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0 timeout 300 cratonvm $P Gcd1PinnedCalleeOomeProbe; echo "[wide=0] rc=$?"; done
>   ```
>   Expected (HotSpot 25 `-XX:+UseSerialGC`, checked 2026-09-28): `callee-oome caught=true`, `callee-oome cleared=true`, `callee-oome usable=true`, `PASS`, rc 0 -- 3/3 on an idle host (the OOME probes lose about 1 in 6 under parallel host load on every build; a failure counts only if it also shows `oldsz_true_root_fallbacks>0` or a census holder that is not `seed/*`), with `oldsz_true_root_fallbacks=0` and `oldsz_true_root_majors` > 0 on every run. The `WIDE=0` arm is the control (d8: 0/3).
> - **Retire when** the default arm prints HotSpot's four lines 3/3 with `oldsz_true_root_fallbacks=0`; this page and `../../internal/gc/gcd-d4o-thread-exit-recovery-fails-without-an-exited-frame-holder-FIXED-20260928.md` retire together. If a run still fails with `fallbacks=0`, the census (`CRATONVM_DBG=gc-stats,oldmark-root-census,root-source`, the post-catch blocks now start with a `seed=true-roots` line) names the next holder.

> **STATUS (2026-09-28, gcd d7/u): OPEN. The census lines forwarded for the
> failing run do not name the post-catch holder, by reading; a diagnostic that
> will tell landed; no fix of a holder.**
>
> - **Measured (orchestrator, d6 build `feaec464b`):** default arm 1/3;
>   `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES=0` 1/3;
>   `CRATONVM_JIT_OSR_DROP_ORPHANS=1` 0/3 (candidate 2, the orphaned
>   exceptional frame, is out). Failing-run census: `holder#1 ... cat=14 ...
>   prov="method=...grow... off=640 region=callee-saved-gpr-image tier=ir sp=1
>   live_hi=320 ..."` (6x) and `top#1 root[383] ... cat=2: Static fields --
>   all classes/jit-shadow-stack-indirect -> young ... Node old_bytes=32980560
>   young_bytes=16613856 prov="method=...grow..."` (4x).
> - **Why those lines are, by reading, PRE-catch:**
>   1. `cat=2: Static fields` reaching 33 MB old + 16 MB young of `Node`s is
>      `chainHead` itself, which `chainCatch`'s handler nulls; a major that
>      still finds it ran before the handler.
>   2. `prov=` is recorded only during the collection's own root scan and
>      cleared at the start of every collection
>      (`gc_quiescence::clear_pinned_jit_roots`), and the band scan reaches a
>      frame only through the precise walk of the thread's LIVE compiled
>      frames: a `grow` frame named there was on the stack at that
>      collection. The only collections with `grow` live are the ones inside
>      its failing allocation: `jit_new_object`'s forced young cycle, the
>      overhead-limit majors and `jit-oome-second-major` (one is in the
>      stderr), and the construction of the `OutOfMemoryError` itself. At all
>      of those the chain is legitimately reachable (`chainHead`, and `grow`'s
>      own `h`).
>   3. `region=callee-saved-gpr-image` with no `reg=`/`claim=` suffix is a
>      word of the IR frame's tail that is not one of its published GPR saves
>      (`CompiledMethod::ir_saved_gpr_slots`; on this backend the
>      callee-saved band spans the whole tail, including the 256-byte deopt
>      register image), read conservatively. Real, but held for a frame that
>      is live.
> - **What decides it:** the majors AFTER the catch (the native funnel's nine
>   `native-after-oome-major` collections run from the catch path on); their
>   `[holder-census] major #N` blocks are the LAST ones of the run. **New (gcd d7/u):** every band-word `prov=`
>   now ends `rbp=0x.. above_scan=0x.. returns_to=<caller>`
>   (`vm/src/jit/conservative_roots.rs::band_word_context`), so a `grow` frame
>   named after the catch would show which activation it is and whom it
>   returns to (a live one returns into `chainCatch` above the scanner; a
>   stale one would not be reachable by the walk at all).
> - **Likeliest post-catch cause, by elimination:** nothing roots the chain
>   and the requested majors cannot free it -- the true-root seed's fallback on
>   a promoting cycle (candidate 3, lane r's code), the same shape as
>   `../../internal/gc/gcd-d4o-thread-exit-recovery-fails-without-an-exited-frame-holder-FIXED-20260928.md`.
>   A post-catch major block that names NO root holder for the marker (or only
>   `young from-space reaches it`) with `oldsz_true_root_fallbacks > 0` confirms
>   it.
>
> **Run (orchestrator):**
> ```
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
> CRATONVM_DBG=gc-stats,oldmark-root-census,root-source \
>   timeout 600 cratonvm $P Gcd1PinnedCalleeOomeProbe 2>pco.log
> # the run ends in `main`'s uncaught OOME, so the LAST majors are post-catch
> # (a block whose holders still include the `Static fields` root is pre-catch):
> start=$(grep -n 'holder-census\] major' pco.log | tail -4 | head -1 | cut -d: -f1)
> tail -n +"$start" pco.log | grep -E 'holder-census\] major|holder#|young from-space|marker#' | head -60
> grep -h 'oldsz_true_root' pco.log
> ```
> Expected (HotSpot): `callee-oome caught=true`, `callee-oome cleared=true`,
> `callee-oome usable=true`, `PASS`. On a failing run the post-catch blocks
> say which: a `holder#1` with a `prov=... returns_to=` naming a compiled frame
> (this lane: send it), a VM root category (the owner of that root), or no
> root holder with `oldsz_true_root_fallbacks > 0` (lane r).

> **Earlier status (2026-09-28, gcd d6/u): OPEN, filed by reading; holder NOT
> identified; one candidate fixed in the same wave (below). Owner: this lane
> (frames) until the census names a non-frame holder.**

- **Measured (orchestrator, d5 build `89532cb58`, `-Xmx64m`, JIT on),
  DEFAULT arm, 3/3:** `callee-oome caught=true`, then `main` dies of an
  uncaught `OutOfMemoryError: Java heap space`. HotSpot (`-XX:+UseSerialGC`):
  `callee-oome caught=true`, `callee-oome cleared=true`, `callee-oome
  usable=true`, `PASS`. stderr: `[GC] g1 gc-entry: forced by
  native-after-oome-major: 9`, `forced by jit-oome-second-major: 1`.
- The same shape (`GenR4W6JitOomRootProbe`'s `oome-compiled-callee`) fails
  only in arm C (2 of 3); its default arm passes 3/3.
- **Severity:** a wrong result (a spurious `OutOfMemoryError` after the
  program dropped everything). Generational with the JIT.

## What the output says

`caught=true` means `chainCatch`'s own handler ran (`caught` is set only
there), and `chainHead` was nulled. Nine native-funnel majors ran after it
(`native_call_owes_oome_major` takes one debt per raised heap OOME, so about
nine OOMEs were raised afterwards: `usable()`'s, the `println`s'). So after
`chainCatch` returned, the chain (about 60 MB of `Node`s) was still strongly
held, across majors, while `main` ran on. The holder outlives `chainCatch`'s
activation: it is `main`'s frame, a thread-level channel, or nepotism.

## Candidates, by reading, and what tells them apart

1. **A compiled frame word kept by a MEMORY-token range (fixed this wave).**
   The IR builder reuses a call as the next memory node's token, and the slot
   plan counted that as a read; a memory phi then carried `grow`'s returned
   chain head (the loop's `chainHead = grow(..)`) round `chainCatch`'s loop,
   so its home was never a dead-home clear candidate while the frame lived.
   `jit/src/ir_lower.rs::value_use_ranges` (gcd d6/u, default on) removes
   that. It only matters if `chainCatch` (or `main`'s OSR body) keeps running
   compiled after the catch. Arm: `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES=0`
   vs default.
2. **An orphaned exceptional frame** (`gengc-r4w5-oomjit5-...`). `main`'s
   warm-up loop is OSR'd into the optimizing tier and its `chainCatch(ref, -1)`
   call is inside `main`'s `try`, so that call site is protected. If `grow`
   or `chainCatch` published a reason-9 frame naming `grow`'s `from`/`h` (the
   chain) and the OSR sink re-stashed it as foreign, `LAST_EXCEPTIONAL` roots
   the chain until the next take on this thread. That is the shape the
   oomjit5 page has been waiting for ("a caller with a try around a callee
   that catches"). Arm: `CRATONVM_JIT_OSR_DROP_ORPHANS=1`; census label
   `deopt-stash`.
3. **Nepotism through the true-root seed's fallback** (lane r's code): the
   chain `grow` built in compiled code alternates young/old; a requested major
   whose young phase promoted anything seeds the old mark from every young
   survivor (`promo_seeds > 0` in `sweep_old_gen_non_moving_impl`). Read
   `oldsz_true_root_fallbacks` against `oldsz_true_root_majors`.
4. **A pin or pending slot left on the thread** (`native_pin_roots`,
   `native_pending_return`): by reading, `CompileArgPins` and
   `JitArgPinGuard` release on drop; census label `native-pin` would say
   otherwise.

## Run (orchestrator)

```
javac -d tools/bench tools/bench/Gcd1PinnedCalleeOomeProbe.java
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
for arm in "" "CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES=0" "CRATONVM_JIT_OSR_DROP_ORPHANS=1"; do
  for i in 1 2 3; do env $arm timeout 300 cratonvm $P Gcd1PinnedCalleeOomeProbe; echo "[$arm] rc=$?"; done
done
CRATONVM_DBG=gc-stats,oldmark-root-census,root-source timeout 600 cratonvm $P Gcd1PinnedCalleeOomeProbe 2>pco.log
grep -n 'holder#1\|oldsz_true_root\|dropped .* orphaned' pco.log | head -20
```

Expected (HotSpot's lines): `callee-oome caught=true`, `callee-oome
cleared=true`, `callee-oome usable=true`, `PASS`, rc 0.

- Default arm passes: candidate 1 (and the value-range arm `=0` should
  fail).
- Only the drop-orphans arm passes: candidate 2 (make the OSR sink's drop the
  default for a caught exception, or land the stash-floor proposal).
- The census `holder#1` otherwise names the owner: a `deopt-stash` label = 2,
  a `jit-...` / `method=` provenance = a compiled frame (this lane), no root
  provenance with `oldsz_true_root_fallbacks > 0` = 3 (lane r).
