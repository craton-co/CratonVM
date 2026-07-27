# HQL parser rejects chained additive/duration/concat operators (second `+`/`-`/`||` fails)

**Status:** FIXED and MERGED to `dev` (`3864097b`, merge of
`fix/hql-chained-operator-parse`). **Fully re-verified 2026-07-04**
(independent session, dev `8e9e99c8`, Azure Linux host, real-JDK,
`CRATONVM_JIT_OSR` both on and off, TIMEOUT=600s): all three originally-failing
classes now pass completely —
`org.hibernate.orm.test.query.TemporalParameterPlusDurationTest` **6/6**,
`org.hibernate.orm.test.mapping.basic.TimeZoneStorageMappingTests` **6/6**,
`org.hibernate.orm.test.query.hql.StandardFunctionTests` **44/44** (the
`ArrayIndexOutOfBoundsException` sub-failures mentioned below as unrelated are
also gone — recheck whether that was a red herring or fixed as a side effect
before assuming it needs separate tracking). No regressions observed in the
same 255-class rerun this verification came from. **Not the same bug as
`jit-deep-recursion-fault-recovery.md` ("Bug C") — that hypothesis was
REFUTED, see below.** Root-caused to a one-line defect in CratonVM's *native
Rust reimplementation* of `ParserATNSimulator`'s closure algorithm
(`../../../native-builtins/src/lib.rs`, `antlr_parser_closure_impl`): it passed
`inContext = !full_ctx` to `getEpsilonTarget` instead of real ANTLR's
`inContext = (depth == 0)`, wrongly attaching a left-recursion precedence
predicate to configs that should have had it suppressed. Fixed on branch
`fix/hql-chained-operator-parse`, commit `4e2a4493`. **Discovered:**
2026-07-04, triaging the residual FAIL bucket after the JIT SIGSEGV
regression investigation (dev `0ef780a2`+). **Investigated + fixed:**
2026-07-04, branch `fix/hql-chained-operator-parse` (dev `c20f6f15`+, Azure
Linux host, real-JDK, `hibpkg` harness).

## The fix

`../../../native-builtins/src/lib.rs`, `antlr_parser_closure_impl`: changed the 6th
argument to `antlr_parser_native_get_epsilon_target` from `!full_ctx` to
`depth == 0`. See
[[reference_antlr_closure_incontext_precedence_predicate_fix]] for the full
root-cause writeup and the methodology used to find it (a
Hibernate-independent minimal ANTLR4 grammar reproducer +
`ParserATNSimulator.debug`/`trace_atn_sim`'s built-in trace, diffed against
HotSpot).

**Verified:** the minimal grammar reproducer now matches HotSpot exactly.
`TemporalParameterPlusDurationTest`: **6/6 pass** (was `ok=2 failed=4`).

**NOT yet re-verified against the fix** (do this before merge/triage):
`TimeZoneStorageMappingTests`, `StandardFunctionTests` (its 3
`ArrayIndexOutOfBoundsException` failures are a confirmed-unrelated bug —
throws from the test's own lambda body, not ANTLR — expect those to persist
after this fix), and the ~32-class `query.hql`/`query.criteria` regression
sample (ran clean against the *unfixed* baseline; should stay clean). Held
off on these reruns this session because another agent was already running
a full Hibernate sweep concurrently on the shared Azure host — don't
duplicate that work, just consume its results when ready.

## Symptom

3 Hibernate ORM 8.0 test classes fail with `IllegalArgumentException:
org.hibernate.query.SyntaxException: ... no viable alternative at input '...'`:
- `org.hibernate.orm.test.query.TemporalParameterPlusDurationTest` (4/6 sub-tests fail — full class, cleanest repro)
- `org.hibernate.orm.test.mapping.basic.TimeZoneStorageMappingTests` (2/6 methods)
- `org.hibernate.orm.test.query.hql.StandardFunctionTests` (9/44 methods, some interleaved with an unrelated `ArrayIndexOutOfBoundsException` — see below)

ANTLR's error message embeds a `*` marker at the exact token position where
prediction failed (its standard "here's where it broke" convention — not part
of the real query text):

```
At 1:38 and token '+', no viable alternative at input
  'from SimpleEntity where :i + 1 second *+ 2 second > inst'
At 1:35 and token '+', no viable alternative at input
  'from SimpleEntity where :i + 3 day *+ 2 day > ldate'
At 1:30 and token '-', no viable alternative at input
  'select e.theTimestamp + 4 day *- 1 week from EntityOfBasics e'
At 1:46 and token '+', no viable alternative at input
  'select (2 * (e.theTimestamp - e.theTimestamp) *+ 3 * (4 day + 2 hour)) by second from EntityOfBasics e'
At 1:28 and token '||', no viable alternative at input
  'select 'foo' || e.theString *|| 'bar' from EntityOfBasics e'
At 1:306 and token '-', no viable alternative at input
  '...extract(offset from e.offsetTimeAuto)...'
```

**Pattern: parsing succeeds through the FIRST additive/concat operator in an
expression, then fails at the SECOND occurrence of the same operator class**
(`+`, `-`, `||` all affected — not specific to one operator, specific to
*repetition* of the same grammar production). A single `a + b` parses fine;
`a + b + c` (or `a || b || c`) does not. Re-verified at current dev tip
(`c20f6f15`) with the exact repro from the original doc — identical failures,
unchanged.

## REFUTED: not the Bug C / `PredictionContext` JIT-miscompile family

The original doc hypothesized shared root cause with
`docs/known-issues/jit-deep-recursion-fault-recovery.md` ("Bug C"), which
documents a JIT miscompile in 7 methods of Groovy's shaded ANTLR4 runtime
(`groovyjarjarantlr4/v4/runtime/atn/{PredictionContext,
SingletonPredictionContext, ObjectEqualityComparator}` — see
`../../../vm/src/jit/skip_list.rs` `is_antlr_prediction_context_miscompile`). Two
independent tests refute this:

1. **`--nojit` does not fix it.** Running the exact `TemporalParameterPlusDurationTest`
   repro (and a standalone reproducer, below) with `--nojit` produces byte-for-byte
   identical failures. Bug C is specifically a JIT codegen defect (de-JIT'ing
   the 7-method cluster fixes Groovy parsing); this bug reproduces with **no
   JIT involved at all**, so it cannot be the same defect.
2. **The existing ban wouldn't even apply here.** `is_antlr_prediction_context_miscompile`
   and the surrounding blanket ban in `skip_list.rs` match only the
   `groovyjarjarantlr4/` class-name prefix (Groovy's shaded/relocated ANTLR4
   copy). Hibernate's HQL grammar (`org.hibernate.grammars.hql.{HqlLexer,HqlParser}`)
   runs against the **real, unshaded `org.antlr:antlr4-runtime:4.13.2` jar**
   (confirmed: `PredictionContext`/`SingletonPredictionContext`/
   `ObjectEqualityComparator` classes live at `org/antlr/v4/runtime/...`, verified
   both in the local Gradle cache jar and in the exact jar on the classpath used
   for this repro, `.../org.antlr/antlr4-runtime/4.13.2/.../antlr4-runtime-4.13.2.jar`).
   `CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/` is therefore irrelevant to
   this bug either way — **extending that ban's prefix to `org/antlr/` would
   not fix anything**, since the defect isn't JIT-related in the first place.
   Do not pursue that as a fix.

## Root-caused further: generic ANTLR4-runtime interpreter bug, independent of Hibernate/HQL

Built a minimal, Hibernate-independent reproducer: a 5-rule ANTLR4 grammar with a
single left-recursive rule (`expr : expr ('+'|'-') expr | INT ;`), generated with
the standard `antlr4` tool (not Hibernate's grammar, no Groovy involved), run
directly against `org.antlr:antlr4-runtime:4.13.2`:

```java
MiniExprLexer lexer = new MiniExprLexer(CharStreams.fromString("1 + 2 + 3"));
MiniExprParser parser = new MiniExprParser(new CommonTokenStream(lexer));
parser.expr();
```

- HotSpot (JDK 25): parses `1 + 2 + 3` cleanly (a benign `<EOF>` diagnostic
  fires once per parse regardless of chain length — grammar-design noise, not
  this bug, confirmed present identically on HotSpot).
- CratonVM baseline (dev `c20f6f15`, JIT-on **and** `--nojit`): an *additional*
  `no viable alternative at input '+'` fires at token position of the
  **second** `+`, the **third** `+` for `1+2+3+4`, etc. — i.e. every visit to
  the loop-continuation decision *after the first* fails, for **any**
  left-recursive grammar, not just HQL's. This reproduces as the very first
  parse ever executed in a fresh process (a single `tryParse` call, no prior
  warm-up call) — ruling out cross-call/DFA-staleness as a precondition; the
  defect is purely about **revisiting the same ATN decision a second time
  within one parse**.
- Isolated to per-decision scope: a **two-decision** grammar
  (`expr: expr ('+'|'-') term ; term: term ('*'|'/') INT ;`) parsing
  `"1 + 2 * 3"` — one visit to the `+`/`-` decision, one visit to the
  *different* `*`/`/` decision, neither decision revisited — parses with
  **zero errors** on CratonVM. So the defect is not a global/session-wide
  corruption; it is specific to a decision's *own* cached DFA/ATN-config state
  surviving incorrectly from its first construction into its second use within
  the same parse.
- Confirmed reproducible in pure `PredictionMode.SLL` alone (not just the
  SLL→full-LL escalation path), narrowing the defect to core
  `ParserATNSimulator`/DFA decision-caching mechanics rather than the
  full-context fallback logic specifically.
- Sanity-checked `MurmurHash.update`/`finish` (the hash function
  `PredictionContext.calculateHashCode` uses, including the manual
  rotate-left-by-15 `(x<<15)|(x>>>17)` idiom and the two negative 32-bit
  multiplier constants) against dozens of edge-case inputs (0, ±1, INT_MIN/MAX,
  etc.) — CratonVM's results are byte-for-byte identical to HotSpot. This rules
  out a generic int shift/rotate/multiply-overflow interpreter bug as the
  cause; whatever is wrong is more specific than that (e.g. a caching/identity/
  mutation bug tied to the decision's DFA or config-set state, not raw
  arithmetic).

**UPDATE — root cause pinned and fixed (same session, continued).** Flipping
`ParserATNSimulator.debug`/`trace_atn_sim` (both `public static`, settable via
reflection with no recompilation) gave full built-in ANTLR trace output;
diffing CratonVM's trace against HotSpot's for the same input pinpointed the
exact diverging config. Bytecode-level instrumentation of a recompiled
`ParserATNSimulator.java` placed ahead of the real jar on the classpath then
revealed something classpath-override tests couldn't explain (the
instrumented method's own prints never fired even though the method clearly
still ran) — which led straight to `../../../native-builtins/src/lib.rs`: CratonVM has
a **native Rust reimplementation** of `ParserATNSimulator`'s closure/ATN-
simulation methods, registered by class+method+descriptor, which silently
overrides whatever bytecode is on the classpath for
`closureCheckingStopState`/`closure_`/`closure`/`getEpsilonTarget`/
`computeReachSet`/`canDropLoopEntryEdgeInLeftRecursiveRule`. See "The fix"
above and [[reference_antlr_closure_incontext_precedence_predicate_fix]] for
the full writeup — the originally-hypothesized `skip_list.rs` prefix change
remains confirmed inapplicable (this was never a JIT issue), but a real,
different fix does now exist.

## Secondary finding in `StandardFunctionTests`: confirmed UNRELATED

Re-ran with `-Dcraton.trace=1` to get full stack traces (current dev tip,
`c20f6f15`): `found=44 started=44 ok=35 failed=9` (6 `SyntaxException` from
this bug + 3 `ArrayIndexOutOfBoundsException`, matching the original "9/44,
some AIOOBE" observation). The `ArrayIndexOutOfBoundsException` stack traces
are **NOT** inside ANTLR — they throw from the test's own lambda body, e.g.:
```
java.lang.ArrayIndexOutOfBoundsException
	at org.hibernate.orm.test.query.hql.StandardFunctionTests.lambda$testExtractFunctionTimeZoneOffset$0(StandardFunctionTests.java:686)
	at org.hibernate.testing.orm.transaction.TransactionUtil.wrapInTransaction(TransactionUtil.java:76)
	...
```
So this is **confirmed a different, unrelated bug** — not an OOB into ANTLR's
internal DFA/state arrays, but ordinary Java array-indexing inside the test's
own result-handling code (`testExtractFunctionTimeZoneOffset` and similar,
thematically related to `TimeZoneStorageMappingTests`'s `extract(offset from
...)` queries, but a separate defect from the chained-operator parse failure).
Out of scope for this doc; not investigated further here.

## Repro

Minimal, Hibernate-independent (fastest to iterate on — no DB/JDBC/JUnit
bootstrap, isolates the bug to the ANTLR runtime itself):
```
# Generate MiniExprLexer/MiniExprParser from:
#   grammar MiniExpr;
#   expr : expr op=('+'|'-') expr # AddSub | INT # Atom ;
#   INT: [0-9]+; WS: [ \t\r\n]+ -> skip;
# with: java -jar antlr4-4.13.2.jar -no-listener -no-visitor MiniExpr.g4
# then a 3-line main() that parses "1 + 2 + 3" via parser.expr() against
# org.antlr:antlr4-runtime:4.13.2. Fails identically with and without --nojit.
```

Original Hibernate-level repro (still valid, current dev tip `c20f6f15`+):
```
cd apps/hib-suite-runner   # or the Linux mirror at /home/victor/hibpkg/runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home <jdk25> --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-TemporalParameterPlusDurationTest> 0
```
`TemporalParameterPlusDurationTest` is the cleanest Hibernate-level repro
target: all failing sub-tests fail identically, no interleaved unrelated
failures, and the query shape is minimal (`:i + N unit + M unit`). Confirm on
HotSpot that all methods pass (expected — this is CratonVM-only).

## Regression check (against the UNFIXED baseline — pre-fix sanity check, not a post-fix diff)

Ran a 32-class sample from `query.hql.*`/`query.criteria.*` (not overlapping
the 3 known-affected classes) on the unfixed baseline binary: all 32 pass
cleanly (zero unexpected failures; the 2 apparent "skipped" counts in
`CollateTests`/`CriteriaBuilderNonStandardFunctionsTest` are pre-existing
conditional `@Skip`s, not regressions). Confirms the bug's blast radius really
is scoped to queries that repeat the same operator/decision, not a broad
`query.hql`/`query.criteria` regression. **Not yet re-run against the fixed
binary** — do this before merge (see "The fix" section above).

## Scope

Small in Hibernate terms (3 classes, ~15 sub-test methods across them) but the
generic-grammar reproduction means the actual blast radius is any HQL query —
or any other CratonVM-hosted ANTLR4 grammar (SpEL, Groovy, etc.) — that visits
the same parser decision twice in one parse: chained date/duration arithmetic,
chained string concatenation, or (per the two-decision test above) potentially
other repeated-decision shapes not yet surveyed. Fix exists on branch
`fix/hql-chained-operator-parse` (commit `4e2a4493`), verified against the
primary repro; not yet merged.
