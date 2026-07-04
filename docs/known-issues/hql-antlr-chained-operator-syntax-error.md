# HQL parser rejects chained additive/duration/concat operators (second `+`/`-`/`||` fails)

**Status:** OPEN, not yet root-caused to a specific fix. **Discovered:** 2026-07-04,
triaging the residual FAIL bucket after the JIT SIGSEGV regression investigation
(dev `0ef780a2`+, Azure Linux host, real-JDK JIT-on).

## Symptom

3 classes fail with `IllegalArgumentException: org.hibernate.query.SyntaxException:
... no viable alternative at input '...'`:
- `org.hibernate.orm.test.mapping.basic.TimeZoneStorageMappingTests` (2/6 methods)
- `org.hibernate.orm.test.query.TemporalParameterPlusDurationTest` (4/4 methods — full class fail)
- `org.hibernate.orm.test.query.hql.StandardFunctionTests` (9/44 methods, see below)

Every failing query shares an identical shape. ANTLR's error message embeds a
`*` marker at the exact token position where prediction failed (its standard
"here's where it broke" convention — not part of the real query text):

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
(`+`, `-`, `||` all affected — this is not specific to one operator, it's
specific to *repetition* of the same grammar production). A single `a + b`
parses fine; `a + b + c` (or `a || b || c`) does not.

## Likely same root cause as the tracked ANTLR `PredictionContext` family

`docs/known-issues/jit-deep-recursion-fault-recovery.md` ("Bug C") already
documents CratonVM-specific corruption in ANTLR's adaptive-LL(*) prediction
machinery for Hibernate's HQL grammar — previously observed as **hangs/deep
recursion** (the `function.json.JsonArrayUnnestTest` HQL census-H4 timeout is
consolidated there) rather than outright mis-parses. A repeated grammar
production (the additive-expression rule recursing to parse a second `+`)
failing prediction entirely, rather than merely being slow, is consistent with
the same underlying `PredictionContext`-cache corruption manifesting as a hard
parse failure instead of a throughput/hang symptom for this particular
grammar shape. **Not confirmed identical** — needs the same JIT-ban/interpreted-
ANTLR-coldpath workaround tested against this repro to confirm shared root
cause before merging docs.

## Secondary, unconfirmed finding in the same test class

`StandardFunctionTests` also throws `ArrayIndexOutOfBoundsException` (empty
message, no captured stack trace — this harness only captures the `@@FAIL`
summary line, not a full trace) on 4 of its 44 methods, interleaved with the
`SyntaxException` failures. Could be the same underlying prediction-context
corruption manifesting differently (an OOB into ANTLR's internal DFA/state
arrays instead of a clean "no viable alternative"), or an unrelated bug. Needs
a rerun with full stack-trace capture (`-Dcraton.trace=1` or similar) to
separate these before concluding.

## Repro
```
cd apps/hib-suite-runner   # or the Linux mirror at /home/victor/hibpkg/runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home <jdk25> --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-TemporalParameterPlusDurationTest> 0
```
`TemporalParameterPlusDurationTest` is the cleanest repro target: all 4/4
methods fail identically, no interleaved unrelated failures, and the query
shape is minimal (`:i + N unit + M unit`). Confirm on HotSpot that all 4
methods pass (expected, since this is CratonVM-only per the CV bug-hunting
convention).

## Scope
Small (3 classes, ~15 sub-test methods across them) but a real HQL grammar
correctness gap — any application query chaining more than one date/duration
arithmetic operator, or more than one string concatenation, would hit this.
