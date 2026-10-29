# `compile` row: which Pattern parse bodies stay calls, and field rereads across calls

**Status:** OPEN (round rx27 waves 5 and 6, lane chain, 2026-10-28). One opt-in switch is built, pending build and
measurement. The census for item 1 has not been taken yet; it is the first thing to run.

**Area:** splice admission of the `Pattern` parse bodies; the load CSE (`ir_optimize::memory_chain_reaches`);
`jit/src/lib.rs` `mod rx27_call_write_summary`; `vm/.../jit_bridge.rs` `call_field_write_summary`.

**Severity:** medium. `compile` is the worst row (5.5x HotSpot, 4 381 against 800 instr/op, gate 2.0k). The
profiler's page (`rx27-w4-per-row-instruction-breakdown-20261028.md`) gives this lane 0.4k (helpers) and
0.54k (field rereads).

## 1. The parse bodies: sizes and why each one stays a call (JDK 25 `Pattern.class`, read from the jmod)

| body | bytecode | exception table | loop | stays a call because (by reading) |
|---|---:|---:|---|---|
| `compile()V` | 537 | 0 | yes | compile root |
| `RemoveQEQuoting()V` | 370 | 1 | yes | over 325 (`too-large`), and an exception table |
| `escape(ZZZ)I` | 1 306 | 0 | no | over 325 |
| `sequence(Node)Node` | 647 | 0 | yes | over 325 |
| `atom()Node` | 422 | 0 | yes | over 325 |
| `closure(Node)Node` | 276 | 2 | yes | typed handlers (`multiplyExact` -> `error`) |
| `expr(Node)Node` | 142 | 0 | yes | **unknown**: within 325; needs the census |
| `curly(Node,I)Node` | 66 | 0 | no | **unknown**: within 325; needs the census |
| `peek()I` 26, `next()I` 33, `read()I` 33, `skip()I` 23, `unread()V` 11, `has(I)Z` 15, `mark(I)V` 11, `nextEscaped()I` 19 | | 0 | no | none expected: within the cold cap, and none of them is a profile symbol |
| `accept(ILString;)V` 43, `peekPastWhitespace(I)I` 56 (loop), `qtype()` 39 | | 0 | | just over 35 bytes; reached only on `COMMENTS` / quantifier paths |

None of the small helpers the profile page names (`has`, `peek`) appears as a symbol in its `compile` table,
so they are most likely spliced already. The bodies over `FreqInlineSize` (325) would not be inlined by C2's
size rule either. So the per-call cost there is the frames and prologues (bimodal's lever) plus the field
rereads (item 2). The two in-range bodies that stay calls are `expr` and `curly`. Their refusal reason
needs the census:

```
CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-$T -cp /root/rx27/p R24RegexSplitRows count compile 20000 20000 2>&1 \
  | grep -E 'inline-resolve REFUSED java/util/regex/Pattern\.(expr|curly|closure|peek|next|has|accept|qtype)|ir-splice-refused .*Pattern\.(expr|curly|closure)|ir-splice-cold-nested|keep-loop|ir-splice-anewarray-loop-callee'
```

Candidates, by reading:
* `expr` has a loop, so the keep-loop-splice-caller rule (bimodal's) or the W13-6 loop-root rule may apply;
* `curly` allocates `BmpCharPropertyGreedy` / `Curly`, so `ir-splice-new-site-class-init-pending` may apply;
* either one may hit the store-replay fence (`ir-splice-trap-after-store`).

## 1b. The census (cvm-rx27b6, wave 6) and what each refusal is

The host census of `count compile 20000 20000`, read refusal by refusal:

* **`next` / `peek` / `nextEscaped` / `peekPastWhitespace` `array-load [tier=sp]`** (42 sites). These refusals
  come from the single-pass inliner, in the first-tier compiles of the parse methods. The steady state does
  not run those bodies: the profiler's page says every sampled body on these rows is `full/ir`, and the IR
  tier splices these helpers (none appears as a profile symbol or in a `[tier=ir]` refusal).
  * Teaching the single-pass inliner array loads would only shorten the warm-up.
  * IR admission of the parse methods is already the steady state.

  **Decision: neither change.** No switch.
* **`ir-splice-cold-nested ... qtype ... (kept as calls)`** (and the same lines under `peek`, `next`, `atom`,
  `sequence`, `closure`). The count is of NESTED bodies under the named top-level site:
  * each `next()` / `peek()` keeps its `peekPastWhitespace` (behind `has(COMMENTS)`) as a call, and `qtype`
    has three `next()`s, so 3;
  * `closure` keeps its `{n,m}` `read()`s;
  * `sequence` keeps its error-message `StringBuilder.append`s.

  These are calls on paths `\s+` never takes. They cost nothing per op, and C2 would not inline an
  unreached site either. **Correct as is.** No switch.
* **`curly` `ir-splice-new-site-unresolved (callee pc 52)`.** pc 52 is `new Curly`. `Pattern$Curly` is not
  loaded, and two never-run edges lead to it: the `qtype != GREEDY` edge at 9, and the
  `!(prev instanceof CharProperty)` edge at 36, which sits behind the `!(prev instanceof BmpCharProperty)`
  edge at 16. The cold-arm proof (`pc_behind_unexecuted_branch_edge`) cut one edge at a time, so it failed
  on both.
  **Fixed (opt-in): `CRATONVM_JIT_IR_SPLICE_COLD_ARM_EDGE_SET`** cuts every proven never-run edge together.
  The site is the same precise class-loading exit as before; only the proof's reach changes. The
  `cp-resolved-method-is-private [route=receiver=...]` line beside it is the guarded route's attempt after
  the constant-pool route refused. A private method is bound, so that line is not a separate defect.
* **`expr` `ir-splice-not-single-trailing-return`.** `expr` is `for (;;) { ... if (peek() != '|') return
  head; next(); }`: one return inside the loop, and the body ends in a backward `goto`. The resolver admits
  a trailing back `goto` only for a multi-return body admitted with handlers (the builder closes the walk
  after such a `goto` only when it has seen two or more returns). Admitting the one-return shape needs that
  builder close to accept a single reachable return. That is builder work in the multi-return close, not
  done here (no unit fixture of the builder's close was at hand to change it safely). `expr` runs once per
  `compile`.
* **`closure` `callee-exception-table [tier=sp]`.** Single-pass first-tier only, as above.

**Host check and A/B.**

```
CRATONVM_JIT_IR_SPLICE_COLD_ARM_EDGE_SET=1 CRATONVM_DBG_JITC=1 $B -cp /root/rx27/p R24RegexSplitRows count compile 20000 20000 2>&1   | grep -E 'splice-unresolved-new-exit java/util/regex/Pattern\.curly|REFUSED java/util/regex/Pattern\.curly|ir-splice-bytes java/util/regex/Pattern\.curly'
DIR=/root/rx27/p REPS=5 tools/probes/icount2.sh rx27f7 "compile split-ws regex-split cached"   def:$T:CRATONVM_JIT_IR_SPLICE_COLD_ARM_EDGE_SET=0 es:$T:CRATONVM_JIT_IR_SPLICE_COLD_ARM_EDGE_SET=1   escws:$T:CRATONVM_JIT_IR_SPLICE_COLD_ARM_EDGE_SET=1,CRATONVM_JIT_IR_CALL_WRITE_SUMMARY=1 hs:hs:
```

Expected:
* `splice-unresolved-new-exit ...curly...` with the switch, then an `ir-splice-bytes ...curly...` line in
  `closure`'s compile;
* `curly`'s symbol (166 instr/op) and its call leave `compile`, a small share of the 2.0k.

The deopt census (`CRATONVM_DBG_DEOPT=1`) must not show the `Curly` exit firing per op: `\s+` never takes
that arm. Test: `rx27_a_target_behind_two_never_run_edges_is_cold_with_the_edge_set` (`jit/src/ir.rs`, JDK 25
`curly` bytecode).

## 2. `CRATONVM_JIT_IR_CALL_WRITE_SUMMARY`: removed (2026-10-29)

It never showed a gain, and its VM walk added an unallowlisted `find_method_recursive(` call site, so it
failed the metadata-table bypass guard after the dev merge. The code, tests and flag rows are removed. The idea
(reusing a primitive field read across a call whose callee provably writes no such field) would need a
throw-path model to reach the parse bodies, and would need to go through an allowlisted resolution path.
