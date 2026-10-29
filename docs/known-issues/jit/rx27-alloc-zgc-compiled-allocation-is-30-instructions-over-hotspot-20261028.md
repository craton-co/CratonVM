# A compiled allocation on ZGC is about 30 instructions over HotSpot's (the regex / split rows' allocation share)

Status: OPEN (round rx27, lane alloc, 2026-10-28; by reading, not measured). Wave 3b built two opt-in trims (below);
the per-object start bit remains.
Area: JIT optimizing tier, inline TLAB allocation on ZGC (`jit/src/runtime_lowering.rs`
`emit_inline_tlab_new_ir_impl` / `emit_inline_tlab_newarray_ir_impl_rt`, `jit/src/x64/objects.rs` start-bit store).
Severity: low for the open rx27 pages. It is about 5-8 % of the CratonVM instruction count on the allocating rows,
so it cannot close their 3x gate alone.
Parents: `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md` ("Round rx27 wave 1"),
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`.

## Round rx27 wave 3b (lane alloc, 2026-10-28; pending build): the lean start-bit store, opt-in

Commit `b97a250bfd` on `rx27-alloc`. Two switches, both opt-in, default bytes unchanged.

**`CRATONVM_JIT_ZGC_LEAN_START_BIT=1`**
- Long inline store (IR `new` / `newarray` / `anewarray`, through `emit_zgc_inline_announce_raw_form_end*`): the
  `blocked` load and test become one `cmp qword [T + short_blocked], 0` (3 -> 2 instructions).
- IR call-free pair (`concat_emit::emit_zgc_start_bit_precheck` / `emit_zgc_start_bit_store_prechecked`: the
  substring `byte[]` + `String` joint bump, `toString`, the `char[]` copies) gets the z11/s1 trims it never had:
  - the owned-word window per object, with no span or grid test;
  - the same gate;
  - no `and rcx, 63`.
  - Per object, pre-check 13 -> 7 and store 11 -> 10 instructions; the shared part 12 -> 11.
- IR `new` layout-replace guard: `mov eax, [moffs64]` (2 -> 1). This applies on every collector.

**`CRATONVM_JIT_ZGC_START_BIT_BTS=1`** (with the window): the bit by one `bts qword [words], rcx`
- long store 3 -> 1;
- lean pair store 5 -> 1.
- A memory `BTS` is microcoded, so judge it by instructions and by time.

**Totals.**

| allocation | default | `LEAN=1` | `LEAN=1` + `BTS=1` |
|---|---:|---:|---:|
| ZGC compiled `new` | about 45 | 43 | 41 |
| array | about 44 | 43 | 41 |
| substring `byte[]` + `String` pair, start-bit part only | 60 | 45 | 37 |

This is not HotSpot's shape (bump, compare, header stores). The remaining start-bit share needs the per-object
start registry to go, which the zr8st / zr9bo censuses refuted. See "What would close it" below; proposal 1
(allocation groups) is still the next lever.

**GC invariants relied on.** Nothing outside the emitters changed; header, then bit, then cursor, as before.
- An armed buffer under a matching epoch lies in `[base, base + span)`, and its window is non-empty only when
  every word index fits a dword (`zgc_inline_announce_for_chunk`, `zgc_owned_word_window`).
- Every object passed to the pair is from one bump that passed the TLAB end test, so `start <= obj < end`.
- `short_blocked == 0` implies `blocked == 0`: it is raised first, lowered last, and set first on withdrawal.
  It is closed on a base that is not 512-aligned, where the store declines to the helper.
- The word is owned, so the unlocked `or` / `bts` is the existing read-modify-write of the same word and bit.

**Engagement.**
`CRATONVM_DBG_JITC=1 CRATONVM_JIT_ZGC_LEAN_START_BIT=1 [CRATONVM_JIT_ZGC_START_BIT_BTS=1] cvm-<tag> -cp . R24RegexSplitRows split-ws 1 3 2>&1 | grep -c 'zgc lean start-bit: form=pair'`
should be above 0, and so should `form=long`. With the switches unset the count is 0.

**icount2 arms** (`/root/rx27/p`, the failing rows):
```
icount2.sh rx27-alloc-lean "regex-matches regex-find-email regex-find-ci split-ws cached matchonly regex-groups compile" \
  base:<tag>: lean:<tag>:CRATONVM_JIT_ZGC_LEAN_START_BIT=1 \
  bts:<tag>:CRATONVM_JIT_ZGC_LEAN_START_BIT=1,CRATONVM_JIT_ZGC_START_BIT_BTS=1 hs:hs:
```
- Expected effect: 32 allocations per op on `regex-matches` and about 50 on `cached` / `split-ws`, with 20 substring
  pairs on the split rows, so roughly -100 to -500 instructions per op. That is 1-3 %, inside the 15 % bar.
- A flip is not expected. Run it for the record, and as the base for proposal 1.

**Batteries before any flip.**
- `cargo test -p cratonvm-jit --lib zr9bo_short_announce_tests` and the jit lib suite with both switches exported.
  Expected failure under the exported switches: `concat_emit` `the_collector_prechecks_match_the_single_pass_bytes`,
  because the IR pair then intentionally differs from the single-pass copy.
- The ZGC rows: `Zr8StPeerHandoff`, `ConcurrencyUnderGcSweep`, `RandomLiveAcrossGc`, `Zr3JaRefArrayAlloc`,
  `Zs2Ca2AllocWindows`.
- `vm/tests/jit_cov06_array_allocation.rs` with `CRATONVM_REQUIRE_E2E=1`.
- `R24RegexSplitRows facts` in both modes.

## Evidence

At tip every allocation shape on the R24 rows should take the inline bump on ZGC, with no call on its fast path
(census: `ir new-site` / `ir newarray-site ... call=none`). The remaining cost is the sequence itself. The
split-route `new` (count from `docs/internal/fixed-bugs/jr21-alloc-compiled-allocation-cost-FIXED-20261025.md`, rechecked against tip):

| part | instructions |
|---|---:|
| layout-replace guard (`mov r11, imm64; mov eax, [r11]; cmp; jne`) | 4 |
| thread from the TLS mirror, null test | 3 |
| cursor, align test | 3 |
| `lea` end, `cmp` TLAB end, `ja` | 3 |
| header stores (class word, mark; legacy shape and hash add 2; an array adds length and hash) | 2-4 |
| start-bit store, long form with the z11/s1 trims, commit from RAX, `mov rax, r11` | about 29 |
| result to its home | 1 |
| total | about 45 |

HotSpot C2 needs about 10-20: load top, `lea`, compare end, branch, store top, mark and klass stores, plus the
zeroing of small bodies. CratonVM needs no zeroing, because the chunk is zeroed at carve. Allocations per op
(JDK 25 bodies, probe strings): `regex-matches` 32 (8 x `Matcher` + `int[]` + `int[]` + `IntHashSet[]`);
`cached` / `split-ws` about 50 (one `Matcher`, `ArrayList` and three `Object[]` grows, 20 x `String` +
`byte[]`, `subList`, `String[20]`); `matchonly` 4. At about 30 over HotSpot each, that is about 1 000
instructions per op on `regex-matches` and 1 500 on `cached`. Their gaps are 10 000+ per op.

## What was already tried (do not redo)

All of these were decided on wall time or process CPU. The rx27 gate counts user-mode instructions.

- Lazy start registration at TLAB retire: refuted by the query census (1-2 k header reads per peer query),
  `docs/internal/gc/zgc-zr8st-lazy-tlab-starts-remaining-steps-20261013.md`.
- Card start index: refuted by the same census, `docs/internal/proposals/gc/tier4/zgc-zr9bo-proposal-card-start-index-refuted-by-the-query-census-20261013.md`.
- Epoch gate (5 fewer per object): flat on the host three times, retired (zt t12/rt). Aligned chunks with a
  biased bit address: refuted with it, `docs/internal/gc/zgc-zs2ca2-proposal-aligned-chunks-biased-bit-address-20261014.md`.
- Short form (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE=1`, opt-in): 29 instructions with the z11 trims, the same as
  the long form. No instruction gain.

## What would close it (proposals, not built)

1. **Allocation groups.** Two or more inline allocations with no call, safepoint or deopt point between them:
   `String` + `byte[]` in `StringLatin1.newString`, and `Matcher`'s three arrays once `Matcher.<init>`
   splices (`CRATONVM_JIT_IR_SPLICE_ANEWARRAY`). Such a group bumps once, writes every header, tests the
   announce table once, sets each object's bit, and commits once after the last bit. Saving: about 15 per extra
   object.
   - GC invariant it must keep (nio21 w10/w12, zr z11): for every object, header before start bit before the
     cursor commit. A peer frozen anywhere in the group must own each object either registered or inside its
     published tail `[cursor, end)`. Committing once after the last bit keeps that.
   - A refusal after some bits are set must commit through the helper arm for the remaining objects only.
     This is the hard part.
   - Battery: ZGC frozen-peer rows (`Zr8StPeerHandoff`, `ConcurrencyUnderGcSweep`, `RandomLiveAcrossGc`);
     `R21FlipRows`, `R21AllocRows` and `Zs2Ca2AllocWindows` on ZGC, G1 and Generational;
     `vm/tests/jit_cov06_array_allocation.rs` with `CRATONVM_REQUIRE_E2E=1` under every collector; facts
     battery in both modes.
2. **Layout guard as a dependency.** Drop the 4-instruction guard. Instead, invalidate the body when a baked
   class's compact layout is replaced (`register_class_layout` overwrite), as HotSpot deoptimises on class
   redefinition. GC invariant: the bump size equals the registered layout's `total_size` for as long as the
   body is installed. Battery: redefinition / `ClassStore` multi-domain tests, `RJitMapTierDiff`.
3. Measure first. Before any of this, run `icount2.sh` with `perf annotate` of `Pattern.matcher` and
   `StringLatin1.newString` `[c2]` on the host. If the allocation sequences are under 5 % of the rows'
   instructions, close this page as not the lever.
