# rx27 wave 3 review: residuals not fixed in code

Status: OPEN. Area: JIT String prefix carve-out, scan-loop hoist, chain resume. Severity: perf (items 1-2),
none known for correctness. Found by rx27 lane R reviewing `3362784bb9..4e1dcc0533` on 2026-10-28.

Fixed on `rx27-review` in the same review (not repeated below):

* `b989d085fe`: `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST=1` ignored an explicit `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST=0` /
  `_ENTRY_BOUND=0`. Test: `rx27_the_outer_hoist_yields_to_an_explicit_kill`.
* `62adeeb7e5`: the nested String-prefix strip dropped a `CharSequence.subSequence` body that the profile had
  guarded on another class (`StringBuilder`, `CharBuffer`). Test:
  `a_nested_char_sequence_sub_sequence_keeps_a_non_string_body`.

## 1. The top-level carve-out still keeps every `CharSequence.subSequence` site a call

**FIXED in code (lane splitfast, wave 3b, 2026-10-28; pending build).** `ir_tier_attempt` now asks
`top_level_string_prefix_site_stays_call` (`jit/src/lib.rs`): a `SeqSubSequence` site stays a call only when its
receiver profile has no row or String holds at least half of it (and there is a String layout); otherwise it falls
through to the guarded planner and prints `ir-splice string-search-prefix-site java/lang/CharSequence.subSequence...
receiver profile not String, left to the guarded planner` under `CRATONVM_DBG_JITC=1`. Every other kind is
unchanged. Tests: `rx27_review_top_level_seq_subsequence_tests::{a_non_string_profiled_seq_subsequence_site_keeps_its_splice,
other_string_prefix_kinds_are_unchanged}` (the first fails on the old carve-out: every profile stayed a call).
Host check as below; retire this item once a `Pattern.split(StringBuilder)` row shows the line and its splice.

`jit/src/lib.rs` `ir_tier_attempt` (`ir-splice-refused string-search-prefix-site`) refuses the splice of any
site whose kind stays a call, before the receiver profile is read. Since `CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX`
became default ON, a top-level `CharSequence.subSequence` site whose profile is `StringBuilder` or `CharBuffer` (for
example `Pattern.split` over a `StringBuilder`) loses its guarded splice. The prefix then declines that receiver
to the interface call. The guarded `Seq*` accessors were deliberately left out of `ir_string_prefix_site_stays_a_call`
for this reason.

**What would close it:** keep the top-level `SeqSubSequence` site a call only when its receiver profile is
String-dominant, or has no row. Otherwise, fall through to the guarded planner. Host check:
`CRATONVM_DBG_JITC=1` on a `Pattern.split(StringBuilder)` row, then
`grep 'string-search-prefix-site java/lang/CharSequence.subSequence'`.

## 2. The outer hoist's `coder == LATIN1` guard on a UTF16 subject

`charat_trap::hoist_one` (lane scanloop) charges the new coder guard as a `Class` predicate. It names the guard
in `chain_predicate_guards` only for a spliced header, which is the same as the String class guard beside it. For a
flat header, withdrawal goes through `predicate_withdrawn(.., header bci, Class)` in the plant pass. A UTF16 subject
should therefore deopt a bounded number of times and then recompile without the predicate. This has not been
observed on a binary.

**Host check:** `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST=1 CRATONVM_DBG_DEOPT=1 CRATONVM_DBG_JITC=1` on
`tools/probes/Rx27ScanLoopSubjects.java`, then:
* count the deopt lines at the `Pattern$Start.match` header;
* confirm that a later compile prints `refused ... class speculation was withdrawn`;
* confirm that the facts equal HotSpot.

## 3. A redefined spliced scope with an exception table now refuses resume

`splice_scope_sources` (lane matcher) now drops the row of every body whose own code has an exception table.
Before, it emitted a template with no table: a resumed frame could not catch its own exception, which was a
wrong answer. So this change is a fix. It also means that a chain trap in a redefined class's handler-bearing
body now takes the VM's refusal path.

**What would close it:** a template carrying the original table. Host check: the redefinition battery
(`CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE` default) with `CRATONVM_DBG_DEOPT=1`, grep `own source`.
