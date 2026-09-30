# The young pin ledger excused compiled-frame words on deadness claims that the marking scan does not trust

> **STATUS (2026-09-29, gce e1/x): KEEP -- the flag-on netty loop was not run.** **Remaining:** the netty 3-class x 10 loop with the pinned young copy on.

> **STATUS (2026-09-29, gce e1/y, review only): unchanged -- FIXED IN CODE, awaiting the flag-on netty loop.** Nothing this round changed touches the ledger (`vm/src/jit/conservative_roots.rs`) or its consumers (`pause_young_pin_read`, `build_pinned_young_plan`). This lane's changes on the young path are behaviour-identical on the pinned copy's cycles (the mark and zeroing moved to the heap's pool: the non-moving sweep's, and the pinned rebuild still zeroes serially-or-scoped as before; the card bulk re-mark skips only repeats). Gate unchanged: the two `cratonvm-vm` unit tests, then the netty 3-class x 10 loop with `CRATONVM_GEN_PINNED_YOUNG_COPY=1` (30/30 rc 0, `moving-pinned-pages>0`) and arm D with `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1` (30/30).

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- FIXED IN CODE, awaiting the flag-on netty loop.** The two unit tests pass in the round's Windows suite. The d7 host run of the netty suites (`netty-d7-off`) is the FLAG-OFF arm only (four `io.netty.buffer` classes x 3, all rc 0, full pass counts), which cannot judge this page. **Remaining gate unchanged:** the 3-class x 10 loop with `CRATONVM_GEN_PINNED_YOUNG_COPY=1` 30/30 rc 0 with `moving-pinned-pages>0`, and arm D (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`) 30/30.

> **STATUS (2026-09-28, gcd d5/s): FIXED IN CODE (verified by reading
> `d916d1c40`), awaiting the netty loop, which the orchestrator is
> re-running on the d4 build. Retire on the gate below; nothing else is
> open on this page.**
>
> In the code as described: `vm/src/jit/conservative_roots.rs`
> `young_pin_slot_excuses(.., trust_deadness, ..)` and
> `young_pin_frame_words(.., trust_deadness)`; `young_pin_scan_own_chain`
> passes `!young_pin_ledger_licenses_moves()` to
> `young_pin_classify_compiled_frames`, so a ledger that licenses a move (the
> pinned copy, its take-over arm, option B) excuses only a base in a slot the
> active map names. The census-only ledger (both flags off) is unchanged.
>
> **Gate (retire on it):**
>
> ```bash
> cargo test -j 5 -p cratonvm-vm --lib a_ledger_that_licenses_moves_excuses_only_rewritten_slots
> cargo test -j 5 -p cratonvm-vm --lib young_pin_frame_screen_excludes_rewritten_slots_and_keeps_interior_words_raw
> ```
>
> both `1 passed`; then the netty 3-class x 10 loop (r5w4/pin8 STATUS of
> `gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md`) with
> `CRATONVM_GEN_PINNED_YOUNG_COPY=1`: 30/30 `rc=0`, and
> `moving-pinned-pages` above zero under `CRATONVM_DBG=gc-stats`.
>
> *Cost noted by gcd d5/s:* the "objects that share a pinned page are
> retained for one more cycle" line below is one of the two ingredients of
> the flag-on retention on
> `../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md`
> (more pinned pages, more young islands in a promoted chain). This fix stays
> right; the retention is fixed on that page, not by trusting deadness
> again. See also `gcd-d5s-proposal-pinned-copy-roots-by-word-not-by-page-20260928.md`.
>
> **Previous STATUS (2026-09-26, gen r5w4/pin8): FIXED IN CODE, awaiting the netty
> matrix.**
>
> What landed:
>
> - `young_pin_slot_excuses` / `young_pin_frame_words` take a
>   `trust_deadness` switch.
> - `young_pin_scan_own_chain` passes `!young_pin_ledger_licenses_moves()`.
>
> Whenever the ledger licenses a relocating cycle
> (`CRATONVM_GEN_PINNED_YOUNG_COPY` or `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`),
> only a base in a slot the active oop map NAMES is excused. That is the one
> slot `remap_one_jit_frame` rewrites. Every other young word of a classified
> compiled frame pins its page. The census-only ledger (both flags off) is
> unchanged.
>
> **Verify (unit):**
>
> ```bash
> cargo test -j 5 -p cratonvm-vm --lib a_ledger_that_licenses_moves_excuses_only_rewritten_slots
> cargo test -j 5 -p cratonvm-vm --lib young_pin_frame_screen_excludes_rewritten_slots_and_keeps_interior_words_raw
> ```
>
> Both must pass.
>
> **Verify (runtime):** the netty 3-class x 10 loop in the r5w4/pin8 STATUS of
> `gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md`. It must
> print 30/30 `rc=0`, and `moving-pinned-pages` must stay above zero under
> `CRATONVM_DBG=gc-stats`.
>
> Retire this page when that loop is green.

*Filed 2026-09-26 by gen round 5, wave 4, lane `pin8`.*

- **Severity:** memory corruption, with `CRATONVM_GEN_PINNED_YOUNG_COPY=1`
  or option B (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`) only. It is a
  candidate cause of the flag-on netty SIGSEGVs in
  `../../internal/gc/generational-bytebuf-suite-sigsegv-hashbrown-rehash-FIXED-20260928.md`.
- **Fits the orchestrator's split on `897210f83`:** the crash reproduces
  under BOTH flags (BigEndianDirect `+6` under each; BigEndianHeap
  `0x100000004` under option B) and under neither without them. The two
  flags share nothing but this ledger. Under option B a dead-claimed word
  left the ledger EMPTY, so term 4 stood down and a Cheney copy moved the
  word's object.
- **Code:** `vm/src/jit/conservative_roots.rs`:
  - `young_pin_slot_excuses`;
  - `young_pin_frame_words`;
  - `young_pin_classify_compiled_frames`;
  - `young_pin_scan_own_chain`.

## What was wrong

A pinned in-place cycle may move a young object only if every word that names
it is either rewritten after the move or pins its page. The pause's ledger
collects the words that must pin. For a compiled frame whose layout resolved,
it EXCUSED three kinds of word:

| excuse | rewritten after a move? | read as a marking root? |
|---|---|---|
| a base in a slot the active map names | yes (`remap_one_jit_frame`) | yes |
| a base in a Java-local / operand-spill slot the map does NOT name ("dead by the map") | **no** | **yes** (`scan_one_frame_filtered` roots every modelled word) |
| an operand-spill word above the safepoint's live cursor | **no** | no (dead-spill claim) |

The last two rows rest on liveness claims: the oop map's, and the spill
cursor's. The collector itself does not rely on the first claim when it
marks. It keeps the word's object alive because the word MAY still be live.

Under the non-moving sweep a wrong claim costs little: the object is marked
through the word and never moves. At worst, if the claim is wrong and the
word was the ONLY reference, the object is freed.

Under the pinned copy a wrong claim costs much more. The object moves even
while it stays reachable through other references, and the frame slot keeps
its old address. The copy then zeroes the vacated bytes and re-serves them in
the same pause. The compiled code resumes on whatever the allocator puts
there next. For netty that is a `ByteBuf` whose fields drive `Unsafe` stores
into native memory, which is where the allocator metadata behind the crash's
victim lives.

The known ways such a claim is wrong, all measured or documented elsewhere:

- `GenR5W2OsrDeadSlotProbe` fails in every JIT arm.
- The oop-map oracle counts `WRONG_MAP` hits (the sp-id selects a map that
  omits a live slot) and `NEVER_MAPPED` hits.
- A `frame_holds_no_references` frame makes every modelled word "dead by the
  map".

The pinned copy is the first mode that spends these claims on almost every
JIT-warm cycle. By default term 4 made those cycles non-moving, so the claims
were never spent at scale.

## Fix

When the ledger licenses moves it trusts no deadness claim. The rule it
applies is the one the page's own principle states: a word is excused only
if a channel rewrites it. The rewritten set is exactly "a base in a slot the
active map names", and that is what `remap_one_jit_frame` rewrites
(value-gated, the same map selection).

## Cost, and what to watch

More words pin pages. That has three effects:

- the 1/8 over-bound divert fires more often (`nonmoving-pinned-pages-over-bound`);
- the 256-word per-thread deposit may overflow into
  `nonmoving-pin-ledger-incomplete`;
- objects that share a pinned page are retained for one more cycle.

Measure `GenR4W4JitWarmDivertProbe`'s `moving-pinned-pages` against the
129-of-129 reference. If the price is too high, see
`../../internal/gc/gengc-r5w4-pin8-proposal-exact-pins-for-dead-claimed-frame-words-REJECTED-20260928.md`.

## A/B on the base binary

`CRATONVM_GC_NO_BAND_MAP_LIVENESS=1 CRATONVM_GC_DEAD_SPILL_ROOTS=0`
approximates this fix on `897210f83`. It removes both excuses, but it also
changes what marking and the coverage proof read, so it is a lead, not a
proof.
