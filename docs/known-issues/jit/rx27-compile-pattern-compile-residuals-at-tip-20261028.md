# rx27 compile: what is left on the `compile` row outside the type checks

Status: OPEN. Area: JIT tiering and splicing of `java.util.regex.Pattern`'s compile path. Severity: perf; the
`compile` row is 5.6k instructions per op against HotSpot 25's 810 (6.9x, round 25), and `split-ws` /
`regex-split` pay the same compile per op. Filed 2026-10-28 by round rx27, lane compile, by reading
`df86fe90ba` (a proposal: it changes no code, so there is no fix to measure). Parent: `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`.

The lane's own item, the lambda-proxy type checks, is on the parent page ("Round rx27 wave 1"). These are the
other residuals the r23-w5 waves 9b-12 named, as they stand at tip.

## Wave 3 (2026-10-28, lane compile; by reading `jit-regex-r27` `dc5f532718`; pending build)

Measured state (orchestrator, rx27b2): `compile` 4.36k instructions per op against HotSpot's 803 (5.4x);
`CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS=1 CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS=1` read 4.12k (0.95x, inside the
15 % bar, kept opt-in). The 3x gate needs about 2.4k, so 1.9k per op has to go.

### What one op does (JDK 25 source, `Pattern.compile("\\s+")`)

Calls, in order: the row lambda -> `Pattern.compile(String)` -> `Pattern.<init>(String,int)` (a
`catch (StackOverflowError)` row) -> `compile()` -> `expr` -> `sequence` -> `nextEscaped` / `unread` -> `atom`
-> `escape` -> `CharPredicates.ASCII_SPACE()` (a constant lambda) -> `newCharProperty` -> `closure` -> `curly`
-> `qtype` -> `BmpCharPropertyGreedy.<init>` -> `CharPropertyGreedy.<init>`; back in `compile()`:
`new Start(matchRoot)` -> `TreeInfo.<init>` -> `next.study(info)` (`CharPropertyGreedy.study`) ->
`next.study(info)` (`Node.study` on the shared `lastAccept`); then the empty `topClosureNodes` iterator. The
small helpers (`peek`, `next`, `has`, `skip`, `unread`) run several times each.

Allocations (11): `Pattern`, `int[5]` (`temp`), `int[32]` (`buffer`), `GroupHead[10]`, `ArrayList` and its
`Object[10]`, `BmpCharProperty`, `BmpCharPropertyGreedy`, `Start`, `TreeInfo`, the `ArrayList$Itr`.
HotSpot allocates the first nine too (they escape into the `Pattern` or are written to its fields); it removes
`TreeInfo` and the iterator by escape analysis once `Start.<init>`'s `study` chain and the `for` loop inline.
Here the `study` chain stays calls (item 1), so `TreeInfo` escapes into them and is a real allocation.

Class tests (6): `p instanceof BmpCharPredicate` (a lambda proxy: the r25 proxy ways answer it inline after the
first call; its `(BmpCharPredicate) p` is dropped by `CRATONVM_JIT_IR_CAST_AFTER_INSTANCEOF`),
`prev instanceof BmpCharProperty` (the exact class: one compare), `(BmpCharProperty) prev` (dominated, dropped),
`matchRoot instanceof Slice` (non-final; a display miss for the hierarchy-only `BmpCharPropertyGreedy`),
`instanceof Begin` / `instanceof First` (final user classes: the final-miss arm). None of them should reach a
helper in steady state; the `CRATONVM_DBG_TYPECHECK_FILTER` census below says whether one does.

So the per-op gap is not one dominant item that the type-check lane owns. HotSpot pays the same ~15 calls
(`compile`, `sequence`, `atom` and `escape` are over `FreqInlineSize`) at a few instructions each; CratonVM pays
an optimizing-tier frame per call (prologue, fresh-frame clear, shadow-frame publication, entry poll) and a
helper-backed or inline TLAB allocation per object. Round 25's frame study measured the clear at 0.4-4.6 % of
the regex rows. It did not measure `compile`, whose root `compile()V` is a 110 KB body with about 64 splices
and is entered once per op. Its frame clear is the first number to read (below).

### Wave 3 code (this lane)

- `CRATONVM_JIT_SP_INSTANCEOF_PROXY_WAYS` (opt-in): the single-pass `instanceof` gets the proxy ways (page
  `rx27-compile-single-pass-typechecks-have-no-proxy-ways-20261028.md`). It matters to this row only if a
  `Pattern` body that tests a proxy runs single-pass in steady state. On `regex-split`,
  `Pattern$BmpCharProperty.study [c1]` is the one single-pass body left there, and it does no proxy test. So
  expect no row to move; it closes the tier gap.

### One host run that names the remaining cost (`cd /root/rx27/p`, `B=/root/r25-bin/cvm-<tag>`)

```
# 1. tiers: every Pattern body published, and which stayed single-pass
CRATONVM_JIT_PERF_MAP=1 CRATONVM_DBG_JITC=1 $B --jdk-only -cp . R24RegexSplitRows compile 1 3 2> c.txt
grep -E '\[ir\] (admission|acceptance) java/util/regex/(Pattern|CharPredicates)' c.txt | sort | uniq -c
grep -cE 'c2-supersede published java/util/regex/Pattern.compile\(\)V' c.txt        # churn: want 1
grep -E 'stale-caller recompile requested.*Pattern' c.txt | sort | uniq -c
# 2. refused splices on the path
grep -E 'inline-resolve REFUSED java/util/regex/Pattern|ir-splice-refused.*java/util/regex/Pattern' c.txt \
  | sed 's/ depth=.*:/:/' | sort | uniq -c | sort -rn | head -30
grep -E 'ir-chain-tree .*(study|Start)' c.txt | sort | uniq -c
# 3. the per-entry frame clear of each Pattern body (instructions per entry; compile()V is entered once per op)
grep -E '\[ir-frame-clear\] method=java/util/regex/Pattern' c.txt | sort -u
# 4. type checks still reaching a helper (all targets in the regex package)
CRATONVM_DBG_TYPECHECK_FILTER=java/util/regex $B --jdk-only -cp . R24RegexSplitRows compile 1 3 2>&1 \
  | grep census | awk '{print $3, $5, $7}' | sort | uniq -c | sort -rn | head
```

Then a quiesced per-symbol profile (`R24RegexSplitRowsQ countq compile ...` with `perf record -p` on the 3N
window and `CRATONVM_JIT_PERF_MAP=1`, as in
`docs/internal/fixed-bugs/r25-w10-steady-state-single-pass-bodies-FIXED-20261027.md` "Closure"). Item 3's
`instrs=` times the entries per op is the clear's share. The per-symbol split says whether the remaining
1.9k is frames, allocation or `study`.

### Arms for `icount2.sh rx27c3 compile,split-ws,regex-split`

`hs:hs:` `def:<tag>:`
`pw:<tag>:CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS=1,CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS=1`
`pwsp:<tag>:CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS=1,CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS=1,CRATONVM_JIT_SP_INSTANCEOF_PROXY_WAYS=1`
`r25off:<tag>:CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS=0` (what the round-25 ways still buy on `compile`)
`clr0:<tag>:CRATONVM_JIT_IR_FRESH_CLEAR_COVER=0` (if the cover is refused on `compile()V`'s frame, the
run-wise clear is what runs; this arm shows the cover's share).

Facts: `tools/probes/jit/rx27/RX27ProxyTypecheckFacts.java` on `pwsp`, both modes, and again with
`CRATONVM_C2_SUPERSEDE=0` so its sites stay in their single-pass bodies (the arm this wave adds).

## Wave 3b (2026-10-28, lane compile; trace rx27b3, pending build): the `study` walk splices

**The trace** (orchestrator, `compile` row, `CRATONVM_DBG_JITC=1`):
- The frame clear is small: 14-18 words, 7-9 instructions per entry on the study bodies, 25 on
  `compile()V`.
- The lead is `Pattern$Node.study`. It is refused at every depth 1-6 (~2000 lines) with
  `chain-link-keeps-spliced-call ... next-link call at callee_pc=12 is spliced (class 576) in this body's own
  compile`.

**Mechanism.** `Node.study` is `return next != null ? next.study(info) : info.deterministic`. Its own artifact
guard-splices the next link to class 576 (`Pattern$Node`, the `accept` node), whose `study` is the same body.
The nested resolution of that next link is refused one level deeper for the same reason, down to the nest cap.
The W7-B fix-2 lost-link rule then refuses every parent in turn. `CharPropertyGreedy.study` (pc 41, class 577
`LastNode`) falls with it. Even a resolved walk would be dropped by the planner's loop-rooted rule (`unrooted`),
because `Start.<init>` runs it once per op. So each op pays `Start.<init>` -> `CharPropertyGreedy.study` ->
`Node.study` -> `Node.study` as calls, and `TreeInfo` escapes into them.

**Change: `CRATONVM_JIT_IR_ONCE_WALK_CHAIN_SPLICE` (opt-in).** Two predicates live in `jit::rx27_once_walk_chain`,
each asked at one guard:
- Resolver (`jit_bridge::resolve_inline_site_from`, the lost-link search): a kept next link whose artifact splice
  selects the same declaring body is not a loss (`next_link_is_self_walk`).
- Planner (`r25_w7b_chain_splice::strip_chain_rows`, the `unrooted` strip only): a short (<= 64 bytes) loop-free
  link at most 6 levels down stays (`unrooted_link_admitted`).

Functions touched: `resolve_inline_site_from` (one block in the W7-B lost-link `find_map`) and
`strip_chain_rows` (the `if !rooted` condition). The rest of the chain rules are as they were.

Engagement (`cd /root/rx27/p`, `B=/root/r25-bin/cvm-<tag>`):
```
CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_ONCE_WALK_CHAIN_SPLICE=1 $B --jdk-only -cp . R24RegexSplitRows compile 1 3 2> w.txt
grep -c 'once-walk-chain ADMITTED' w.txt                                  # > 0
grep -c 'chain-link-keeps-spliced-call.*Pattern\$Node.study' w.txt       # from ~2000 to near 0
grep -E 'ir-chain-tree .*(Start|study)' w.txt | sort | uniq -c            # kept>0 unrooted=0 on Start.<init>'s tree
grep -E 'acceptance java/util/regex/Pattern.compile\(\)V' w.txt          # still accepted
```
Arms: `icount2.sh rx27c3b compile,split-ws,regex-split,cached hs:hs: def:<tag>: walk:<tag>:CRATONVM_JIT_IR_ONCE_WALK_CHAIN_SPLICE=1
walkpw:<tag>:CRATONVM_JIT_IR_ONCE_WALK_CHAIN_SPLICE=1,CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS=1,CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS=1`.

Expected effect:
- `compile` loses three compiled calls per op (about 40 instructions each plus a 7-9 instruction clear). Once
  the walk is inline, escape analysis can drop `TreeInfo`. That is roughly 150-250 of the 1.9k gap. It is not
  the whole gap, so the remaining split needs the per-symbol profile above.
- `split-ws` and `regex-split` gain the same per compile.
- Watch `cached`, `matchonly` and `regex-groups`, whose match chains are rooted: the predicate only touches
  unrooted rows, so they should not move.

Batteries: `R24RegexSplitRows facts` and the regex facts probes in both modes; `regex-*` icount rows for the
other chains.

**`c2-supersede published Pattern.compile()V` = 2.** In this excerpt every Pattern method's admission and
acceptance lines appear twice. That includes bodies with no loop and no callee churn, such as
`TreeInfo.<init>` (C1). So the 2 is not shown to be churn of `compile()V` alone. It may be the W13-2 `C1 -> C1`
duplicate recompile (by design) followed by one C2, or two compile requests per method. Check per publication:
`grep -nE 'c2-supersede published java/util/regex/Pattern.compile\(\)V|stale-caller recompile requested .*Pattern.compile|refused-publication .*Pattern.compile|bg-compile C1 duplicate KEPT .*Pattern' w.txt`.
If the second publication follows a `stale-caller recompile`, the callee that republished is named on that
line. That is the rebind path (wave 10), not this lane. Nothing is changed for it here.

## 1. `Node.study` chain stays calls (owner: chain-splice rules, lane F)

`callee-unresolved` with a speculated class id means the VM's receiver-callee resolver declined the guarded
callee (`jit/src/lib.rs`, `InlineRefusal::CalleeUnresolved { speculated_class_id: Some(_) }`). For
`Node.study` under `Pattern.compile` that is now by design: W7-B round 2's loop-rooted rule
(`CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`, default ON) keeps a chain link only when a loop roots it, and the
compile path's `study` chain runs once per root entry (`ir-chain-tree ... kept=0 unrooted=1` on c11). Each link
is one call per op. Splicing it needs a rule for once-per-entry chains that does not cost the frame and budget
W7-B round 2 measured (`regex-split` +14 % when every chain was spliced). Confirm the count first:
`CRATONVM_DBG_JITC=1 ... R24RegexSplitRows compile 1 3 2>&1 | grep -E 'ir-chain-tree .*study|nest-chain .*study'`.

## 2. `Pattern.<init>(String,int)` tier

Its only String-family call sites outside the `Unknown flag` throw arm are `pattern.isEmpty()` and the
`compile()` call; the cold arm is skipped by `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS` (unset, it follows
`IR_INLINE_BYTE_ARRAYCOPY`, which follows the default-ON `IR_SPLICE_ARRAYCOPY_ROW_BOUND`). So it should no longer
be pinned single-pass. Wave 9b's note stands either way: the body is field stores and the `compile()` call (537
bytes, too large to splice), so the tier is worth at most one frame per op. Check:
`CRATONVM_DBG_JITC=1 ... compile 1 3 2>&1 | grep 'admission java/util/regex/Pattern.<init>'`.

## 3. `Pattern.compile()V` republication churn

Every switch rounds 23-25 built against it is default ON at tip: `PUBLISH_OVER_SUPERSEDED_CALLEE`,
`PIN_BAKED_CALLEES_WHILE_COMPILING`, `C2_PUBLICATION_REFUSAL_REFUND`, `STALE_CALL_REBIND_ABS64`,
`IR_DIRECT_CALL_IMM_ALIGN`, `SP_PATCHABLE_CALL_ALIGN`. Not re-counted since round 25. Count per run:
`CRATONVM_DBG_JITC=1 ... compile 1 3 2>&1 | grep -cE 'c2-supersede published java/util/regex/Pattern.compile\(\)V'`
and `grep -c 'stale-caller recompile requested'`; one publication and no stale-caller recompile of `compile()V`
means the churn is gone and this item can be struck.

## What would close it

A `[c2]`-only RIP sample (or `perf record -e instructions:u` with `CRATONVM_JIT_PERF_MAP=1`) of the `compile` row
on the host, naming the symbols that hold the 4.8k excess instructions per op. The r23 suspects not covered above
were the per-allocation GC bookkeeping (`jit_zgc_note_tlab_object` / `is_object_address`; lane D's area) and
the `study` chain. Until that sample exists, no lane can size its share of the gap.
