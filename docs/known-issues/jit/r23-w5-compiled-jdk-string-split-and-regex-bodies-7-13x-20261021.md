# Compiled JDK `String.split` / regex bodies run 7-13x HotSpot (the residual of the split and toArray rows)

> **Round rx27 close (2026-10-29, orchestrator): STILL OPEN for `split-ws`, `cached` and `compile`; the split
> fast path and `list` pass.** Linux host, `tools/probes/icount2.sh`, median of 5, `cvm-rx27b19` (`fac6921ae7`, every rx27 flip):
>
> | row | base `df86fe90ba` | rx27 close | HotSpot 25 | ratio | gate (3.0x) |
> |---|---:|---:|---:|---:|---|
> | `split-ws` | 65 128 | 41 919 | 12 594 | 3.33x | open |
> | `split-comma` | 6 542 | 5 026 | 2 831 | 1.78x | **passes** |
> | `split-comma-10` | 6 275 | 4 641 | 2 763 | 1.68x | **passes** |
> | `compile` | 4 356 | 4 160 | 803 | 5.18x | open |
> | `cached` | 62 198 | 37 635 | 11 771 | 3.20x | open |
> | `list` | 3 832 | 3 736 | 1 817 | 2.06x | **passes** |
>
> rx27 levers on these rows: `STRING_SUBSTRING_PREFIX` / `SEQ_SUBSEQUENCE_PREFIX` default ON (split-comma 0.77x,
> split-comma-10 0.75x); the keep-loop rules (cached/split-ws -25 %); `CHAIN_DEEP_SPINE` default ON with
> `CHAIN_ENTRY_SPLICE` opt-in (cached -15..-21 %, split-ws -13..-15 %); `FRAME_BLOCK_DESCRIPTOR` (compile -5..-7 %).
> Opt-in, measured without a disjoint gain: `ARRAYLIST_ADD_PREFIX`, `ARRAYS_COPY_OF_RANGE_PREFIX`,
> `SPLICE_COLD_ARM_EDGE_SET`, `SP_INSTANCEOF_PROXY_WAYS`, `ZGC_LEAN_START_BIT`. Removed as never paying:
> `ONCE_WALK_CHAIN_SPLICE`, `CALL_WRITE_SUMMARY` (each added a resolver bypass the metadata guard refuses). The remaining cost is the same body quality as the Matcher rows (see
> `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`, "Round rx27 close"); `compile` additionally runs the
> parse in large frames (5.2x). Correctness side effect of the round: `--compatible` printed nothing on dev's
> fast-boot prune (fixed, `docs/internal/fixed-bugs/j27-compat-fast-boot-jit-runs-no-main-20261028-FIXED-20261029.md`).

> **j27 hand-off: out of scope for round j27, moves to a separate session.** Round j27 (lane STRRX) removed one
> per-field call from the split fast path (below); the rows are still 3.6x HotSpot, so the page stays OPEN. What
> remains, in order of size:
>
> 1. `cached` / `split-ws` / `regex-split` (the `Pattern` engine): unchanged by j27 (`cached` 4 080 vs 4 045
>    ns/op, HotSpot 789, 5.1x). Their residual is the Matcher rows' list on
>    `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md` (the `Node.match` chain code quality, `Matcher.<init>`,
>    the `Pattern.matcher` monitor-guard call); work it there.
> 2. `split-comma` / `split-comma-10` (the `String.split(CIZ)` fast path, now 549 / 556 ns/op against HotSpot's
>    154 / 155): per field it still pays `substring` (a fresh String + `Arrays.copyOfRange`) and
>    `ArrayList.add`, and at the end `subList(0, n).toArray(result)`; HotSpot runs the whole loop as one C2 body
>    with the allocations bumped inline. Next step: `perf`/RIP-sample `split-comma` with `=0`/default of the new
>    switch to name the next symbol (`CRATONVM_DBG_IR_COMPILES=1` shows `String.split(CIZ)` accepted with 5 call
>    prefixes; read its `inline-plan` for which of `substring` / `ArrayList.add` stayed calls).
> 3. `list` (258 vs 111 ns/op, 2.3x): `subList().toArray` -- the copy, see the round-24 notes below.
>
> Measurement recipe (Windows, the j27 one): build `cargo build --release -j 5 -p cratonvm-cli` with
> `CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`, copy the exe, then per row
> `cmd //c start "" //b //wait //affinity FFFF <exe> [--compatible] -cp <cls> R24RegexSplitRows <row> 1 3`
> (HotSpot the same with `java.exe`), 5 interleaved processes per arm, median of the per-process minima. On the
> Linux host prefer `tools/probes/icount2.sh` (instructions per op).
>
> **j27 wave 1 (lane STRRX, 2026-10-28): PARTLY FIXED, MEASURED LOCALLY (Windows), switch default ON.** Item 3's
> String half: the `String.split(CIZ)` fast path calls `this.indexOf(ch, off)` once per field, and
> `String.indexOf(int, int)` was the one hot String member on the split rows with no inline answer in either tier
> (no resolver row, no prefix kind): in an optimizing-tier body it was a real call, or a splice of `Math.max` /
> `isLatin1` / `Math.min` / `StringLatin1.indexOf` -> `canEncode` -> the `indexOfChar` byte loop. New prefix kind
> `IrStringSearchKind::IndexOfCharFrom` (`jit/src/lib.rs` `ir_string_search_prefix_kind`, `num_params` 2, kept a
> call by `ir_string_prefix_site_stays_a_call`; `jit/src/ir_lower.rs` `Lowerer::emit_str_index_of_char_from_arm`,
> sharing the new scan helpers `emit_str_byte_scan` / `emit_str_unit_scan` / `emit_str_pair_scan` /
> `emit_str_index_result` with the `indexOf(int)` arm): JDK 25's body exactly (`fromIndex` clamped to `[0, len]`;
> LATIN1 `canEncode` then the byte scan, 16 bytes per SSE2 step; UTF16 the code-unit scan, or for a
> supplementary needle the surrogate-pair scan at `i < len - 1`; a negative / invalid needle -1). Nothing in that
> body throws except the null receiver, which declines to the call; an unknown coder and an odd UTF16 length
> decline too. The arm notes `ScalarIntrinsic` acceptance evidence (no single-pass form exists), as the
> `getChars` prefix does; without it a body whose only gain is this site was refused by the acceptance gate.
> Switch **`CRATONVM_JIT_IR_STRING_INDEXOF_FROM_PREFIX`, default ON, `=0` kill** (registered: token
> `ir-string-indexof-from-prefix` in `types/src/flag_groups.rs`, `docs/flag-tokens.md`,
> `docs/config/flag-inventory.md`; rx27 lane R reviewed the arm against JDK 25 and found it exact, and the
> `indexOf(int)` arm's bytes unchanged by the helper refactor apart from the pair scan's scratch register).
>
> Measured (Windows, cvm j27 build on the dev tree of 2026-10-28, P-cores, 5 interleaved processes per arm,
> median of per-process minima, ns/op, `tools/probes/R24RegexSplitRows.java`):
>
> | row | HotSpot 25 | cvm `=0` | cvm default | change |
> |---|---:|---:|---:|---:|
> | `split-comma` (`--jdk-only`) | 154 | 681 | 549 | 0.81x (4.4x -> 3.6x HotSpot) |
> | `split-comma-10` (`--jdk-only`) | 155 | 654 | 556 | 0.85x (4.2x -> 3.6x) |
> | `split-comma` (`--compatible`) | 154 | 694 | 606 | 0.87x |
> | `split-comma-10` (`--compatible`) | 155 | 723 | 611 | 0.85x |
> | `list` (control) | 111 | 275 | 258 | noise |
> | `cached` (control, regex) | 789 | 4 080 | 4 045 | noise |
>
> Facts: `tools/probes/jit/j27/J27IndexOfFrom.java` prints HotSpot 25's lines byte for byte in both modes, with
> the switch default and `=0` (also `R24RegexSplitRows facts` and `tools/probes/t1/T1W8RlStrMixedCoder.java`,
> both modes; `--compatible` printed without `CRATONVM_COMPAT_FAST_BOOT_JIT=0` on the final build)
> (`fact indexof-from-checked 3600`, `fact indexof-from-mismatches 0`,
> `fact comma-from-0 1`, `fact comma-from-2 3`, `fact comma-from-neg 1`, `fact comma-from-len -1`,
> `fact comma-from-max -1`, `fact utf16-comma-from-5 9`, `fact supp-from-0 1`, `fact supp-from-2 -1`,
> `fact emoji-from-1 7`, `fact lone-high-at-end 9`, `fact invalid-cp -1`, `fact negative-ch -1`,
> `fact split-comma a|b|c|d|e|f|g|h|i|j`, `warm true`). Engagement: `CRATONVM_DBG_IR_COMPILES=1` prints
> `[ir] string-search-prefix n202 java/lang/String.indexOf(II)I kind=IndexOfCharFrom` in the
> `java/lang/String.split(CIZ)` compile, which is accepted. Unit tests (`cratonvm-jit --lib`, pass, also with
> `CRATONVM_JIT_IR_STRING_SSE2=0` and `CRATONVM_JIT_IR_STRING_INDEXOF_SUPPLEMENTARY=0`):
> `j27_strrx_indexof_from_kind_tests::*` and the executed block in
> `ir_lower::tests::nio20_ir_string_search_prefix_answers_inline_or_dispatches` (every coder, every `fromIndex`
> clamp, SSE and scalar paths, pairs / split pairs / lone surrogates, legacy receiver, declines, the `=0` arm
> calls). Still to confirm on the Linux host: the same rows by `icount2.sh`.

Status: OPEN. Round 25 (2026-10-24): the split rows are 2.1-6.9x HotSpot by instructions (from 2.6-10.3x);
see "Round 25 result". Originally: (JIT round 23 wave 5, lane L5; measured on n23-w4 `a380a3bc69`, nothing built). Filed 2026-10-21.
Wave 6 (lane L1, 2026-10-21; pending build): items 1 and 2 of "What would close it" are done.
`CRATONVM_JIT_IR_STRING_CALL_ADMIT` is now **default ON** (`=0` kill; `callers` keeps its meaning). The real cost
was a cold `String.valueOf(char)` call (details below). The rows stay 4-10x HotSpot, and item 3 is what is left.

## Round rx27 wave 5 (lane chain, 2026-10-28; pending build): `compile` row, parse bodies and field rereads

* **Parse bodies.** Read from the JDK 25 class file: `sequence` (647 bytes), `atom` (422), `escape`
  (1 306) and `RemoveQEQuoting` (370, with a handler) are over `FreqInlineSize`, and `closure` (276) has
  typed handlers. The small helpers (`peek`, `next`, `has`, ...) are not profile symbols, so they are most
  likely spliced already. `expr` (142) and `curly` (66) are the in-range bodies that stay calls; their
  refusal needs the census on the page below.
* **`CRATONVM_JIT_IR_CALL_WRITE_SUMMARY` (opt-in).** A primitive field read is reused across an own,
  statically bound call whose callee's transitive body provably writes no field of that index. The page
  covers its limit: a callee that can construct an exception has no summary, which covers the big parse
  bodies.

Details, census and `icount2` arms: `rx27-chain-compile-row-helpers-and-call-write-summaries-20261028.md`.

## Round rx27 wave 3 (lane splitfast, 2026-10-28; pending build)

Wave-1 result (orchestrator, rx27b2 + flip `69ae68e4a8`): `CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX` and
`CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX` are **default ON** (`split-comma` 0.77x, `split-comma-10` 0.75x,
`regex-groups` 0.86x, `cached` / `split-ws` / `regex-split` 0.93-0.95x of instructions, nothing worse).

**What `split-ws` does per part besides matching** (JDK 25 `Pattern.split(CharSequence, int, boolean)`, read
from the code): `m.find()` (matcher lane), `m.start()` / `m.end()` (`checkMatch` + a field; spliced through the
default-ON unresolved-ldc exit), `input.subSequence(index, m.start()).toString()` (now the `SeqSubSequence`
prefix plus `String.toString`), and `matchList.add(match)`. Per op on top: `Pattern.compile("\\s+")` (the
`compile` row, compile lane), `matcher(input)` (the `Matcher` allocation), three ArrayList growths for ~20
parts (`new Object[10]`, then `Arrays.copyOf` to 15 and 22), and `subList(0, n).toArray(new String[n])` (a
`SubList` plus the checked inline reference copy, wave 11). So the per-part item left in this lane is
`ArrayList.add`.

**Change (both opt-in):**
- `CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX=1` (token `ir-arraylist-add-prefix`): where the JDK `ArrayList`
  bytecode runs (`--jdk-only`, or the `JdkArrayList` alignment row), the VM builds
  `ArrayListFieldLayout::for_jdk_add` (adds `modCount`; the get/size answers stay off, so `--jdk-only` keeps
  compiling the JDK's `get` / `size`); the splice planner keeps `ArrayList.add(Object)` (and `List` /
  `Collection.add` through `invokeinterface`) a call; `ir_lower::try_emit_ir_arraylist_add_prefix` answers
  the no-growth case: exact COMPACT `ArrayList`, non-null element, `size < length`, a null last slot (the
  stale-view marker) and a null target slot (SATB), and `concat_emit::emit_ir_ref_store_proof` (G1 same
  region, Generational young holder, ZGC not blocked -- the `new String(char[])` prefix's rules for its store
  into a non-fresh object). Then `modCount + 1`, the store, `size + 1`, `true`. Growth, a null element and
  every failed proof are the call (whose `aastore` pays the barrier).
- `CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX=1` (token `ir-arrays-copy-of-range-prefix`): the static
  `Arrays.copyOfRange(byte[], int, int)` as a kept call with an allocating prefix
  (`concat_emit::emit_ir_byte_copy_of_range`): `0 <= from <= to <= length` gets a fresh `byte[to - from]`; every
  throwing input (IAE `from > to`, AIOOBE `from < 0` / `from > length`), the zero-padded `to > length`, a full
  TLAB and the ZGC pre-check are the call. Not expected to move the R24 rows (the substring prefix already
  removed their `copyOfRange`); it is for the other callers.

Unit tests (`cratonvm-jit --lib`): `rx27_w3_splitfast_tests::*`,
`x64::concat_emit::r20_xsb_builder_tests::rx27_copy_of_range_copies_exactly_the_range_or_declines`; plus the
`r9w8_arraylist8_arraylist_access` integration test (layout struct gained two fields) and `cratonvm-types` in
full (two flag rows).

**Engagement:**

```
cd /root/rx27/p
CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX=1 CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-<tag> -cp . R24RegexSplitRows split-ws 2 1 2>&1 \
  | grep -E 'arraylist-add-prefix|string-prefix-declined n[0-9]+ java/util/ArrayList.add'
# expect: ir-splice-refused arraylist-add-prefix-site java/util/ArrayList.add(Ljava/lang/Object;)Z ... in java/util/regex/Pattern.split...
#         [ir] arraylist-add-prefix n<id> java/util/ArrayList.add(Ljava/lang/Object;)Z class_id=<id>
CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX=1 CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-<tag> -cp /root/rx27/bat/cls/rx27 Rx27SplitFastFacts 2>&1 \
  | grep -c 'arrays-copy-of-range-prefix'
```

**A/B** (`icount2.sh`):

```
DIR=/root/rx27/p bash icount2.sh sf3 split-ws,cached,regex-split,split-comma,split-comma-10,list,regex-groups \
  hs:hs: def:<tag>: add:<tag>:CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX=1 \
  cor:<tag>:CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX=1 \
  both:<tag>:CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX=1,CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX=1
```

Expected: `add` moves `list` (17 adds), `split-comma(-10)` (10), `split-ws` / `cached` / `regex-split` (~20 per
split) by whatever a call or splice of `add` costs per part minus ~25 instructions -- unknown until measured,
since nothing shows whether the site was spliced before. `cor` flat on these rows. Facts:
`tools/probes/jit/rx27/Rx27SplitFastFacts.java` (now also `fact list-*`: growth boundaries, null elements,
`List` / `Collection` interface sites, the fail-fast iterator's CME, a sub-list after an add) and
`R24RegexSplitRows facts`, both modes, arms `def` and `both`; the GC battery (ZGC, G1, generational) on `add`
(the prefix stores a reference under its own collector proof).

Not done (lane area, no data): an inline `Arrays.copyOf(Object[], int)` for `ArrayList.grow` (two per
`split-ws` op; copying references into a fresh array needs the same per-collector proof per element), and
the `subList(..).toArray(..)` tail (once per op).

## Round rx27 wave 2 (lane bimodal, 2026-10-28; built as `cvm-bimodal-3`): `cached` loses its bimodality

`cached` (and `split-ws`) ran in one of two levels per process because the keep-loop-splice-caller rule read
whichever body of `BmpCharPropertyGreedy.match` was published when `Pattern$Start.match` compiled. With
`CRATONVM_JIT_IR_KEEP_LOOP_STRING_PREFIX` and `CRATONVM_JIT_IR_KEEP_LOOP_RARE_ROW` (default ON) `cached` is
45 226 main/op on 5/5 processes (42.8-46.6k) against rx27b2's 62 186. Details, arms and the other rows:
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`, "Round rx27 wave 2 (lane bimodal)".

## Round rx27 wave 1 (lane splitfast, 2026-10-28; pending build)

**What the split body runs per field (read from the code, not measured).** After j27 the `indexOf(ch, off)` is a
prefix; `substring(off, next)` is either a call into the compiled `substring` or its splice: `length()`,
`checkBoundsBeginEnd`, `isLatin1`, `StringLatin1.newString` (its `ldc ""` arm unresolved unless some empty part
was ever made -- the opt-in ldc exit of wave 9), then `Arrays.copyOfRange([BII)` (the clone test, a
`new byte[n]` allocation, the `System.arraycopy` byte prefix since the default-ON
`CRATONVM_JIT_IR_SPLICE_ARRAYCOPY_ROW_BOUND`), then `new String` + `String.<init>([BB)V`. Two allocations, two
headers, a copy prefix and up to three call edges per field; HotSpot does one allocation pair and an intrinsic
copy. The planner's `static-intrinsic-keeps-its-own-fast-path` refusal no longer applies to this chain (R25-FLIP
`CRATONVM_JIT_IR_SPLICE_PAST_ARRAYCOPY`), so the lever is not another splice rule but removing the chain.

**Change.** Two new optimizing-tier String-prefix kinds, kept CALLS by the splice planner
(`ir_string_prefix_site_stays_a_call`), so the prefix replaces the whole chain:
- `Substring`: `String.substring(II)` and `String.subSequence(II)`. Switch
  **`CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX`, opt-in `=1`** (token `ir-string-substring-prefix`).
- `SeqSubSequence`: `CharSequence.subSequence(II)` on a receiver whose header says String (`Pattern.split`'s and
  `Matcher.group`'s `input.subSequence(..).toString()`). Switch **`CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX`,
  opt-in `=1`** (token `ir-seq-subsequence-prefix`).

Code: `jit/src/lib.rs` (`ir_string_search_prefix_kind`, the two `*_enabled` readers),
`jit/src/ir_lower.rs` `Lowerer::try_emit_ir_string_substring_prefix`, `jit/src/x64/concat_emit.rs`
`emit_ir_latin1_substring` (JDK 25 bounds, `this` for the whole range, else a fresh LATIN1 `byte[]` + String in ONE
TLAB bump through the `StringBuilder.toString` prefix's allocation, new `src_prechecked` mode). Declines to the
unchanged call before any store: null / non-String receiver, UTF16, every range the JDK throws
`StringIndexOutOfBoundsException` for, the empty range (the `""` literal), and the allocation's own (layout
replaced, compact strings off, cap, no thread, full TLAB, G1 / ZGC pre-checks). So every exception, message and
identity is the JDK body's.

Unit tests (`cratonvm-jit --lib`): `rx27_splitfast_substring_kind_tests::*` and
`x64::concat_emit::r20_xsb_builder_tests::rx27_substring_copies_exactly_the_range_or_declines` (runs the emitter
for every `(begin, end)` of a LATIN1 value, the whole range, and every throwing / declining input). Also run
`cratonvm-types` in full (two new flag rows).

**Engagement** (one line per site per compile; a decline names its reason):

```
cd /root/rx27/p
CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX=1 CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-<tag> -cp . R24RegexSplitRows split-comma 2 1 2>&1 \
  | grep -E 'string-substring-prefix|string-prefix-declined n[0-9]+ java/lang/String.substring'
# expect: [ir] string-substring-prefix n<id> java/lang/String.substring(II)Ljava/lang/String; kind=Substring ...
CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX=1 CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-<tag> -cp . R24RegexSplitRows cached 2 1 2>&1 \
  | grep -E 'string-substring-prefix|string-prefix-declined.*subSequence'
# expect: kind=SeqSubSequence in the java/util/regex/Pattern.split compile
```

**A/B** (`icount2.sh`, instructions per op):

```
DIR=/root/rx27/p bash icount2.sh sf split-comma,split-comma-10,list,cached,regex-split,regex-groups,split-ws \
  hs:hs: def:<tag>: sub:<tag>:CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX=1 \
  seq:<tag>:CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX=1 \
  both:<tag>:CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX=1,CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX=1
```

Expected: `sub` moves `split-comma` / `split-comma-10` (ten substrings per op; the chain above is most of the
~1 000 instructions per field) -- a guess of -25 to -40 %, which would put them near 2.5x HotSpot; `list`,
`cached`, `split-ws`, `regex-*` flat. `seq` moves `cached` / `split-ws` / `regex-split` (17 / 17 / ~30 parts per
op) and `regex-groups` (two `group()` per find); the split fast path flat. Not expected to move `list` (no
substring; see `rx27-splitfast-arraylist-add-and-sublist-toarray-per-field-20261028.md`).

**Batteries.** Facts: `tools/probes/jit/rx27/Rx27SplitFastFacts.java` (new: substring / subSequence / every
`copyOfRange([BII)` edge -- whole-array clone, empty, padding past the end, `from > to` IAE text, negative and
past-the-end `from` AIOOBE text, null -- and the split edges; `fact stable true`, `fact substring-mismatches 0`,
`fact copyofrange-mismatches 0`) and `R24RegexSplitRows facts`, both modes, arms `def` and `both`, against
HotSpot (`bat.sh`). The prefix allocates inline, so also the GC battery (ZGC, G1, generational) on arm `both`.
Page stays OPEN until the rows are within 3x.
## Round rx27 wave 3b (lane alloc, 2026-10-28; pending build): lean ZGC start-bit store, opt-in

Two opt-in switches, `CRATONVM_JIT_ZGC_LEAN_START_BIT` and `CRATONVM_JIT_ZGC_START_BIT_BTS` (commit `b97a250bfd`),
trim the inline ZGC start-bit store:
- the substring `byte[]` + `String` pair: 60 -> 37 instructions;
- a compiled `new`: about 45 -> 41.

The expected effect on these rows is 1-3 %. Arms, invariants and batteries are on
`rx27-alloc-zgc-compiled-allocation-is-30-instructions-over-hotspot-20261028.md`.

## Round rx27 wave 1 (lane alloc, 2026-10-28; pending build)

Question: why do `matchonly` / `cached` spend 18-22 % in `jit_new_object`, `jit_post_alloc_init` and
`jit_anewarray_object` (wave 13)? Answer by reading tip (`df86fe90ba`): on the default collector no
allocation shape on these rows should call Rust per allocation any more. The wave-13 samples came from an
older build. A census is added so the host can confirm this per site. No codegen change; no perf switch.

**What tip does (ZGC, optimizing tier).**
- `new C` (`ir_lower.rs` `Op::New`) takes the inline bump with the inline start-bit store, and makes no call
  on its fast path, when the class is proven initialised and is a post-init no-op. A post-init no-op class
  has no finalizer, is not a `java.lang.ref` class, and owes no typed default. With a compact layout
  (default ON), every default is zero bytes (`jit_bridge::jit_new_site_flags`, `CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM`).
  `Matcher`, `ArrayList` and `String` qualify unless their layout is LEGACY (a PADDED class, a
  multi-`ClassStore` process, or `CRATONVM_COMPACT_REF_FIELDS=0`).
- A site pays `jit_new_object` per allocation only on the stub route: the class is not proven initialised at
  compile time, the bump declined, or `CRATONVM_JIT_IR_INLINE_TLAB=0`. A deferred site (`Op::NewCp`, class
  unresolved at compile time) pays `jit_new_object_cp` per allocation. A legacy-layout class with reference
  fields pays `jit_post_tlab_init` -> `jit_post_alloc_init` per allocation.
- `new Object[n]` / `new IntHashSet[n]` (`Op::NewArray`, `element_type == 0`): on ZGC the bump is offered by
  `registration && CRATONVM_JIT_INLINE_TLAB_ANEWARRAY` (zr z3/ja, default ON), with the start-bit store. The
  wave-13 sentence "on ZGC the IR tier allocates `Object[]` through the helper
  (`CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC` stays off)" does not hold at tip. That switch only covers
  Generational / G1 (`ref_admitted` in `runtime_lowering::emit_inline_tlab_newarray_ir_impl_rt`). It has also
  been default ON since round 23 wave 2b (`r21-lean-allocation-rows-cost-is-outside-the-inline-sequence-FIXED-20261024.md`
  item 1: G1 `objarr16` 46.7 -> 22.2 ns). Its old opt-in reason (the r22 A/B was mixed) was measured on ZGC,
  where the switch does nothing. Nothing to flip.
- In `cached`, `jit_anewarray_object` was 20 % inclusive and 17 % of that was `refill_tlab_at_least`
  (wave 13). That is the TLAB-full slow path: every route calls it once per 512 KiB chunk. In time it is the
  chunk zeroing. In instructions the memset is about 16 000 per chunk (32-byte stores), about 50 per op at
  1.6 KB per op. HotSpot's compiled code zeroes the same bytes per object, so this is not a gap.

**What the census prints (`CRATONVM_DBG_JITC=1`, commit `e629e40686`).**
- `ir new-site n<id> class=<cid> ... route=<sunk|zgc-split|inline-*|stub-only|cp-helper> call=<reason> method=<Class.m(desc)>`:
  one line per `new` site, including the ZGC split route, which printed no site line before.
- `ir newarray-site n<id> atype=<t> component=<cid> route=<zgc-split|inline|stub-only> call=<reason> method=<...>`:
  one line per array site. The Generational / G1 sunk route keeps its `ir newarray map sink` line.
- `call=none` means the fast path makes no call. Anything else names the per-allocation call:
  `inline-tlab-off`, `class-not-proven-initialised`, `bump-declined` (preceded by
  `ir inline-TLAB bump DECLINED: <why>`), `class-owes-post-init`, `spliced-class-owes-post-init`,
  `zgc-inline-start-bit-off`, `post-init-elision-off`, `anewarray-any-gc-off`, `inline-array-off`,
  `deferred-class`. The rules are `new_site_call_reason` and `newarray_site_call_reason` in `jit/src/ir_lower.rs`.
  Tests: `ir_lower::rx27_alloc_site_census_tests` (5).

**Host commands.** Use the build of this branch. Run each in both modes (add `--compatible`).

```
cd /root/rx27/p
for r in matchonly cached regex-matches split-comma; do
  CRATONVM_DBG_JITC=1 CRATONVM_DBG_LAYOUT=1 /root/r25-bin/cvm-<tag> -cp . R24RegexSplitRows $r 1 3 2>&1 \
    | grep -E 'ir (new|newarray)-site|ir lean body|\[layout\] java/util/(regex/Matcher|ArrayList) ' \
    | sed 's/ n[0-9]* / /' | sort | uniq -c > alloc-$r.txt
  grep -v 'call=none' alloc-$r.txt | grep -E 'regex|ArrayList|String|Arrays'   # expect nothing
done
CRATONVM_DBG_JIT_METHOD_STATS=1 /root/r25-bin/cvm-<tag> -cp . R24RegexSplitRows matchonly 1 3 2>&1 \
  | grep -E 'compiled-new helper entries|tlab_refills|jit_anewarray_calls'
```

- Positive control: the census must print `ir new-site ... method=java/util/regex/Pattern.matcher(...)` (or
  the caller it is spliced into). If it does not, the `new Matcher` is not in an optimizing body, and the rows
  below do not apply.
- Expected: `jit_new_object_calls` and `jit_anewarray_calls` about equal to `tlab_refills`, not to the op count.
- `[layout] java/util/regex/Matcher ... LEGACY` or `PADDED` would explain a `class-owes-post-init` line.
- `ir lean body ... thread=frame` would mean a body whose bump reads a zeroed thread slot and takes the stub on
  every execution.
- Any `call=` other than `none` on a hot JDK site is a defect. File it under `docs/known-issues/jit/` with its reason.

**A/B arms.** None. This wave adds no perf switch, and the census changes no byte of code.

**Expected effect per row.** None from this wave. By reading, allocation is not what closes the 3x gate. The
remaining per-allocation cost on ZGC is the inline sequence itself: about 45 instructions per object against
HotSpot's 10-20 (`rx27-alloc-zgc-compiled-allocation-is-30-instructions-over-hotspot-20261028.md`). With about
50 allocations per op on `cached` / `split-ws` and 32 on `regex-matches`, that is roughly 1 000-1 500
instructions per op, 5-8 % of the CratonVM count, against gaps of 10 000+ per op.

**Batteries.** None for this wave (diagnostic only): `cargo test -p cratonvm-jit --lib rx27_alloc_site_census`
and the usual lib suite. No GC invariant changes: no emitted byte, ordering or map changed.
## Round rx27 wave 1 (lane matcher, 2026-10-28; pending build): `cached` gets the W13-6 rule back

The j27 mass flip turned `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES` ON. That is W13-6's loss arm: a
looping `anewarray` callee (`Pattern.split(CharSequence,int,boolean)`) splices into its callers and its in-loop
sites lose their plan (`cached` +12.6 % instructions per op). It is opt-in again; `CRATONVM_JIT_IR_SPLICE_ANEWARRAY`
stays default ON. Expect `cached` about 11 % lower than the j27 default. Recipe, census lines and A/B arms:
"Round rx27 wave 1 (lane matcher)" on `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`.
## Round rx27 wave 1 (lane compile, 2026-10-28; pending build)

Read at `df86fe90ba`, nothing built or run. Target: `compile` (`Pattern.compile("\\s+")`, 5.6k instructions per
op against HotSpot's 810 in round 25), and the per-op compile inside `split-ws` and `regex-split`.

**What is still true of the named residuals.**

1. `jit_instanceof` from `Pattern.newCharProperty`'s `p instanceof BmpCharPredicate` (21 % in the r23 sample).
   Answered by round 25 for the `compile` row: `CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS` is default ON, and its one
   hit way holds the `ASCII_SPACE` lambda after the first call; the `(BmpCharPredicate) p` cast on the hit edge is
   dropped by `CRATONVM_JIT_IR_CAST_AFTER_INSTANCEOF` (default ON) when the cast's control is the `If`
   projection itself. Two gaps remain, both fixed below behind opt-in switches: `regex-split` compiles `",\\s*"`
   per op, so the site sees the `Single` lambda and then `ASCII_SPACE`'s, both `true`, and with one hit way each
   evicts the other (two helper calls per op); and a proxy `checkcast` that is not dropped always calls
   `jit_checkcast`. The single-pass tier has no proxy arm at all: filed as
   `rx27-compile-single-pass-typechecks-have-no-proxy-ways-20261028.md`.
2. The `Node.study` chain (`callee-unresolved` with a speculated class id) and `Pattern.<init>`: not this lane's
   area; status and owners on `rx27-compile-pattern-compile-residuals-at-tip-20261028.md`.
3. `Pattern.compile()V` republication churn: every switch rounds 23-25 built against it is default ON at tip
   (`PUBLISH_OVER_SUPERSEDED_CALLEE`, `PIN_BAKED_CALLEES_WHILE_COMPILING`, `C2_PUBLICATION_REFUSAL_REFUND`,
   `STALE_CALL_REBIND_ABS64`, `IR_DIRECT_CALL_IMM_ALIGN`, `SP_PATCHABLE_CALL_ALIGN`). Whether it still churns is a
   host count (same residuals page).

**What changed** (jit `ir_lower.rs`, `lib.rs` `ir_tier_attempt`, `ir.rs`; vm `helpers/typecheck.rs`):

- `CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS` (opt-in `=1`): the proxy cell (`TypecheckSeenCell`, now four words) keeps a
  second hit way; a helper `1` moves the hit way's proxy into it (`XCHG [cell], id ; MOV [cell+8], old`).
- `CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS` (opt-in `=1`): an IR `checkcast` shares its target's proxy cell
  (`Graph::checkcast_proxy_cells`), passes a proxy found in a hit way inline, sends any other proxy straight to the
  helper, and fills the hit way from the reference a passing `jit_checkcast` returns. Sound for the reason the
  r25 ways are: both helpers ask `jit_typecheck_resolve` (`lenient` only labels), whose `true` for a proxy id is a
  function of the proxy's call-site row and the resolved target; a cast never writes the miss way. The r25 W6-R
  loader rule is untouched (the helper still decides every first sight; cells are per interned target, which is
  per resolved class id).
- The type-check census (`CRATONVM_DBG_TYPECHECK_FILTER=<sub>`) names a proxy receiver `reason=lambda-proxy`
  instead of `no-display`.

Tests: `ir_lower::rx27_proxy_ways_tests` (switches; the fill bytes executed on a real cell; the lowered
`instanceof` and `checkcast` executed against a counting fake helper, with the one-way arm as the positive
control), vm `rx27_the_census_names_a_lambda_proxy_receiver`.

**Host commands** (`cd /root/rx27/p`, `B=/root/r25-bin/cvm-<tag>`):

```
# engagement: the cells and the casts that reach lowering
CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS=1 CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS=1 \
  $B -cp . R24RegexSplitRows regex-split 1 3 2>&1 | grep -E 'ir (instanceof|checkcast) proxy ways' | sort | uniq -c
# residual helper calls on proxies (per arm; expect regex-split default > two-ways arm, compile ~0 in steady state)
CRATONVM_DBG_TYPECHECK_FILTER=CharPredicate $B -cp . R24RegexSplitRows regex-split 1 3 2>&1 \
  | grep 'reason=lambda-proxy' | awk '{print $3, $5}' | sort | uniq -c
```

A/B (`icount2.sh rx27c compile,split-ws,regex-split,cached <arms>`):
`hs:hs:` `def:<tag>:` `two:<tag>:CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS=1`
`cast:<tag>:CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS=1`
`both:<tag>:CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS=1,CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS=1`
`r25off:<tag>:CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS=0` (how much round 25's ways still buy).

Expected: `regex-split` loses about two `jit_instanceof` calls per op under `two` (a few hundred instructions of
about 112k: inside the noise band, so judge it by the census count, not the row); `compile` and `split-ws` see one
proxy per op and should not move; `cast` moves a row only if the census shows a `checkcast` proxy site left after
`CAST_AFTER_INSTANCEOF`. Neither switch closes the 3x gate for `compile`; what does is not in the type checks
(residuals page). Batteries: `tools/probes/jit/rx27/RX27ProxyTypecheckFacts.java` (new: lambda proxies of two
loaders, sub-interface, Serializable intersection and hidden classes through compiled `instanceof` / `checkcast`
sites; diff against HotSpot 25 in both modes, arms default / both switches / `INSTANCEOF_PROXY_WAYS=0`),
`tools/probes/jit/r25/R25W6ProxyWaysLoaders.java` with both switches, `R24RegexSplitRows facts`, the 66-probe facts
battery with `both`.

## Round 25 result (2026-10-24, orchestrator): the split rows 0.68-0.90x of round 24 by instructions; still OPEN

Build: `jit-r25-arch` merged with dev `3076317260`, release with LTO off, on the Linux host (16 cores,
load 10-25). Probe: `tools/probes/R24RegexSplitRows.java`, user-mode instructions per op as
(I(3N) - I(N)) / 2N under `perf stat -e instructions:u` (repeats within 5-8 %). `base` is dev `c89d11b473`.
Facts equal HotSpot 25 in both modes (the 66-probe battery: 114 SAME, and the same 18 HotSpot-side DIFF
lines as the base, with the round-25 switches on and with all of them at `=0`).

| row | HotSpot | base | round 25 | ratio to HotSpot (base -> r25) |
|---|---|---|---|---|
| `split-ws` | 12 307 | 70 922 | 56 139 | 5.8x -> 4.6x |
| `split-comma` | 2 804 | 12 197 | 10 154 | 4.3x -> 3.6x |
| `split-comma-10` | 2 760 | 12 446 | 9 899 | 4.5x -> 3.6x |
| `cached` | 11 580 | 59 373 | 53 313 | 5.1x -> 4.6x |
| `matchonly` | 7 074 | 44 006 | 35 135 | 6.2x -> 5.0x |
| `list` | 1 831 | 4 716 | 3 883 | 2.6x -> 2.1x |
| `compile` | 810 | 8 303 | 5 618 | 10.3x -> 6.9x |

`compile` gained most: its C2 `Pattern.compile()V` now publishes, because a callee retired only by the
supersede redirect no longer refuses the caller (`CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE`, default ON;
`docs/internal/fixed-bugs/r25-regex-rows-publication-proxy-instanceof-copyofrange-FIXED-20261024.md`). The
eleven default-ON switches and the opt-ins left off are listed in the round-25 section of
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md` and in `docs/internal/jit-proposals/r25-optin-flip-triage-20261024.md`. The split rows run the same
compiled-code shape as the Matcher rows, so what is left is that page's residual list.

## Round 24 (2026-10-24, dev `ec551c685e`, Linux host): measured state, still OPEN

Same method and probe as the regex page's round-24 section (`tools/probes/R24RegexSplitRows.java`,
user-mode instructions per op from two `perf stat` runs at N and 3N ops; the host's load made ns/op
unusable for A/Bs). Facts equal HotSpot 25 in both modes.

| row | HotSpot | CratonVM `--jdk-only` | ratio |
|---|---|---|---|
| `split-comma` (`CSV.split(",")`, ten fields) | 2 850 | 11 900 | 4.2x |
| `split-comma-10` | 2 764 | 11 850 | 4.3x |
| `split-ws` (`TEXT.split("\\s+")`) | 12 482 | 71 900 | 5.8x |
| `cached` (`P.split(TEXT)`) | 11 619 | 63 000 | 5.4x |
| `list` (17 adds, `subList().toArray(new String[n])`) | 1 840 | 4 750 | 2.6x |
| `compile` (`Pattern.compile("\\s+")`) | 795 | 8 000 (3 100 when the C2 `compile()V` publishes) | 10x (3.9x) |

* `compile` is multimodal by run: about 8.0k instructions per op in nearly every run (the single-pass
  `Pattern.compile()V`, whose `Character.charCount` goes through the native door), 3.1k in one run of
  ten, 18k in one. `CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND=1` changed none of five runs (7.7-8.4k).
* `list` is the one row inside 3x by instructions. `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS=1`
  (with the byte-copy arm) halved the CPU time of the unsized `toArray(new String[0])` form of the row
  (980 -> 487 ns; `copyOfRange(Object[]..)` leaves the single-pass tier) but costs `split-comma` and `cached`
  (`copyOfRange([BII)`'s optimizing body is slower than its single-pass one), as wave 13 found.
* Every other existing opt-in was neutral here too (the list is in the regex page's round-24 section).

What is left is the regex page's redesign list: the split rows run the same compiled-code shape
(home store and reload of every value, guarded field reads, per-call shadow traffic, the frame clear),
and `cached` / `split-ws` add the `Node.match` chain. This page stays open until those land.

## Wave 7 (lane L1, 2026-10-21): the w6 numbers and the exact remaining work

**The flip is confirmed on n23-w6** (coordinator A/B, pinned, 3 reps; battery 166 SAME). Admit unset (now ON)
against `=0`:

| row | admit ON | `=0` | HotSpot | gap |
|---|---|---|---|---|
| `RxLoop split-comma` | 828-932 | 1360-1760 | ~260 | ~3.4x |
| `R23RefArraycopy split-comma-10` | 794-821 | 1232-1316 | 119-141 | ~6x |
| `RxLoop split-ws` | 6225-7974 | 6514-8506 | ~1000 | ~7x |

**Wave 7 code for these rows:** `CRATONVM_JIT_IR_MAJOR_RECEIVER_GUARDED_SPLICE` (default ON; see
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`). It splices the `CharPredicate.is` top receiver in the
`Pattern` greedy loops, which `split-ws` (`\s+`) runs too. It is not re-measured (no build this wave).

**What is left.** The page stays OPEN, because the gate (within 3x HotSpot) is not met.
1. **`split-ws`.** `split("\\s+")` runs `Pattern.compile` on every call; the JDK does the same on HotSpot. The
   compiled share is the `Pattern` constructor's work:
   - `Pattern.<init>`, `expr` / `sequence` / `atom`, `Node.study`, `CharPredicate.negate`, `Branch.add`;
   - the `CRATONVM_DBG_JITC=1` census on n23-w6 refuses these with `no-profile-evidence` (one-shot sites),
     `callee-unresolved` (`Node.study`, `IntHashSet.clear` for class 615) and `callee-too-large`
     (`TreeInfo.<init>`).
   - Then `Pattern.split` / `Matcher.find` run over the `Node.match` chain, which is megamorphic (see the regex
     page).

   Closing it needs inlining at the unresolved `Node.study` / `IntHashSet.clear` sites (find why
   `speculated_class_id` 608 / 615 resolve nothing). It also needs a profiled type-switch for the `Node.match`
   chain.
2. **`split-comma` / `split-comma-10`.** These are in the optimizing tier now. The splice budget of the C2
   `split(CIZ)` body (cap 40, 1291 bytes) runs out before three cold sites, which are not the gap. What remains is
   per-element allocation (`substring` → `StringLatin1.newString`, `ArrayList.add` / `grow`, the final `subList`
   `toArray`) against HotSpot's escape-analysed, intrinsic `Arrays.copyOfRange`.
   - Next step: re-profile the published C2 body with the RIP sampler and the JIT disassembly map, and count the
     per-element helper calls.

The A/B on `n23-w5` (`c74c0e996f`), P-core pinned, `--jdk-only`, two or three reps per arm, interleaved:

| row | unset (old default) | `=1` | `=callers` |
|---|---|---|---|
| `RxLoop split-comma` (ns/op) | 2080 / 2256 | 1672 / 1322 | 2371 / 2261 |
| `R23RefArraycopy split-comma-10` | 1550 / 1667 | 1085 / 1088 | - |
| `RxLoop split-ws` | 10977 / 10984 | 10357 / 9328 | - |
| `Nio20GetClass heapcb` (printed ns/op; one put = 10 ops) | 3 / 2 / 3 | 3 / 2 / 3 | - |
| `heapcb` with `CRATONVM_JIT_IR_STRING_GETCHARS_PREFIX=0` | 5 | 7 | - |

**The heapcb history is closed.** `r20-string-call-admit-heapcb-regression-CLOSED-20261012`:
- The regression was never a lost intrinsic. It was the optimizing body of `HeapCharBuffer.put` /
  `String.getChars`.
- The nio20 wave-6 getChars prefix moved that population off the admit switch. With the prefix on (its default),
  heapcb is equal on both arms.
- With the prefix off, the old 50 -> 70 ns per put is still there. That only matters if someone kills the prefix.
- The `callers` arm is no better than unset here, so it stays an opt-in value.

**Why `String.split(CIZ)` never left the single-pass tier.** The wave-5 reading ("no C2 request") is wrong on w5.
`CRATONVM_DBG_JITC=1` shows that the C2 request is made and the method is admitted. Then
`IrBuilder::build refused at ir.rs:26331 (bytecode pc 31) ... callee 0xb8 java/lang/String.valueOf(C)`:
- the cold single-char `String.valueOf(char)` site has no `invoke_info`, because the planning blanket refused it;
- `ir_ordinary_static_string_call_admits` exempts static String calls only for callers OUTSIDE the String classes;
- `CRATONVM_JIT_IR_STRING_VALUE_OF_CALL` is opt-in;
- so the whole 32 KB fast path stayed single-pass.

With the admit ON, the same request is accepted (`evidence: inlined,guard-elided,sunk-late,licm,scalar-intrinsic,
simplified; call prefixes 1`) and published (`c2-supersede published ... c2=54445`).

**Safety.** The admit only stops refusing members that have NO single-pass intrinsic. Those members are ordinary
calls in both tiers (`try_resolve_string_intrinsic` is asked through the name probe, a superset of the real
compile). The intrinsic members keep bailing unless the inline prefix answers them.
- Facts: the w5 battery list (`C:\craton\n23p\bat\w5all.list`, both modes) was run with `=1` on `n23-w5`.
  - 144 of 146 rows are SAME as HotSpot.
  - The 2 DIFFs are `H4b` in both modes, the known compiled-recursion-depth page, which differs on the w5
    baseline as well.
  - `H8Msgs`, which differed on the w5 baseline, is SAME here.
- Code: `ir_string_call_admit_enabled` in `jit/src/lib.rs` now uses `runtime_flag_default_on`. In
  `types/src/flag_groups.rs` the `ir-string-call-admit` entry gets `off_word: Some("0")`. The tests that pin the
  refusal arm (`jit/src/tests.rs`) now set `=0` explicitly. `the_string_call_admit_callers_arm_keeps_string_internals_refused`
  also asserts that unset == `=1`.

**Expected after the build:** split-comma about 1100-1350 ns/op (HotSpot ~260), split-comma-10 about 1090 (HotSpot
119-141), split-ws about 9300-10400 (HotSpot ~1000), heapcb unchanged.

**What is left (item 3, the next owner).** In the C2 `split(CIZ)` body the splice budget (cap 40, 1291 bytes) is
spent before three cold sites are reached: `valueOf(C)` at pc 31, `substring` at 118, `ArrayList.add` at 121.
- Those are cold, so they are not the gap. Re-profile the published C2 body.
- In the `split-ws` row the time is in the `Pattern` engine's own bodies (`Pattern.split`, `Matcher.find`, the
  `Pattern$Node.match` chains). The `CharPredicate.is` sites there have 5 receiver types and are not inlined
  (L5 finding). A guarded polymorphic inline of small lambda bodies is the wave-7 candidate.
Area: the optimizing tier's String-call planning (`jit/src/lib.rs` `string_site_bails_ir_planning` /
`string_site_bails_ir_planning_in`, `CRATONVM_JIT_IR_STRING_CALL_ADMIT`), the single-pass bodies of the JDK's
`String` / `Pattern` / `ArrayList` methods.
Severity: MEDIUM (performance only; every fact equals HotSpot).

## What is left after the per-call fixes

The two parent pages (`r23-w3-reference-arraycopy-costs-microseconds-FIXED-20261021.md`,
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`) cut the funnel crossings, the reflective allocation, the
double validation and the String reads. What remains of these rows is compiled JDK code, not helpers:

| row (P-core pinned, ns/op) | HotSpot | n23-w4 `--jdk-only` |
|---|---|---|
| `R23RefArraycopy split-comma-10` (`"a,b,...".split(",")`, the single-char fast path) | 119-141 | 1734-2276 |
| `RxLoop split-comma` (ten fields) | ~260 | 1642-1765 |
| `RxLoop split-ws` (`TEXT.split("\\s+")`, the `Pattern` path) | ~1000 | 9360-13173 |
| `R23ArraysEqRegex regex-split` | 1126-1236 | 11880-13900 |

RIP samples (n23-w4, `RxLoop`): `split-comma` 150k of ~200k samples in `?jit` (the helpers it calls --
`jit_system_arraycopy_direct`, the provenance memo -- are a few k each); `split-ws` 70k of ~120k in `?jit`, plus
`Pattern.compile`'s stand-in (~10 %, fixed by the wave-5 flip of `CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE`).

## Why the JDK String bodies stay in the single-pass tier

`CRATONVM_DBG_JITC=1` on `RxLoop split-comma`:

* `String.split(CIZ)` (the fast path, a 32 KB single-pass body) is only ever compiled `tier=C1`; no C2 request
  for it is logged.
* `String.split(Ljava/lang/String;IZ)` is refused by the optimizing tier: `NO invoke_info -- pc=116:
  java/lang/String.split(CIZ) is a loop-shaped or memory call-site intrinsic`. The reason is the planning
  blanket `string_site_bails_ir_planning`: a `java/lang/String` call site that is not one of the expander's
  accessors / search prefixes keeps the WHOLE calling method single-pass. Inside the JDK's own `String` code most
  calls are String members, so the String implementation methods are pinned to the single-pass tier, and with
  them every method that splices them.
* `ArrayList.add(Object)`'s splice is refused in the single-pass tier (`call at callee_pc=20 to
  ArrayList.add(Object,Object[],int) ... is neither spliced nor direct-bound`; the inner one has an
  `arraylength`).

A/B of the existing opt-in `CRATONVM_JIT_IR_STRING_CALL_ADMIT=1` (n23-w4, pinned, x3 interleaved, `--jdk-only`):
`split-comma` 1642 / 1742 / 1765 -> 1235 / 1114 / 1192 ns/op (~30 % faster), `split-ws` 13173 / 11599 / 12339 ->
11934 / 11922 / 13841 (no change). Not flipped here: that switch's history (`r20-string-call-admit-heapcb-
regression-20261007`, the `callers` arm) says it needs its own battery, and even with it the rows stay 4-12x.

## What would close it

1. Measure `CRATONVM_JIT_IR_STRING_CALL_ADMIT=1` and `=callers` on the String / collection probe set and the
   heapcb row; flip the winner.
2. Per-site instead of per-method: let the optimizing tier keep a String member call it cannot expand as an
   ordinary call (the single-pass intrinsic it would lose is only a call there too, for most members) instead of
   refusing the method; the `split(CIZ)` -> C1-only observation suggests the C2 request is never made for String
   implementation methods at all -- find why (tier policy or a pin).
3. With the String bodies in the optimizing tier, re-profile `split-ws` (the `Pattern` engine's own compiled code:
   `Pattern.split`, `Matcher.find` over `Pattern$Node.match` chains -- the Matcher rows of the regex page).

Gate: `R23RefArraycopy split-comma-10`, `R23ArraysEqRegex regex-split`, `RxLoop split-comma` / `split-ws` within
3x HotSpot; every fact line unchanged in both modes.

## Wave 7 A/B (orchestrator, n23-w7 6a11d90dcf, P-cores, 3 interleaved reps)

RxLoop split-comma 824-928 ns/op, split-ws 6249-6800 (HotSpot about 260 and 1000). The wave-7 major-receiver splice measured neutral and is opt-in. The items in L1's wave-7 section remain.

## Wave 8 (lane W8-L6, 2026-10-22): where the `split-ws` time can be, and no code this wave

**No code change and no new switch.** The trace from wave 7 (`C:\craton\n23p\w7L1\sw.txt`, `CRATONVM_DBG_JITC=1`,
`RxLoop split-ws` on n23-w6) answers the questions this lane was given. None of them needs a new mechanism.

**1. `Pattern.compile` per call is a small share of `split-ws`, even on HotSpot.** The new probe
`C:\craton\n23p\w8L6\W8L6SplitParts.java` splits the row into parts. HotSpot 25, P-core pinned, 3 s per row, 2 reps:

| row | what it runs | HotSpot ns/op |
|---|---|---|
| `full` | `TEXT.split("\\s+")` (the `RxLoop split-ws` body) | 581 / 627 |
| `compile` | `Pattern.compile("\\s+")` alone | 47 / 49 |
| `cached` | `P.split(TEXT)` with a static `Pattern` | 526 / 527 |
| `matchonly` | `Matcher.find` loop, no parts built | 355 / 378 |
| `comma` | `CSV.split(",")` (the fast path) | 126 / 133 |
| `list` | `ArrayList.add` x17, then `subList().toArray` | 75 / 83 |

On HotSpot, compile is about 8 % of the row, matching is about 60 %, and the parts (substrings, list, `toArray`)
are about 30 %. Our version of the same table is the first thing to measure (below). If our `compile` row is below
about 1000 ns, most of the 5-6 us gap is in `cached` minus `compile`. That means the `Pattern.split` /
`Matcher.find` / `Node.match` path, which is the regex page and lane L5, not this page.

**2. `Pattern.compile()V` never gets an optimizing body. This is one exact site.** Each C2 attempt (three in the
trace) builds as far as bytecode pc 478 and then stops: `IrBuilder::build refused at ir.rs:25290 (bytecode pc 478)`.
- pc 478 is `instanceof Pattern$Loop`, in the `topClosureNodes` loop at the end of `compile()`. A `\s+` pattern
  never loads `Pattern$Loop`.
- The unresolved-class trap for that site is refused, because `CRATONVM_JIT_IR_UNRESOLVED_TYPECHECK_IN_PLACE` is
  opt-in. The replay rule also refuses it: the `putfield`s before pc 478 commit side effects.
- So the "c2" body is the single-pass body published a second time (`c2-supersede published ... c1=61317
  c2=61317`).

Everything else in `compile()` already builds:
- both `new Pattern$StartS` sites become `Op::NewCp`;
- `putfield Loop.posIndex` gets the unresolved-field trap;
- the `Start.<init>` / `List.iterator` / `Iterator.hasNext` / `next` sites are spliced.

The `deferred-new ARMED` / `HELD x15` / `RETIRED` lines that follow are a side effect, not the cause. The build
failed with deferred `new` sites present, but those sites were not why it failed. The three extra compiles come from
`stale-caller recompile` (a rebind is declined because the call's displacement straddles an 8-byte word), not from
the deferred-new memo.

Under the existing opt-in, `frame_resumes_in_place_cleanly` holds at pc 478 and at the `checkcast` at 486. The
stack holds only the node, and the locals are `this`, two ints, the iterator and the node, with no null constant
and no `Op::New`. So the build should go through. This is an A/B of an existing switch. It is not a reason to add
code. That switch's flip decision belongs to the proxy-define page, which owns its census and risk (a sink that
cannot rebuild the frame falls back to replay and raises `InternalError`).

**3. `Pattern.<init>(String,int)` is pinned to the single-pass tier.** The trace prints `pinned to the single-pass
backend: it has a String access intrinsic here`.
- The pinning sites are the `StringBuilder.append(String)` / `toString` calls in the cold `Unknown flag 0x...`
  throw arm. Those are released only under the opt-in `CRATONVM_JIT_IR_SB_PREFIX`.
- The body is field stores and a call to `compile()`, which at 537 bytes is too large to splice. Nothing is lost
  that matters. No change.

**4. The JDK `String.split` / `Pattern.split` bodies are in the optimizing tier.** On n23-w6:
- `String.split(String,int,boolean)` is published at c2 = 13516 bytes.
- `Pattern.split(CharSequence,int,boolean)` is published at c2 = 100485 bytes, with 64 spliced bodies. These
  include the in-loop `subSequence` / `toString` (receiver guard on `String`), `ArrayList.add`, `Matcher.start` /
  `end` and `ArrayList.subList`.
- `Matcher.find` is spliced only as its shell (`ir-splice-cold-nested ... kept as calls`). The `search` /
  `Node.match` chain behind it is L5's megamorphic page.
- `List.toArray` at pc 314 is not spliced (125 bytes of budget were left). It runs once per call, so it is not the
  per-element cost.
- `String.split(String)` is spliced into the caller. Its own C2 body is dropped by the acceptance gate ("no
  evidence"), which is correct for an 8-byte forwarder.

**What to measure (orchestrator, `n23-w8.exe`):** `bash C:/craton/n23p/w8L6/ab8L6.sh <bin> 3`. It runs every
`W8L6SplitParts` row with `--jdk-only`, pinned and interleaved, on two arms: default, and
`CRATONVM_JIT_IR_UNRESOLVED_TYPECHECK_IN_PLACE=1`. Read it as follows:
- `compile` (ours) minus 50 is the Pattern-construction gap. The `tc1` arm shows what the `Pattern.compile()V`
  optimizing body buys.
- `matchonly` and `cached` minus `compile` belong to the regex page.
- `list` and `comma` minus `list` are the per-element allocation share.

Confirm the `tc1` arm with `CRATONVM_DBG_JITC=1`: `c2-supersede published java/util/regex/Pattern.compile()V` should
show a c2 size different from 61317.

**Exact remaining work for this page:**
1. Run the table above on n23-w8.
2. If `compile` is a large share, the next site is the in-place type-check trap, flipped through the proxy page.
3. If `cached` minus `compile` dominates (the expected case), this page closes with the regex page's `Node.match`
   work (L5).
4. If `list` / `comma` stays more than 3x HotSpot, the per-element cost (`StringLatin1.newString`, the
   `ArrayList.grow` copy, `SubList.toArray`) needs a RIP sample of the published `Pattern.split` /
   `String.split(CIZ)` bodies, as wave 7 item 2 says.

## Wave 9 (lane W9-L6, 2026-10-22): `substring` is a real call in every split, because one cold `ldc ""` is unresolved

**The w8 numbers** (n23-w8, `ab8-w8b.log`, defaults, 3 reps):

| row | n23-w8 | HotSpot |
|---|---|---|
| `W8L6SplitParts full` | 8920-15795 | 581-627 |
| `compile` | 1706-3617 | 47-49 |
| `RxLoop split-comma` | 1306-1981 | 126-260 |

After R23-W8-FLIP, the coordinator reports `full` 7184-7360. Compile is now about 20-25 % of `full`, and the rest is
`Pattern.split` plus matching.

**The defect (a per-element cost, both split paths).** `String.substring` calls
`StringLatin1.newString([BII)`, which begins with `if (len == 0) return "";`, an `ldc #148` at pc 4.
- A split that never produced an empty part never runs that `ldc`, so its `CONSTANT_String` entry is never
  resolved.
- The optimizing tier's splice resolver refuses any body with an unresolved String `ldc`. The w6 trace
  (`C:\craton\n23p\w6L1\jitc_X=1.txt`) shows it as `inline-resolve REFUSED java/lang/StringLatin1.newString([BII)...:
  ldc-non-numeric-constant (callee pc 4, cp #148: not resolved or no slot)`.
- So in the published `String.split(CIZ)` and `Pattern.split` bodies, every `substring` is a spliced shell around a
  real call to `newString`. Inside that call are `Arrays.copyOfRange` and `new String`, which escape analysis and the
  allocation path never see. HotSpot inlines that path whole.

**The fix (`CRATONVM_JIT_IR_SPLICE_UNRESOLVED_LDC_EXIT`, JIT, perf, default ON, kill `0`).** This is the
unresolved-`new` splice exit (`CRATONVM_JIT_IR_SPLICE_UNRESOLVED_NEW_EXIT`) applied to `ldc`:
- **VM (`resolve_inline_site_from`, `jit_bridge.rs`).** An unresolved String `ldc` behind a branch edge the callee's
  own profile says never ran (`splice_typecheck_arm_unexecuted`, at least 8 samples on the edge that did run) is sent
  as a slot-0 `ir_ldc_ref_rows` row. A `splice-unresolved-ldc-exit` line is printed under `CRATONVM_DBG_JITC`.
  - Any other unresolved `ldc` is refused as before.
  - Class constants are never sent this way.
- **jit (`append_ir_inline_site`).** The slot-0 String row goes into `unloaded_static_call_pcs` if a chain snapshot
  exists (`unloaded_static_call_trap_admitted`). Otherwise the body keeps `ldc-non-numeric-constant`. A malformed
  slot-0 row (a Class row, no holder, not on an `ldc`) refuses the body as `ir-splice-ref-ldc-row-mismatch`. The pure
  rule is `splice_unresolved_ldc_row_verdict`.
- **ir (`IrBuilder` ldc arm, `plant_unresolved_ldc_exit`).** The builder plants an always-failing guard at the
  `ldc`, resumed through the chain snapshot, so the interpreter resolves (interns) `""` in the callee's frame. It is
  stamped `DeoptReason::ClassLoading`, so the first firing asks for a recompile. A `Ref` null is pushed for the dead
  tail.
- **Byte-for-byte.** Resolution still happens only in the interpreter, at the same bci, when the arm first runs.
  Nothing of the splice is replayed. The constant's value and identity are unchanged (the intern table).

**Tests:**
- `jit/src/lib.rs` `r23w9_l6_unresolved_ldc_row_tests` (the row verdict, 3 tests).
- `r20_zldc_splice_ref_ldc_append_tests::an_unresolved_string_row_without_a_chain_keeps_the_refusal`.
- `jit/src/ir.rs` `r23w9_l6_unresolved_ldc_exit_tests`: the `newString` bytecode's pc 4 is cold under a
  never-fell-through profile and warm otherwise, and the switch defaults.

**What to build and A/B.** Build n23-w9 and run `bash C:/craton/n23p/w8L6/ab9L6.sh <bin> 3`. It runs every
`W8L6SplitParts` row, `R23RefArraycopy split*` and `RxLoop split-comma`, on two arms: default, and
`CRATONVM_JIT_IR_SPLICE_UNRESOLVED_LDC_EXIT=0`.
- Expected: `comma`, `split-comma-10`, `RxLoop split-comma` and `cached` / `full` move. `compile`, `matchonly` and
  `list` stay flat (`list` makes no substrings).
- Confirm with `CRATONVM_DBG_JITC=1` on `RxLoop split-comma`: you should see `splice-unresolved-ldc-exit
  java/lang/StringLatin1.newString([BII)... callee_pc=4`, `[ir] unresolved-ldc exit at combined pc ...`, and no
  `ldc-non-numeric-constant (callee pc 4` for `newString`.
- Facts: the w5 battery list, both modes, plus a probe that does split with empty parts (`"a,,b".split(",")`, and
  `"".substring(0,0)` after warm-up) to fire the exit once. Expect SAME, and one `ClassLoading` deopt followed by a
  recompile.
- Before building: regenerate the flag docs (`docs/flag-tokens.md`, `docs/config/flag-inventory.md`) for the new
  row.

**What is left after this:**
1. `compile`, which is about 35x HotSpot (1.7 us against 48 ns). With `Pattern.compile()V` in the optimizing tier
   since w8, the next step is a RIP sample of the `compile` row. The candidates are:
   - the megamorphic `Node.study` chain (`callee-unresolved` 608/615, L5's dispatch area);
   - `Pattern.<init>`, pinned single-pass by its cold-arm `StringBuilder` sites;
   - the per-call `CharPredicates.ASCII_SPACE()` (IR refused at pc 0, `invokedynamic`).
2. `matchonly`, which is L5's `Node.match` page.
3. If `comma` is still more than 3x HotSpot after this, RIP-sample the `String.split(CIZ)` C2 body. The 538-byte
   spliced `indexOf(II)` spends 40 % of the splice budget.

## Wave 9b (lane W9-L6, 2026-10-22): `compile` runs single-pass because its C2 body is refused at publication

**The w9a decision on the ldc exit.** `CRATONVM_JIT_IR_SPLICE_UNRESOLVED_LDC_EXIT` was made opt-in because n23-w9a
showed no win. It does engage: on n23-w9b with `=1`, `RxLoop split-comma` prints
`splice-unresolved-ldc-exit java/lang/StringLatin1.newString([BII)... callee_pc=4` and
`[ir] unresolved-ldc exit at combined pc 97 in java/lang/String.substring(II)`, so `newString` is now spliced into
`substring`. It is neutral because the next level stays a call:
`inline-resolve REFUSED java/util/Arrays.copyOfRange([BII)[B depth=1: static-intrinsic-keeps-its-own-fast-path`.
The byte copy and the `byte[]` allocation are still behind that call, which escape analysis cannot see. It stays
opt-in. Making it pay needs `copyOfRange` spliced or lowered in place, which is out of this lane's scope
(arraycopy lowering).

**RIP sample of `W8L6SplitParts compile`** (w9base `cratonvm.exe` with its pdb,
`C:\craton\n23p\w8L6\tools\RawCaller.exe`, a `StackSampler` copy that names the JIT return address under a focus
frame; `CRATONVM_JIT_PERF_MAP=1` maps it to a method):
- Self samples in the native door: `safe_native_call_impl`, `try_lean_native_door`, `LEAN_NATIVE_SITES`,
  `enter_excusable_native`, `CURRENT_NATIVE` and similar. This is 30-50 % of the row.
- The only native under it is `native_character_char_count`.
- Both JIT return addresses that call into the door fall inside `java/util/regex/Pattern.compile()V [c1]`. That is
  the single-pass body. In it, `x += Character.charCount(c)` at pc 114 is a registered `Intrinsic` native, and it
  goes through the native door.
- The optimizing body lowers that site as arithmetic (`scalar-intrinsics ... 1 site(s) lowered as arithmetic`).
  It is simply not the body that runs.

**Why the optimizing body does not run.** The perf maps of four default runs on w9base list no
`Pattern.compile()V [c2]` (1569-1798 ns/op). With `CRATONVM_JIT_SP_CODE_POINT_SCALAR=1` (only a different timing),
four of four runs have it (971-1165 ns/op). A `CRATONVM_DBG_JITC` run of the losing kind shows why:
- Every C2 attempt builds the 110 KB body (`full-compile ... len=109889`).
- `put` then drops it: `jit-cache-put DROPPED ... (ir): baked-callee-retired (prepare_for_publication)` and
  `c2-publication REFUSED -- the single-pass body stays live`. While the large body compiles, its callees
  (`expr`, `curly`, `Start.<init>`, `newCharProperty` and others) keep republishing, so one of its baked direct-call
  targets has always retired by the time it publishes.
- Each drop is charged to `tier_fail_count` (budget 3). After the third drop the retry answers
  `refused-publication retry REFUSED ...: tier_fail_count exhausted`, and the method keeps its single-pass body
  for the life of the process.
- So a publication race decides the row. It is bistable from run to run, which is why the w8/w9 `compile` numbers
  scatter from 1.0 to 3.6 µs.

**The fix (`CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND`, JIT, perf, default ON, `=0` kill).**
- When a method-entry C2 body is refused at `put` (`optimizing_publication_refused`, which covers both a retired
  baked callee and a moved dependency), the worker calls `CompilerCore::refund_refused_publication`. It does so
  after `finish` has charged the attempt and before the `refused-publication` retry.
- The refund hands one `tier_fail_count` charge back, up to `MAX_PUBLICATION_REFUSAL_REFUNDS` (6) per method
  (`MethodState::publication_refusal_refunds`).
- So the retry keeps going until the callees settle. The attempt count stays bounded at 3 + 6.
- A `refused-publication refund` line is printed under `CRATONVM_DBG_JITC`.
- Code: `jit/src/tiered.rs` (`publication_refusal_refund`, the pure rule;
  `c2_publication_refusal_refund_enabled`; the worker call).
- Tests: `r23w9_l6_publication_refusal_refund_tests` (4 tests: the refund, off and uncharged, the bound of 9
  attempts, and the switch's default and kill).

**Expected effect:** `compile` settles at the c2 numbers, about 1.0-1.2 µs instead of 1.6-1.8. `full` and
`split-ws` lose the same share. Other large methods whose C2 attempts lose this race during warm-up can now
publish too.

**What to build and A/B:** run `bash C:/craton/n23p/w8L6/ab9bL6.sh <n23-w9c.exe> 5`. Each run writes a perf map, and
the `c2=` column counts `Pattern.compile()V [c2]` entries. The script was smoke-run on w9base, where the switch is
absent, so `def` and `ref0` there are the same binary. The arms are:
- `def`;
- `ref0` (`CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND=0`);
- `spcp1` (`CRATONVM_JIT_SP_CODE_POINT_SCALAR=1`, an older opt-in that answers `charCount` inline in the
  single-pass body; the r22 A/B found no measurable effect, so it stays off unless it wins here);

each on the `compile` and `full` rows plus `RxLoop split-ws`.

Expected: `def` shows c2>0 in every run, while `ref0` sometimes shows c2=0 with the slow `compile`.

Then confirm one run with `CRATONVM_DBG_JITC=1`: look for `refused-publication refund
java/util/regex/Pattern.compile()V` followed later by `c2-supersede published java/util/regex/Pattern.compile()V`.

**Battery:** the w5 battery list in both modes. Only tiering changes; the compiled code does not. Also the JIT
compile-time rows (`CRATONVM_DBG=jit-method-stats` C2 attempt counts), to check that the extra retries do not keep
a compile thread busy on a method that never publishes. A method that never publishes is bounded at 9 attempts.

**Still open after this:** with C2 published, `compile` is about 1.0 µs against 48 ns. The next RIP sample should
be taken on a run whose perf map shows `[c2]`. The remaining suspects are:
- `jit_instanceof`, 11.6 % self in one sample: the `topClosureNodes` / `instanceof` sites and the `Pattern$Node`
  type checks;
- `jit_zgc_note_tlab_object` / `is_object_address`: per-allocation ZGC bookkeeping;
- the `Node.study` chain (L5's area).

## Wave 9c (lane W9-L6, 2026-10-23): the refund is opt-in

**The A/B (orchestrator, n23-w9c, 3 reps, noisy host; ns/op):**

| row | default (refund ON) | `CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND=0` |
|---|---|---|
| `W8L6SplitParts compile` | 1486 / 1075 / 1170 | 1607 / 1491 / 1125 |
| `W8L6SplitParts full` | 8824 / 7145 / 6586 | 6382 / 6810 / 6304 |
| `RxLoop split-ws` | 7686 / 10642 / 7188 | 11750 / 7492 / 5751 |

The result is mixed: `compile` won 2 of 3, but `full` lost all 3. **The switch is now opt-in** (`=1`) and kept
(`tiered::c2_publication_refusal_refund_enabled` reads `runtime_flag_on`, the row has `off_word: None`, and the
test is `the_switch_is_opt_in`).

**What the refund did on `full`** (`CRATONVM_DBG_JITC=1`, n23-w9c, 3 runs per arm; the debug runs read 8.8-13.5 us
in both arms):

| arm | C2 requests | C2 publications | dropped puts | refunds | `tier_fail_count exhausted` | `Pattern.compile()V` C2 published |
|---|---|---|---|---|---|---|
| refund ON | 112 / 116 / 113 | 86 / 87 / 91 | 13 / 24 / 11 | 8 / 8 / 5 | 0 | 4 / 3 / 6 times |
| `=0` | 104 / 111 / 118 | 81 / 88 / 90 | 20 / 21 / 18 | 0 | 0 | 4 / 5 / 2 times |

- On the `full` row, `Pattern.compile()V` publishes its C2 body in both arms, and nothing ever reaches
  `exhausted`. The publication race that the refund answers is a `compile`-row (warm-up order) phenomenon. It did
  not occur on `full`, so the refund cannot help there.
- The refund still fires, for the other methods that lose a race (`ArrayList.add`, `Matcher.find`,
  `Pattern.split`, `Pattern.closure`, `curly`, `isSupplementary`, `BmpCharPropertyGreedy.<init>`), and it adds a
  few C2 compiles and republications per run.
- Each republication retires a body that other callers baked, which can refuse their own publications in turn.
  That is the plausible cost on `full`: compile-thread time and stale-caller churn in the same warm-up window as
  split's own bodies. The counts above are too close to prove it, so the honest reading is "no win on `full`".
- `Pattern.compile()V` itself republishes 2-6 times per run in both arms (`c2-supersede published ... c1=110891
  c2=110891`, with `stale-caller recompile` / `late-bind` as the trigger). This churn exists without the refund
  too, and it is the next thing to look at for the `compile` row.

**Remaining work on `compile` (about 1.0-1.5 µs against 48 ns):**
1. The republication churn of `Pattern.compile()V`. It is a 110 KB body, recompiled whenever a callee it baked
   republishes (`stale-call rebind DECLINED ... straddles an 8-byte word` sends the caller to a recompile). A rebind
   that can patch a straddling call (or alignment of baked call displacements) would end it.
2. With the body settled, take a RIP sample of a `[c2]` run: `jit_instanceof`, the ZGC TLAB note, and `Node.study`
   (L5).

## Wave 10 (lane W10-L6, 2026-10-23): stale callers are rebound in place instead of recompiled

**What drives the churn.** I took the decline reasons from the three `CRATONVM_DBG_JITC` runs of `W8L6SplitParts
full` on n23-w9c (`C:\craton\n23p\w8L6\rip\w9c-full?.txt`), as `stale-call rebind DECLINED` reasons per caller.
Each decline sends its caller to a full recompile (`stale-caller recompile requested`):
- `the return site is not a direct CALL rel32`: 11 in `Pattern.compile()V` (the C2 body), 11 in
  `W8L6SplitParts.full`, 10 in `String.split(String,int,boolean)`, plus `Pattern.atom`, `expr`, `ArrayList.add`,
  `grow` and `StringLatin1.newString`. The optimizing tier's direct cross call is `MOV RAX, imm64 ; CALL RAX`,
  which the rebind did not know.
- `the call's displacement straddles an 8-byte word`: 11 in `String.split(String)`, 10 in `main`, 8 in `sequence`,
  6 in `Pattern.compile()V` (the C1 body), plus `Pattern.split`, `<init>`, `Matcher.<init>`, `subSequence` and
  `checkMatch`. These are single-pass `CALL rel32` sites whose rel32 crosses an 8-byte word (three alignments in
  eight).
- Against that, only 47 `stale-call REBOUND` succeeded in the three runs.

Every recompile of the 110 KB `compile()` body (about 1 s of compile time) republishes it, which retires bodies
that other callers baked, which declines and recompiles those callers. That cascade is the 2-6 republications per
run.

**The fix: three switches, all perf, default ON, `=0` kill.**
1. `CRATONVM_JIT_STALE_CALL_REBIND_ABS64`. `JitCache::rebind_stale_call_site` recognises the
   `48 B8 <imm64> FF D0` site that ends at the return address (`abs64_call_imm_offset`). It rewrites the imm64
   from the patched body's entry to the current body's entry (`rebind_stale_abs64_call_site`). The rules are the
   rel32 path's: the caller roots the old body, the record owns the target, the rewrite happens under the cache's
   `mutation` lock, the target is re-checked under that lock, the caller is listed and marked first, and the
   forward word is cleared for the first sites. A misaligned imm64 is refused.
2. `CRATONVM_JIT_IR_DIRECT_CALL_IMM_ALIGN`. `ir_lower::emit_direct_cross_call` puts one multi-byte NOP
   (`emit_multibyte_nop`, 0-7 bytes) before `MOV RAX, imm64`, so the imm64 starts on an 8-byte boundary.
3. `CRATONVM_JIT_SP_PATCHABLE_CALL_ALIGN`. The single-pass `emit_call_absolute` puts one multi-byte NOP (0-3 bytes)
   before a `CALL rel32` whose target is compiled code (`lookup_jit_code_range`), so the rel32 is 4-aligned and
   never straddles an 8-byte word. This is HotSpot's `align_call`. Helper calls, which nothing patches, are not
   padded.

**Why the patch is safe for a thread executing the site.**
- **One whole word.** Both forms now patch one naturally aligned 8-byte word, with one `LOCK CMPXCHG`. That word
  cannot cross a cache line, so another core's instruction fetch sees either the old bytes or the new bytes, never a
  mix. The Intel SDM vol. 3A 8.1.1 / 8.1.3 guarantee for aligned stores is the one the existing rel32 rebind and
  HotSpot's `NativeCall::set_destination_mt_safe` (4-aligned call displacements) already rely on.
- **No instruction boundary moves.** In the abs64 form the word is exactly the eight immediate bytes. In the rel32
  form the CAS keeps the word's other bytes, which are untouched instruction bytes.
- **The old target stays valid.** Without a serializing instruction, an executor may keep running the old
  immediate for a while. That old entry is the not-entrant body, whose stub forwards to the same current body. The
  caller roots that body and the record owns the target, so both addresses stay mapped while the caller lives.
- **Frame walks.** A thread past the `MOV` has RAX loaded and calls whichever entry it read. The caller is marked
  (`has_rebound_call_sites`) before the write, so `direct_call_callee` stops naming frames by decoding that site,
  in both forms (`direct_call_target_rel32` / `_abs64`).

**Tests:**
- `not_entrant.rs` `r23w10_an_aligned_abs64_stale_call_is_rebound`: hand-built bodies. The aligned site is
  rebound and then calls the new body; a skewed site is refused; `=0` is refused; a second rebind is refused.
- `r23w10_l6_abs64_rebind_tests`: the decode, non-matches, and the switch.
- `r23w10_l6_sp_patchable_call_tests`: the pad puts the rel32 in one word, and the switch.
- `ir_lower.rs` `r23w10_l6_direct_call_imm_align_tests` (the pad and the switch), plus
  `a_direct_calls_entry_imm64_is_eight_byte_aligned`, which lowers a real direct call and checks the published
  imm64's alignment.

**Second item, from a RIP sample of w10base** (`C:\craton\n23p\w10base\cratonvm.exe` with its pdb, `compile` row,
20 s, sampled from 10 s to 15 s). The run was the slow kind again: the perf map shows no
`Pattern.compile()V [c2]`, and the row read 2115 ns/op.
- **The native door, about 34 % inclusive.** This is the single-pass `compile()`'s `Character.charCount` (wave 9b).
  It goes away when the C2 body publishes, which the fix above should make the common case.
- **`jit_instanceof`, about 21 % inclusive.** Two callers, mapped through the perf map:
  - 3524 samples from `Pattern.newCharProperty(CharPredicate)` `[c2]`: its `p instanceof BmpCharPredicate`, whose
    receiver is the `CharPredicates.ASCII_SPACE` lambda proxy;
  - 1461 from the C1 `compile()`.

  A lambda proxy's class id is in the synthetic range (`>= 0x8000_0000`). The IR `instanceof` lowering
  deliberately sends that range to the helper, and the MHIFACE19-1 seen-class ways skip it (`JS -> keep`), so every
  call pays the helper.
  - Closing it needs a proven-stable per-class answer for lambda proxies: the proxy's interfaces never change, but
    the helper's instance-admission rules must be excluded.
  - That belongs to the typecheck area, so I filed no change here. It is the next item.

**What to build and A/B (n23-w10):** run `bash C:/craton/n23p/w8L6/ab9bL6.sh <bin> 5`, which reports the c2 column
per run, on two arms:
- default;
- all three new switches off (`CRATONVM_JIT_STALE_CALL_REBIND_ABS64=0 CRATONVM_JIT_IR_DIRECT_CALL_IMM_ALIGN=0
  CRATONVM_JIT_SP_PATCHABLE_CALL_ALIGN=0`).

Plus `RxLoop split-ws` / `split-comma`.

Expected:
- `compile` gets c2>0 in most default runs, with fewer `stale-caller recompile requested` and more `stale-call
  REBOUND` lines under `CRATONVM_DBG_JITC=1`;
- `full` / `split-ws` hold or improve, because the compile thread is no longer recompiling `compile()`;
- code size grows by about 1.5 bytes per compiled-code call (single-pass) and about 3.5 bytes per IR direct call.

**Battery:** the w5 battery list in both modes. Call sites are patched while threads run them, so also:
- the Spring Boot and netty fixtures;
- the moving-young lane (`CRATONVM_MOVING_YOUNG=1`), because frame walks decode these sites;
- a `CRATONVM_DBG_JITC` census of `stale-call REBOUND` / `DECLINED` on `full`, to confirm the declines are gone.

## Wave 11 (lane W11-L6, 2026-10-23): the parts table on n23-w10b, and the checked copy in `SubList.toArray`

**Measured on n23-w10b** (a scratch copy, `--jdk-only`, P-core pinned, 4 s per row; the host is loaded, and HotSpot
read 1.5-2x its wave-8 numbers in the same window). The `c2` column is `Pattern.compile()V [c2]` entries in the run's
perf map.

| row | n23-w10b r1 / r2 (ns/op) | c2 | HotSpot, same window |
|---|---|---|---|
| `full` | 10578 / 10270 | 1 / 1 | (581-627 in wave 8) |
| `compile` | 2238 / 1731 | 1 / 1 | (47-49) |
| `cached` | 8002 / 6820 | - | 1176 |
| `matchonly` | 3587 / 2997 | - | (355-378) |
| `comma` | 2020 / 1914 | - | 295 |
| `list` | 1101 / 916; 1436 / 1140 / 1180 in 3 more runs | - | 114 |

- `Pattern.compile()V` now publishes its C2 body in every run here (wave 10's rebind).
- `cached` minus `matchonly` (about 3.8-4.4 µs) is building the parts: substrings, list, `toArray`. That is more than
  the matching itself.
- `list` (17 `ArrayList.add`, then `subList().toArray(new String[n])`, no regex at all) is about 10x HotSpot.

**RIP sample of `list`** (w10base with its pdb; `C:\craton\n23p\w8L6\tools\RawCaller.exe` with
`RAW_CALLER_OF=jit_system_arraycopy_direct`; perf map):
- About 44 % of all samples are in the reference arraycopy helper: `try_reference_arraycopy_pair` /
  `copy_reference_slots_raw_checked`, plus `load_and_forward_inner` / `is_object_address` validating both arrays,
  the per-element provenance memo, and the `contain` / switch thread-locals.
- Every one of those helper calls (6132 of 6132 RAWCALLER samples) returns into `java/util/ArrayList$SubList.toArray(
  [Ljava/lang/Object;)[Ljava/lang/Object; [c2]`. That is its `System.arraycopy(root.elementData, offset, a, 0,
  size)`: `Object[]` into `String[]`.
- The inline reference-arraycopy prefix (`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY`) only copies when the component ids
  are equal or the destination is `Object[]`. Any copy that owes a store check went to the helper whole. The two
  `ArrayList.grow` copies (`Object[]` into `Object[]`) are already inline.

**The fix (`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_CHECKED`, JIT, perf, opt-in `=1`).**
`jit/src/ir_lower.rs`:
- `try_emit_ir_ref_arraycopy_prefix`: at the component compare, a mismatch enters a checked arm instead of
  declining.
- `emit_ir_ref_arraycopy_checked_arm` is the arm. It runs the same range checks (byte for byte), then copies
  forward one element at a time. For each element it loads the word, then:
  - a null is stored;
  - a non-null is stored only if its kind tag is plain `Object` and its class id equals `dst`'s component id (the
    exact-class proof; arrays and subclasses are not proven);
  - the first unproven element declines to the call with the original arguments.
- Why this is parity-safe:
  - The helper re-copies the prefix this arm already stored into the same slots of the same other array
    (idempotent: arrays with different component ids are never one array). It then proves, copies or throws
    (`ArrayStoreException`, partial copy) exactly as before. This is the argument the helper's own checked fallback
    makes.
  - Each element is proven from the word it stores, so a racing writer cannot slip an unproven reference in.
  - The collector gates are the unchecked copy's (no SATB window, `dst` young or no old objects). No call or
    safepoint runs between the gates and the last store.
- With the switch off, the bytes are the old ones (the JNE still pushes onto `decline`).
- Tests: `r23w11_l6_checked_ref_arraycopy_tests` (the switch is opt-in; the null skip `CHECKED_COPY_NULL_SKIP` = 18
  covers exactly the proof's bytes).

**A/B and battery for this switch.** Build, then run interleaved and pinned with
`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_CHECKED=1` against unset:
- `cd C:/craton/n23p/w8L6/cls && <bin> --jdk-only -cp . W8L6SplitParts list 4`, and the same with `comma`, `cached`
  and `full`;
- `cd C:/craton/n23p/w5L5/cls && <bin> --jdk-only -cp . RxLoop split-comma 3`;
- `cd C:/craton/n23p/w1L5/cls && <bin> --jdk-only -cp . R23RefArraycopy` (its rows must not regress: same-component
  copies never reach the arm).

Expected: `list` loses most of its helper share (about 40 %); `comma`, `cached` and `full` gain the same per-call
amount.

Facts: the w5 battery in both modes, plus an `ArrayStoreException` probe, which should print the same exception
and partial copy as HotSpot:
- `Object[] s = {"a", 1, "b"}; String[] d = new String[3]; System.arraycopy(s, 0, d, 0, 3)`, warmed so the caller
  is C2. Expect d = `[a, null, null]` and an `ArrayStoreException` with HotSpot's message.

Also the moving-young lane and G1 (the gates decline there exactly as for the unchecked copy).

**Closing rule for this page.** The gate (within 3x HotSpot) is not met on any row, so the page stays OPEN. What
is left, in order of size:
1. Building the parts. After this switch, measure what remains of `cached` minus `matchonly`: `substring`'s
   `Arrays.copyOfRange` call (`static-intrinsic-keeps-its-own-fast-path`, wave 9) and `ArrayList.grow`.
2. Matching (`matchonly`, L5's `Node.match` page).
3. `compile`, which is about 35x HotSpot even with C2; next is the `jit_instanceof` lambda-proxy helper call in
   `Pattern.newCharProperty` (wave 10).

## Wave 12 (lane W12-L6, 2026-10-23): `substring`'s byte copy inline

**The wave-11 A/B (orchestrator, n23-w11a):** `CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_CHECKED` won, so it is now
default ON. Times are in ns/op:

| row | default | `=0` |
|---|---|---|
| `list` | 424 / 267 / 350 | 550 / 536 / 618 |
| `cached` | 4547 / 3315 / 3609 | 9567 / 3624 / 4804 |
| `full` | 5762 / 4631 / 4703 | 6812 / 4915 / 7165 |

**n23-w11a now** (scratch copy, pinned, 4 s per row, one rep; HotSpot from the same minute):

| row | n23-w11a | HotSpot | ratio |
|---|---|---|---|
| `full` | 6300 | 779 | 8.1x |
| `compile` | 1179 | 60 | 20x |
| `cached` | 5533 | 759 | 7.3x |
| `matchonly` | 2362 | 502 | 4.7x |
| `comma` | 1294 | 184 | 7.0x |
| `list` | 703 | 114 | 6.2x |

**The lever: `String.substring`'s copy.**
- `substring` reaches `StringLatin1.newString`, then `Arrays.copyOfRange([BII)`, then `System.arraycopy(byte[] ->
  byte[])`. The w9b trace shows `copyOfRange` is never spliced (`static-intrinsic-keeps-its-own-fast-path`, for its
  `System.arraycopy`).
- `copyOfRange`'s own C2 body therefore calls `System.arraycopy`. The optimizing tier binds that call to
  `jit_system_arraycopy_direct`: one helper call per `substring`, with both arrays validated in it.
- The inline prefix (`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY`) only covered reference arrays. The single-pass tier
  copies the same bytes inline (`ArraycopyPrimitive`, `REP MOVSB`).

**The fix (`CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY`, JIT, perf, default ON, `=0` kill).**
`jit/src/ir_lower.rs` `emit_ir_byte_arraycopy_arm`, emitted at the start of the `System.arraycopy` prefix
(`try_emit_ir_ref_arraycopy_prefix`), ahead of the reference arm.
- It copies only when, at run time:
  - `src` and `dst` are non-null and different objects;
  - both kind-tags bytes are equal and are `byte[]`'s or `boolean[]`'s;
  - the positions and length pass the reference arm's range checks, at scale 1.
- Anything else falls through, with nothing written, to the reference arm and then to the call. The call owns
  every exception, the same-array memmove and every other element type.
- Primitive stores owe the collector nothing, so there are no gates. The arm therefore also runs on G1, where the
  reference arm has no plan.
- The copy is `BYTE_ARRAYCOPY_LOOPS`: eight bytes at a time, then single bytes, forward. Two different arrays never
  overlap.
- Wiring: the arm's one hit jump joins the reference arm's end, or is the prefix's join when no reference arm is
  emitted. The node-shape and carry checks now run before the collector-plan check, so the arm is emitted on every
  collector. Only the order in which the decline notes print changes.

**Verified without a build:**
- I executed `BYTE_ARRAYCOPY_LOOPS` natively from Python (`VirtualAlloc` + ctypes,
  `scratchpad/loopcheck.py`): every length from 0 to 40 copies exactly `len` bytes and writes nothing outside them.
- capstone decodes the arm's check bytes and wave 11's checked arm as intended. The checked arm's null skip lands
  on its store.

**Tests:** `r23w12_l6_byte_arraycopy_tests`:
- the switch's default and kill;
- `the_byte_copy_loops_copy_exactly_len_bytes`, which runs the loops from a finalized `ExecutableBuffer` for lengths
  0..40 and checks both guard regions;
- the arm admits exactly `byte[]` and `boolean[]`.

**What to A/B (n23-w12):** pinned, interleaved, default against `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY=0`:
- `cd C:/craton/n23p/w8L6/cls && <bin> --jdk-only -cp . W8L6SplitParts comma 4` (then `cached`, `full`, `list`);
- `cd C:/craton/n23p/w5L5/cls && <bin> --jdk-only -cp . RxLoop split-comma 3`;
- `cd C:/craton/n23p/w1L5/cls && <bin> --jdk-only -cp . R23RefArraycopy` (reference rows must hold).

Expected: one helper call less per substring, which matters most on `comma` / `split-comma` (10 substrings per op)
and `cached` (17).

**Battery:** the w5 list in both modes, plus a `byte[]` arraycopy probe set compared with HotSpot:
- an out-of-range copy (`ArrayIndexOutOfBoundsException` text);
- `byte[]` into `int[]` (`ArrayStoreException` text);
- a same-array overlapping copy;
- a zero-length copy;
- a `boolean[]` copy.

Run all of them with the caller warmed to C2. Also run on G1 (`-XX:+UseG1GC`): the arm is new there.

**Not done this wave:**
- `ArrayList.grow(I)` is refused for the IR splice (`new/anewarray/multianewarray`, its cold `new Object[...]` arm).
  It runs twice per 17-element list, so it is not the `list` gap; `list` is now 6x, and its remaining share was not
  sampled this wave.
- `compile`'s `jit_instanceof` on the `ASCII_SPACE` lambda proxy in `Pattern.newCharProperty` (wave 10) is
  unchanged. A per-site answer for synthetic (`>= 0x8000_0000`) receiver ids needs proof that a lambda proxy's
  answer depends on its class alone (the helper's instance-admission rules). That is typecheck-area work and is
  still the next item.

The page stays OPEN: every row is 4.7-20x HotSpot.

### n23-w12a result (orchestrator, 2026-10-23)

`CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY` was **neutral** and is now **opt-in** (`=1`). Pinned, 3 interleaved reps, `W8L6SplitParts`, ON against `=0`:

| Row | ON (ns) | `=0` (ns) |
|---|---|---|
| `cached` | 4066-4481 | 4115-4211 |
| `full` | 5237-5516 | 5287-5367 |
| `comma` | 645-651 | 631-648 |
| `list` | 253-264 | 258-262 |

Correctness held: the overlap, bounds, null, negative-length and `ArrayStoreException` probe matched HotSpot in both
modes, ON and OFF (`C:\craton\n23p\w12ac\AcCheck.java`). The helper call is not where the split body's time goes.

## Wave 13 (lane W13-L6, 2026-10-23): where `cached` and `matchonly` spend their time; `copyOfRange` was pinned to C1

**The wave-12 A/B (orchestrator, n23-w12a).**
- `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY` was neutral (`cached` 4066-4481 ns ON, 4115-4211 off) and is now opt-in.
  The sample below shows why: the copy it targets runs in a single-pass body, never in an optimizing one.
- L5's `CRATONVM_JIT_IR_KEEP_LOOP_SPLICE_CALLER` lost (`cached` 3216-3337 ns with it off) and is opt-in.

**RIP samples.**
- Setup:
  - the w13 release base: `C:\craton\tgt-n23w10\release\cratonvm.exe` with its pdb, copied to scratch;
  - `C:\craton\n23p\w8L6\tools\RawCaller.exe` with `RAW_RIP_OUT` (raw RIP stacks);
  - the run's `CRATONVM_JIT_PERF_MAP`;
  - `scratchpad/ripmap.py`, which maps each RIP and the innermost JIT frame to a perf-map method.
- Each run is 16 s, sampled from 8 s to 13 s, with about 25 000 samples.
- Every hot method in both rows runs as an optimizing (`[c2]`) body except `Arrays.copyOfRange([BII)`.

`W8L6SplitParts matchonly` (b13, 3145 ns/op; n23-w12a, 2804):

| where (innermost JIT frame) | share |
|---|---|
| `Pattern$Start.match` `[c2]`, with the `\s+` Greedy and lambda spliced (the `Node.match` chain) | 44 % |
| `matchOnly` `[c2]` (the `find` loop: `Matcher.search` spliced) | 21 % |
| native allocation under `Pattern.matcher` `[c2]`: `jit_new_object` 16 % incl., `refill_tlab` 7 %, `jit_post_alloc_init` 6 % | 18 % |
| `Matcher.<init>` `[c2]` | 8 % |
| `LastNode.match` `[c2]` | 5 % |

- On n23-w12a (no pdb) the same split appears, with `NATIVE(exe)` at 27 % under `Pattern.matcher`.
- A `find` pass costs one compiled call into `Start.match`. In there, the per-position Greedy work runs inline, with
  no megamorphic dispatch left on this row.
- The remaining gap is the `Start.match` loop body itself (L5's page) and allocating the `Matcher` (its `int[]`
  groups and locals, and the object through the `jit_new_object` helper with a post-allocation init).

`W8L6SplitParts cached` (b13, 7444 ns/op in the sampled run; 4208-5879 unsampled):

| where (innermost JIT frame) | share |
|---|---|
| `Pattern.split` `[c2]` self | 29 % |
| `Pattern$Start.match` `[c2]` | 25 % |
| `ArrayList.grow` `[c2]`: its `new Object[n]` through `jit_anewarray_object` | 22 % |
| `Arrays.copyOfRange([BII)` **`[c1]`** | 7 % |
| `StringLatin1.newString` `[c2]` | 6.5 % |
| `Matcher.<init>`, `LastNode`, `Arrays.copyOf`, `SubList.toArray`, `subList` | 7 % |

About `ArrayList.grow`:
- In `cached`, `jit_anewarray_object` is 20 % inclusive. Almost all of it is
  `refill_tlab_at_least -> carve_tlab_chunk_locked -> memmove` (17 %, of which the chunk zeroing is 12.5 %).
- The refills are ordinary: `CRATONVM_GC_STATS=1` shows 2926 refills of 512 KB in 4 s (about 1.6 KB allocated per
  op), and `pristine_bytes_skipped` about equals the bytes carved.
- `grow` is simply where the TLAB runs out, because on ZGC the optimizing tier allocates `Object[]` through the
  helper (`CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC` stays off).
- The heap size is not a lever: `-Xmx64m` / `-Xmx256m` read the same within noise.
- This is the allocation cost tracked on `r21-lean-allocation-rows-cost-is-outside-the-inline-sequence-FIXED-20261024.md`,
  not split's.

**Why `copyOfRange` runs single-pass** (`CRATONVM_DBG_JITC`, b13): `[ir] admission java/util/Arrays.copyOfRange([BII)[B:
pinned to the single-pass backend: it has a String access intrinsic here`.
- Its only String-family sites are the `StringBuilder.append` / `toString` calls of the cold
  `throw new IllegalArgumentException(from + " > " + to)` arm (pcs 26..57, jumped around by `ifge 58` at pc 23).
- So every `substring` runs `copyOfRange`'s `[c1]` body. That body's `System.arraycopy` is the single-pass inline
  copy (or its bail edge). The wave-12 IR byte-copy arm never runs, which is why it measured neutral.
- `Pattern.<init>`'s `Unknown flag` arm is the same shape (wave 9b).

**The fix (`CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS`, JIT, perf, default ON, `=0` kill).**
`jit/src/lib.rs`:
- `string_pin_invoke_ops` / `invoke_ops_outside_arms` remove every invoke site inside a cold throw arm
  (`ir::cold_throw_arms`, the existing static shape: the arm a conditional branch jumps around, ending in its
  `athrow`, with no other entry) before the String-intrinsic pin is asked.
- They are applied at both of the pin's call sites in `try_compile_inner`: the printed verdict chain and the real
  conjunction (`string_intrinsic_pin_declines`). The reason printed and the decision taken still agree.
- A body whose throw shape the detector refuses keeps all its sites, as before.
- In the optimizing body, the arm's `StringBuilder` calls are ordinary calls. `StringBuilder` is not in the planner's
  intrinsic refusal or in `string_site_bails_ir_planning`. Never-run sites take the unreached-call trap where that
  applies.
- Only the tier changes, not any answer.

**Tests:** `r23w13_l6_string_pin_cold_arm_tests`:
- the pure filter on the arm 26..57;
- the real JDK 25 `copyOfRange([BII)` bytecode (82 bytes, hand-transcribed from `javap`) through `cold_throw_arms`:
  the four message sites are dropped, the `System.arraycopy` site stays, and `=0` keeps every site.

**What to A/B (n23-w13).** Pinned, interleaved, three arms:
- default;
- `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS=0`;
- default plus `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY=1` (the byte-copy arm should now be reachable, because
  `copyOfRange` is optimizing).

Rows:
- `cd C:/craton/n23p/w8L6/cls && <bin> --jdk-only -cp . W8L6SplitParts cached 4` (then `comma`, `full`, `compile`
  for `Pattern.<init>`);
- `cd C:/craton/n23p/w5L5/cls && <bin> --jdk-only -cp . RxLoop split-comma 3` and `split-ws 3`.

Check engagement with `CRATONVM_DBG_JITC=1 ... cached 4 2>&1 | grep "admission java/util/Arrays.copyOfRange"`: it
should print `admitted to the optimizing pipeline`, and the perf map should show `Arrays.copyOfRange([BII)[B [c2]`.
Because the switch moves every method whose only String-family sites are in a throw arm, also run a broad battery
row (the w5 list in both modes, plus Spring Boot or the R16 rows).

### Wave 13 result (n23-w13a, 3 pinned reps): the pin switch LOST

| row | default (switch on) | `=0` | on + `INLINE_BYTE_ARRAYCOPY=1` |
|---|---|---|---|
| `W8L6SplitParts cached` | 8232/6360/6684 | 5958/4762/5716 | 6173/5821/6454 |
| `comma` | 2113/1584/1813 | 967/1045/1063 | 1031/1180/1422 |
| `full` | 10976/7823/11872 | 6024/7719/7255 | 13338/8115/9935 |
| `RxLoop split-comma` | 2019/1890/1742 | 949/1339/988 | 999/970/1020 |
| `RxLoop split-ws` | 9859/9476/10126 | 7452/12767/7302 | 7491/6988/6958 |

`copyOfRange`'s optimizing body without the inline byte copy is slower than its single-pass body; with
the inline copy it only catches up. `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS` is now opt-in. Next
lever: make the IR `copyOfRange` body at least as fast as the single-pass one (the arraycopy call edge)
before the pin is relaxed again.
