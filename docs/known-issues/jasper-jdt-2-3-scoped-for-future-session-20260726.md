# JASPER-JDT.2 and JASPER-JDT.3 (Eclipse JDT parser/AST) — BOTH REMOVED 2026-07-26

**Status: both bans removed, re-verified with real Tomcat integration test runs.**

Both were real, previously-nondeterministic Eclipse JDT compiler (ECJ)
miscompiles, discovered via Tomcat's internal use of ECJ to compile JSPs.
Given their documented severity (heap corruption/OOM, original diagnosis
needed repeated full-class runs — "0/8 hits" under `--nojit` vs.
consistent hits with JIT — to reach confidence), both were re-verified
against the same, considerably-higher-than-a-single-run bar: 2 baseline
runs (ban active) + 2 lifted runs (`CRATONVM_JIT_ALLOW_PACKAGES` set),
each using the real Tomcat fixture's own integration test suite (not a
synthetic probe), before removal.

## JASPER-JDT.2 (`org/eclipse/jdt/internal/compiler/parser/`) — REMOVED

Originally (2026-07-08): the real Tomcat
`org.apache.jasper.compiler.TestCompiler` suite hit nondeterministic
parser-adjacent heap corruption/OOM (first face:
`ArrayIndexOutOfBoundsException` in `Parser.parse`) under JIT, bisected
to `Parser.consumeRule` and the parser package generally.

**Re-verification:** real Tomcat fixture at `/data/data/apps/tomcat`
(symlink to `/data/data/tomcat-dohead-fixture-20260717`), classpath
`.suite/cp-linux-fixed.txt`, `org.apache.jasper.compiler.TestCompiler`
(12 real JSP-compilation test methods, each a full embedded Tomcat
boot+shutdown):

```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
<binary> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.jasper.compiler.TestCompiler
```

(must run from `/data/data/apps/tomcat` — relative webapp paths don't
resolve otherwise. ~7-9 min per run; use a 600s+ timeout, 120s is not
enough.)

- Baseline (ban active): 2/2 runs `OK (12 tests)`.
- Lifted (`CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/parser/`):
  2/2 runs `OK (12 tests)`, 0 failures, no AIOOBE, no heap corruption.

4/4 clean. Ban replaced with a `-- REMOVED 2026-07-26` comment in
`vm/src/jit/skip_list.rs`.

## JASPER-JDT.3 (`org/eclipse/jdt/internal/compiler/ast/`) — REMOVED

Originally (2026-07-10): a second, independent Eclipse JDT miscompile in
the AST/flow-analysis package (distinct from JASPER-JDT.2's parser
package). Real Tomcat FORM-auth repro
(`TestFormAuthenticatorA/B/C` forwarding to the login-page JSP):
intermittent `JasperException` with root cause
`ArrayIndexOutOfBoundsException` reported at
`QualifiedNameReference.analyseCode` — a trivial delegating wrapper with
no array access of its own, i.e. the JIT lost/mis-attributed the inlined
callee's own frame. Documented as nondeterministic, same as JASPER-JDT.2,
but never root-caused to a specific backend bug (JASPER-JDT.2 at least
isolated to `Parser.consumeRule`).

**Re-verification:** same fixture, classpath, and working-directory
requirement as above, `org.apache.catalina.authenticator.TestFormAuthenticatorA`
(9 real FORM-auth JSP-compilation test methods):

```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
<binary> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.authenticator.TestFormAuthenticatorA
```

- Baseline (ban active): 2/2 runs `OK (9 tests)`.
- Lifted (`CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/ast/`):
  2/2 runs `OK (9 tests)`, 0 failures, no AIOOBE.

4/4 clean. Ban replaced with a `-- REMOVED 2026-07-26` comment in
`vm/src/jit/skip_list.rs`.

`jdt_parser_and_ast_packages_are_jit_eligible_after_jasper_jdt_2_3_removal`
is the corresponding unit test covering both removals.

## Notable

Both of these were among the most severe-sounding bans re-tested this
session — explicitly documented nondeterministic heap corruption, one
never root-caused to a specific backend bug at all — and both turned out
to be genuinely fixed by today's JIT rework once re-tested properly
against real integration test suites rather than assumed unsafe based on
their historical severity alone.
