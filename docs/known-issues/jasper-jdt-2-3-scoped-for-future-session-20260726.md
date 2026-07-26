# JASPER-JDT.2 (REMOVED) / JASPER-JDT.3 (still open, scoped for a future session)

## JASPER-JDT.2 — REMOVED 2026-07-26

**Status: removed, re-verified with real Tomcat integration test runs.**

Originally (2026-07-08) the real Tomcat
`org.apache.jasper.compiler.TestCompiler` suite hit nondeterministic
parser-adjacent heap corruption/OOM (first face:
`ArrayIndexOutOfBoundsException` in `Parser.parse`) under JIT, bisected
to `Parser.consumeRule` and the `org/eclipse/jdt/internal/compiler/parser/`
package generally. The ban's own documentation explicitly noted this was
**nondeterministic** — its original diagnosis needed repeated full-class
runs ("0/8 hits" under `--nojit` vs. consistent hits with JIT) to reach
confidence, meaning a single clean run would not have been sufficient
evidence to remove it.

**Re-verification methodology:** ran the real Tomcat fixture's own
`TestCompiler` class (12 real JSP-compilation test methods, each a full
embedded Tomcat boot + JSP compile + shutdown) at
`/data/data/apps/tomcat` (symlink to
`/data/data/tomcat-dohead-fixture-20260717`), classpath from
`.suite/cp-linux-fixed.txt`, invoked as:

```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
<binary> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.jasper.compiler.TestCompiler
```

(must run from the `/data/data/apps/tomcat` working directory — relative
webapp paths don't resolve otherwise. Each full run takes ~7-9 minutes;
a 120s timeout is NOT enough, use 600s+.)

- **Baseline** (ban active, current default): run twice — both `OK (12
  tests)`.
- **Lifted** (`CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/parser/`):
  run twice — both `OK (12 tests)`, 0 failures, no AIOOBE, no heap
  corruption.

4/4 clean runs across both configurations. Given the bug's own
documented "0/8 vs consistent" bar, this 2/2+2/2 result is real,
meaningful evidence — not exhaustive, but a considerably higher bar than
a single-run check. The ban was removed in `vm/src/jit/skip_list.rs`
(replaced with a `-- REMOVED 2026-07-26` comment citing this evidence).
`jdt_parser_package_is_jit_eligible_after_jasper_jdt_2_removal` is the
corresponding unit test.

## JASPER-JDT.3 — still open, unchanged

**Status: NOT investigated — remains banned. Separate, different bug
from JASPER-JDT.2; do not conflate the two.**

`org/eclipse/jdt/internal/compiler/ast/` (the AST/flow-analysis package,
as opposed to JASPER-JDT.2's parser package) is a second, independent
Eclipse JDT miscompile. Real Tomcat FORM-auth repro
(`TestFormAuthenticatorA/B/C` forwarding to the login-page JSP):
intermittent `JasperException` with root cause
`ArrayIndexOutOfBoundsException` reported at
`QualifiedNameReference.analyseCode` — a trivial delegating wrapper with
no array access of its own, i.e. the JIT lost/mis-attributed the inlined
callee's own frame. Not yet root-caused to a specific backend bug (unlike
JASPER-JDT.2, which at least isolated to `Parser.consumeRule`).

The real fixture and test classes are ready for whoever picks this up:

```
/data/data/apps/tomcat/output/testclasses/org/apache/catalina/authenticator/TestFormAuthenticatorA.class
/data/data/apps/tomcat/output/testclasses/org/apache/catalina/authenticator/TestFormAuthenticatorB.class
/data/data/apps/tomcat/output/testclasses/org/apache/catalina/authenticator/TestFormAuthenticatorC.class
```

Same classpath file (`.suite/cp-linux-fixed.txt`), same working-directory
requirement, same invocation pattern as JASPER-JDT.2 above (substitute
the test class name and
`CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/ast/`).
**Recommendation:** given JASPER-JDT.2 turned out to be genuinely fixed
after the JIT rework despite its severe original symptoms, JASPER-JDT.3
is a reasonable next candidate for the same repeated-run methodology (2+
baseline, 2+ lifted, using the FORM-auth classes above) — budget real
time, each run through a full Tomcat boot cycle per test method, likely
similar ~7-9 minute-per-run overhead to what JASPER-JDT.2's
re-verification needed.
