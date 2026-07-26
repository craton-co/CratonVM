# JASPER-JDT.2 / JASPER-JDT.3 (Eclipse JDT parser/AST) — scoped for a future session, not attempted this round

**Status: NOT investigated this session — deliberately deferred given its documented severity and the host contention observed today.**

## Why this is next, and why it's harder than today's other items

`org/eclipse/jdt/internal/compiler/parser/` (JASPER-JDT.2) and
`org/eclipse/jdt/internal/compiler/ast/` (JASPER-JDT.3) are two real,
already-substantially-diagnosed JIT miscompiles in the Eclipse JDT
compiler (ECJ), which Tomcat uses internally to compile JSPs. Unlike
today's removals (`JSONSMART-PARSER.1`, `SPB.9`), these bans document:

- **Nondeterministic** heap corruption/OOM ("size varies run to run"),
  not a clean, always-reproducing exception.
- Their own original diagnosis required **repeated full-class runs**
  (the JASPER-JDT.3 note cites "0/8 hits across repeated full-class runs"
  under `--nojit` vs. consistent hits with JIT on) to reach confidence —
  a single clean run proves much less here than for a deterministic bug.
- JASPER-JDT.2 is explicitly "not yet root-caused" at the backend level
  despite extensive bisection (`Parser.consumeRule` isolated as
  sufficient to reproduce, but the underlying x64 lowering bug itself was
  never found).

## What's available for a future session

A real, working Tomcat fixture exists and is directly accessible (no
mount-shadow issue currently) at `/data/data/apps/tomcat` (symlink to
`/data/data/tomcat-dohead-fixture-20260717`), with a ready classpath file
at `/data/data/apps/tomcat/.suite/cp-linux-fixed.txt` and the exact test
classes both bans' own comments reference already compiled and present:

- `org.apache.catalina.authenticator.TestFormAuthenticatorA/B/C`
  (JASPER-JDT.3's real repro — FORM-auth JSP compilation).
- Tomcat's own `testBug55262`/`testBug53257*`-style JDT compiler test
  methods for JASPER-JDT.2 (search the ECJ/JDT core test sources if
  bundled, or use the same Tomcat JSP-compilation path since Tomcat's
  own JSP compiler test suite exercises the same parser code).

Real ECJ jars are also present on the host
(`/home/victor/.m2/repository/org/eclipse/jdt/ecj/3.32.0/ecj-3.32.0.jar`,
plus newer 3.45.0/3.46.0 versions in `.gradle` caches) if a more targeted,
non-Tomcat-integration-test probe is preferred.

## Recommendation

Run the real `TestFormAuthenticatorA/B/C` classes (and/or the JDT
parser-specific bug-number tests) repeated ~10+ times each with the
respective package's ban lifted via `CRATONVM_JIT_ALLOW_PACKAGES`,
matching the original diagnosis's own repeat-count bar for statistical
confidence — a single clean run is not sufficient evidence for a
documented-nondeterministic bug. Budget real time for this (the
FormAuth tests are full Tomcat-boot integration tests, likely slow, and
this host has shown significant contention from concurrent sessions
during this investigation window) rather than attempting it under time
pressure.
