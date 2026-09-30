# What gen r5w6/oomjit10 left of the JIT OOME-retention family

> **STATUS (2026-09-29, gce e1/x): KEEP -- unchanged; no failing probe line of its own.** **Remaining:** items 1 and 5 (item 5 in `vm/src/jit/xt_root_scan.rs`).

> **STATUS (2026-09-29, gce e1/f): OPEN for items 1 and 5, unchanged in code;
> item 2's type-conflict half is fixed elsewhere.** `NativeGrowthReclaimProbe`,
> the one probe this page's STATUS named, is NOT item 1: the census on the
> base names a single-pass `java-local` home and a residue
> `deopt-saved-gpr-image` block (both dropped now, see
> `gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`), not
> a `caller=native claim=Keep` image. Item 1 still has no probe that fails
> because of it, and its fix direction (Rust callers that root what they keep
> in registers) is a per-call-site design item. Item 5 (frozen peers scanned
> conservatively, `vm/src/jit/xt_root_scan.rs`) is untouched here: that file
> is being changed by another lane this wave; the direction stays "scan a
> frozen peer's published chain with `scan_one_compiled_frame_with_layout`",
> which would now also give peers the two new band claims. **Run:** none.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- OPEN for items 1 and 5 only; still no failing probe line of its own.** The OOME probes that fail on d7 (`Gcd1PinnedCalleeOomeProbe`, `Gcd1ThreadExitSpillProbe`, `NativeGrowthReclaimProbe`) name the true-root fallback, not a Rust-owned callee-saved image (`caller=native claim=Keep`), in the census runs that exist (pinned-callee and thread-exit).

> **STATUS (2026-09-27, gcd d3/o, family consolidation): OPEN for items 1
> and 5 only; no probe line of this page fails today.** Each item now has
> exactly one home:
>
> | Item | Home | Failing probe line |
> |---|---|---|
> | 1 Rust-owned callee-saved image (`caller=native claim=Keep`) | HERE | none measured |
> | 2 single-pass locals liveness-blind | `gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md` (the fix) and item 5 of `gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927.md` (the defect) | none |
> | 3 allocation GC points not armed | item 1 of the oomjit9 page | none |
> | 4 `keptOomeCatch` / `listCatch` / `osrListOnceWithFirst` operand-spill | fixed: those cases PASS on the round-5 tip (orchestrator) | -- |
> | 5 frozen in-JIT peers scanned conservatively | HERE | none (`oome-thread-exit` is the livelock page's) |
> | 6 `GenR4W4NativeStringOomProbe` line 77 | the umbrella `../../internal/gc/gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2-FIXED-20260928.md`: gcd d3/o reads it as the NATIVE door under a latched streak and landed an opt-in lever there (`CRATONVM_GC_NATIVE_LATCHED_MAJOR=1`, `vm/src/vm/vm_exec.rs`, `safe_native_call_impl`) | the umbrella's |
>
> No code change on this page's items. Retire when items 1 and 5 either get
> a probe that fails because of them and a fix, or are judged not worth one
> (they cost nothing today). **Run:** none of its own.

> **Earlier status (2026-09-27, gcd d2/f): items 1, 2, 3 and 5 OPEN and unchanged in
> code; items 4 and 6 as the d1/b block below says. One census improvement
> and one lever bear on items 2 and 5.** Re-read on `a1fa77603`.
>
> - **Item 2 (single-pass liveness-blind frames):** the java-local band is
>   unchanged. Its operand-spill half now has the opt-in lever
>   `CRATONVM_JIT_SP_ZERO_SPILL_BAND=1` (`jit/src/x64/frames.rs`,
>   `Compiler::emit_prologue`). It removes a RETURNED frame's leftovers but
>   not this frame's own dead locals. The local-liveness proposal is still
>   the fix.
> - **Items 4 and 5 (shadow-stack holders):** the census label
>   `jit-shadow-stack` is now split into `jit-shadow-stack-indirect` (an IR
>   frame block's home), `jit-shadow-stack-value` (a pushed copy) and
>   `jit-shadow-stack-indirect-below-sp` (a leaked frame block: file it at
>   once). The code is in `vm/src/jit/helpers.rs`,
>   `oldmark_census_label_frames`.
> - **Items 1, 3:** unchanged. On item 3, `Op::New` / `Op::NewArray` are
>   pinned to a control anchor exactly like `Op::Call`
>   (`jit/src/ir_schedule.rs`, `pinned_anchor`), so the argument
>   `dead_ref_clear_site_for` makes for a call ("its bci is its program
>   point") carries over. What is still owed is the price of the stores in
>   allocation loops. It was not taken this wave because no probe collects
>   at an allocation with a dead home.
>
> **Run (orchestrator):** as the umbrella page. Nothing on this page retires
> this wave.

> **Earlier status (2026-09-27, gcd d1/b): item 6 NARROWED (most likely no holder at
> all; fix landed, default on); item 4 re-read against the round-5 tip; items
> 1, 2, 3, 5 OPEN, unchanged.**
>
> - **Item 6 (`GenR4W4NativeStringOomProbe`, no holder named):** the
>   escape at line 77 is the JIT allocation helpers answering a LATCHED
>   GC-overhead streak with an immediate `OutOfMemoryError` when no
>   `SoftReference` is alive -- no major, while `made`'s dropped contents sit
>   in the old generation. The interpreter runs a major first (gen r4w4
>   fixed exactly this `println` for `--nojit`). Landed in
>   `vm/src/jit/helpers.rs` (`jit_latched_overhead_limit_throws`,
>   `jit_overhead_limit_major`, `jit_overhead_limit_verdict`); switch
>   `CRATONVM_GC_OVERHEAD_PROGRESS=0`. Probe on the umbrella page. If it still
>   fails, the census now names the holder (auto markers, and a band word's
>   ` tier= sp= live_hi= in_map=` tail).
> - **Item 4:** the `keptOomeCatch` / `listCatch` / `osrListOnceWithFirst`
>   cases PASS on the round-5 tip (umbrella tables), which confirms this
>   page's reading: those were fill-time holders of data still live.
> - **Items 1, 2, 3, 5:** unchanged; item 2's single-pass half is also the
>   likelier reading of the OsrDeadSlot (c) holder
>   (`../../internal/gc/gengc-r5w3-live7-osr-dead-slot-holder-is-outside-the-osr-frame-FIXED-20260928.md`,
>   whose census now says the tier).
> - **New, related:** the `oome-thread-exit` hang the orchestrator measured on
>   `6d39e8dcc` is not item 5's holder but a livelock:
>   `../../internal/gc/gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles-FIXED-20260928.md`.

*Filed 2026-09-27 by gen round 5 wave 6, lane `oomjit10`, from reading (no
cargo in the lane). The companion of the fixes listed on the umbrella page
`../../internal/gc/gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2-FIXED-20260928.md`:
the holders the wave-5 staging census (`fbcb272ab`) named that this lane did
NOT fix, why, and how to tell each apart on the next census run.*

- **Severity:** retention under the JIT (dropped data stays reachable; the
  next allocation can fail). No item here is a corruption: every one is a word
  that is KEPT.
- **Owner:** the JIT lane (`jit/src/**`, `vm/src/jit/**`) unless an item says
  otherwise.

## 1. A callee's image of a register the VM's Rust code owns

`caller_register_claim` (`vm/src/jit/conservative_roots.rs`) decides a
`callee-saved-gpr-image` word by the COMPILED frame that owns the register. When
the chain reaches a return address outside compiled code -- the Rust side of
the interpreter->JIT boundary (`JitEntryGuard` callers, the dispatch helpers,
the OSR trampoline's Rust caller) -- it keeps the word. That is required, not
timid: Rust code holds raw object pointers in callee-saved registers across a
call into compiled code and uses them after it returns (that is why
`remap_register_image_words` rewrites exactly these words after a move), and
nothing else roots such a pointer. So a dead reference a Rust frame happens to
keep in RBX is still a root through the first compiled frame below it.

**Which census line is this:** a holder with `region=callee-saved-gpr-image`
whose `prov=` now ends `caller=native claim=Keep` (or `caller=jit:<m>
claim=Keep` whose chain ends in Rust). `GenR5W3OsrHolderProbe`'s
`check off=112` is this item if its `prov=` says `native`: it means the
checking call came from the interpreter (the case method's tail ran
interpreted), not from the OSR body.

**Fix direction (not small):** the Rust call sites that enter compiled code
would have to stop carrying references in registers across the call -- i.e.
root every temporary they keep across it (a handle / the frame they came
from) -- after which the boundary's image could be claimed dead like a
compiled owner's. Until then this item is closed only per call site.

## 2. The single-pass tier's locals are liveness-blind

Item 5 of `gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927.md`, unchanged:
a single-pass frame's precise map names every local the TYPE dataflow calls a
reference (`local_oop_masks`), and the band scan reads the whole java-local
band; neither asks bytecode liveness. A single-pass OSR body of
`NativeGrowthReclaimProbe.main` (`CRATONVM_JIT_OSR_OPTIMIZING=0`, or a method
the optimizing tier refuses) keeps the dead `s` / `cs` exactly as the IR tier
did before this wave. Design and the moving-young argument it needs:
`gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md`.

**Census line:** `region=java-local` at a single-pass frame. (Only that tier
has a per-safepoint blind spill, so a frame whose `safepoint-gpr-spill-image`
words print ` reg=... mask=...` is single-pass; the optimizing tier's
`region_name` has no such region.)

## 3. Allocation GC points are not armed (unchanged)

Item 1 of the oomjit9 residuals page. A collection reached from an inline
allocation's slow path sees the optimizing frame's homes as its last CALL left
them. None of this wave's probe shapes collects there (every checked GC point
is a call), so it was not taken; the keep set this wave built
(`snapshot_colours_live_after_each_call`) now answers for every scheduled
`Op::Call` only, and extending `dead_ref_clear_site_for` to `Op::New` /
`Op::NewArray` would reuse it unchanged (the allocation node needs an entry in
`after_kept`: add `is_call || matches!(op, Op::New { .. } | Op::NewArray { .. })`
in the step builder). Price the stores in allocation loops first.

## 4. The `region=operand-spill` holders of frames that THREW

The census listed `GenR4W6JitOomRootProbe.keptOomeCatch off=64`,
`osrListOnceWithFirst off=72` and `listCatch off=88`, all
`region=operand-spill`, all through `jit-shadow-stack`. Each is a frame that
filled the heap and caught the `OutOfMemoryError`. By reading, the frame is
live at a collection where the marker is still LEGITIMATELY reachable -- the
collections inside `fillList` / `listData.add` / `osrData.add`, where the list
is the static the loop is appending to -- and the holder census prints the
holders of every staged marker at every major, whether or not it should be
dead there. The failing shapes (`catch-inline-receiver`,
`oome-throwable-kept`, `oome-osr-loop`) also list `cleared ... off=192
region=safepoint-gpr-spill-image` for their markers, which the RAX fix
(`CRATONVM_JIT_RAX_DEAD_AT_CALL`) addresses.

**How to tell:** on the next census, look only at the majors AFTER the
program dropped the data -- the ones inside `cleared(ref)`'s `System.gc()`.
The census line of such a major follows a `[holder-census] major #N` whose
markers are the case's; a holder at a frame of the method that is CALLING
`cleared` (or of `cleared` itself) is a real one, a holder at a frame that
cannot be on the stack then (`keptOomeCatch` after it returned) is a
provenance record of another scan (`gen_heap_oldmark_census.rs` now documents
that `prov=` may come from any scan since the collection's reset).

## 5. `xt frozen in-JIT peers (conservative)/jit-frame-scan` (`Node`)

A peer thread frozen while in compiled code is scanned conservatively
(`vm/src/jit/xt_root_scan.rs`). In `GenR4W6JitOomRootProbe` the only peers
that hold `Node`s are `oome-thread-exit`'s two fillers while they are still
linking -- i.e. while the chain is live -- and that case PASSES. Nothing was
changed. A precise peer scan (the peer's own band with its layout and
claims, as the collecting thread's own frames get) is the direction if this
ever holds dead data: `scan_one_compiled_frame_with_layout` is reusable for a
frozen peer whose chain is published.

## 6. `GenR4W4NativeStringOomProbe` with the JIT: holder not yet named

The lane had no census of it. Candidates, cheapest first: the optimizing
frames' `Prim`-colour leftovers (now dropped, `CRATONVM_GC_IR_PRIM_SLOT_ROOTS`),
the interpreter frame's stale operand-stack slots after the two OSR exits
(`interp-local-*` labels; not this lane's code), a Rust-owned register image
(item 1). **Verify:**

```
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JIT_ROOTSCAN=1 \
  timeout 600 cratonvm $P GenR4W4NativeStringOomProbe 2>census.log
grep -n '^\[holder-census\]' census.log | tail -40
```

The census now explains the four largest root-reached old objects when no
`WeakReference` marker is staged (`marker#k ... (auto: ...)`), so the last
major before the uncaught `OutOfMemoryError` names the holders directly.
