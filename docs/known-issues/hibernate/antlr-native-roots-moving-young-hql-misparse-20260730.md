# Native ANTLR intrinsics lose object roots under the moving young collector

**Status:** OPEN — root cause identified and partially fixed; the remaining tail
is a known defect *class*, not an unknown bug.

**Witness:** `org.hibernate.orm.test.hql.ASTParserLoadingTest`, `--nojit`.
**Not reproducible under JIT** (106/106 across every run measured).

## Symptom

Valid HQL is rejected at parse time, nondeterministically — a different subset
of tests fails on each run, and once a run starts failing it usually fails
several more times. Every failure has the same shape: a comparison operator
directly after a function call or a parenthesized expression.

```
SyntaxException: At 1:43 and token '>', no viable alternative at input
  'from Animal an where sqrt(an.bodyWeight)/2 *> 10'
SyntaxException: At 1:38 and token '=', mismatched input '=' expecting I
  'from Human h where -(h.intValue - 100)=74'
```

(The `*` is ANTLR's error-position marker, not corrupted input — the bracketed
original query in the exception is intact. An earlier investigation read this as
token-stream corruption; it is not.)

## What it is NOT

Two measurements rule out the explanation that the 2026-07-29 investigation
adopted (and acted on, by deleting `dev`'s trivial-accessor fast path):

1. `CRATONVM_TRIVIAL_GETTER_VERIFY=1` cross-checks every stackless-accessor hit
   against `resolve_field_ref_loader_aware`, the resolver the real `getfield`
   opcode uses. A full 106-test run reported **zero** divergences in field
   index, descriptor byte, reference-ness, volatility, or owning class — and
   still mis-parsed.
2. The same binary with the fast path still enabled and
   `CRATONVM_NO_MOVING_YOUNG=1` passes **106/106**.

The fast path only changes allocation and safepoint timing. It is not the bug,
and `CRATONVM_TRIVIAL_GETTER=0` is the switch to re-check that in one run.

## Root cause

`native-builtins/src/antlr_intrinsics.rs` reimplements ANTLR's
`ParserATNSimulator` closure/reach/merge machinery. It is written throughout on
raw `ObjectRef` locals — including `Vec<ObjectRef>` snapshots of whole config
sets and parent arrays — held across calls that allocate and therefore can run
a moving young collection. A relocated object leaves those locals pointing at
dead memory, and the result is silently linked into the parser's own graph.

The damage is amplified by ANTLR's own design: a wrong config or parent poisons
the closure/reach set, whose result is **memoized as a DFA edge**. One hit
therefore breaks every later prediction that reuses that edge — which is why a
single mis-timed collection produces a cluster of failures and why the affected
grammar path (`<expression> <comparison-op>`) stays broken for the rest of the
process.

`CRATONVM_MOVING_YOUNG_VERIFY=1` reports `forwarded_heap_refs_remaining
young=0 old=0` on every evacuation, which is consistent: the *heap* graph is
fully remapped. The stale references live in Rust locals the verifier does not
scan. `CRATONVM_DBG_BLOCKGC=1` also stays silent, because its canary only
recognises addresses still present in the current cycle's pointer map.

## Fixed so far (2026-07-30)

Seven concrete instances, each an independent use-after-move:

| Site | Defect |
|---|---|
| `antlr_alloc_atn_config` | returned the config allocated *before* an init that runs `SemanticContext$Empty.<clinit>` |
| `antlr_create_parent_array` | stored caller parents captured before class init + array alloc |
| `native_antlr_array_prediction_context_init_singleton` | pinned the parent but discarded the handle, so the local was never re-read |
| `antlr_merge_arrays` | operand parents, result parents, and `a`/`b` all stale across the recursive merge |
| `antlr_merge_singletons` | same, plus `parent == a_parent` comparing post-merge against pre-merge |
| `native_antlr_parser_compute_reach_set` (both loops) | whole-set config snapshot iterated while allocating; per-iteration pinning captured already-moved addresses |
| `antlr_parser_remove_all_configs_not_in_rule_stop_state` | same, plus a per-iteration `unpin_native_roots` that truncated the pin stack and dropped every *later* config's root |

Effect on the witness: failures per `--nojit` run drop from **1–15** to **0–2**.

This continues a 2026-07-22 pass over the same file
(`docs/internal/fixed-suite-bugs/hibernate/hibernate-atnstate-transitions-npe-intermittent-hql-parse-20260721-FIXED.md`),
which fixed the closure walk's `ATNState.transitions` instance of the identical
pattern. Two independent passes have now each found several more.

## Why the tail is still open, and what should close it

The remaining failures are the same defect class at sites not yet converted.
The file is ~7,300 lines and the unsafe idiom — a bare `ObjectRef` local or
`Vec<ObjectRef>` living across an allocating call — is its default style, so
per-site auditing has diminishing returns and no completion signal. Two passes
have each found "a few more".

**Recommended approach:** make the unsafe pattern unrepresentable rather than
hunt it. A scoped handle type that owns `(ObjectRef, pin)` and only yields the
reference through a `&mut NativeContext` re-read would turn every one of these
into a compile-time impossibility, and would also remove the recurring
truncating-`unpin_native_roots` hazard (releasing an early handle silently drops
every later one — a bug found twice in this file alone). Converting
`antlr_intrinsics.rs` to it is a self-contained mechanical change with the
existing ANTLR unit tests plus this witness as the acceptance gate.

**Interim mitigation** for anyone blocked: `CRATONVM_NO_MOVING_YOUNG=1` makes
the witness pass 106/106.

## Relationship to HIB-BYTEBUDDY

None, beyond being found during its verification. This defect predates the
ByteBuddy work, is independent of the `net/bytebuddy/` JIT ban, and does not
reproduce under JIT. The ByteBuddy closure is recorded in
`docs/internal/fixed-suite-bugs/hibernate/hib-bytebuddy-20260730-FIXED.md`;
`ASTParserLoadingTest` is the single `--nojit` non-PASS in its 302-class
corpus, and it is this issue.
