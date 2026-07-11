# `FunctionTests` / `ASTParserLoadingTest` / `DefaultCatalogAndSchemaTest` — no longer crash after the misattribution fix, but not fully triaged (host-load-limited)

Status: OPEN — needs a rerun on an idle host to reach a clean `@@RESULT`

Found: 2026-07-11, verifying the
[uncaught-exception-misattribution fix](../internal/fixed-suite-bugs/uncaught-exception-misattribution-native-pending-return-FIXED.md).
All three of these classes previously crashed the whole process (`rc=1`)
with a misattributed `java/lang/Thread`/`org/antlr/v4/runtime/CommonToken`
"exception" almost immediately. After the fix:

- **Zero crashes** across every rerun (`grep -c 'run() returned Err'` = 0 in
  every log).
- All three now execute **far more** of the real test suite than before —
  `FunctionTests` alone ran to 21,634 output lines (vs. crashing at ~22,600
  lines of *much more verbose* debug-only output previously) before hitting
  a 900s wall-clock timeout, deep inside `testDurationArithmeticWithParameters`
  (a heavily `@ParameterizedTest`-driven method generating many distinct H2
  `datediff`/`dateadd` SQL queries) — i.e. it is doing large amounts of
  genuine forward work, not spinning.
- This investigation ran on a **heavily shared Azure host** (`uptime` load
  average 9-10 on 16 cores throughout, with multiple *other* concurrent
  sessions' full ES/Tomcat suite reruns active — see
  `docs/known-issues/reference` conventions for this host's known
  contention pattern) — none of the three reached a final `@@RESULT`/`@@DONE`
  within the time available (400-900s per attempt).

## What's confirmed vs. not

- CONFIRMED: the misattribution bug (this doc's sibling FIXED writeup) is
  gone for all three classes.
- CONFIRMED (from live instrumentation during root-causing, before the fix):
  `FunctionTests`'s masked real exception was a genuine
  `java.lang.NullPointerException` — message/call site not captured (the
  instrumentation only logged the class, not the full stack; a
  `CRATONVM_DBG_ATHROW=1 -Dcraton.trace=1` rerun would surface it, since NPEs
  synthesized by the VM's own null-check paths don't go through the
  interpreter's `athrow` opcode and so weren't visible in that trace either —
  a `CRATONVM_DBG_CHARSET`-style dedicated NPE dump, or just capturing
  `@@FAIL` lines with `-Dcraton.trace=1`, is the more direct route next time).
- NOT YET DETERMINED: whether `ASTParserLoadingTest` and
  `DefaultCatalogAndSchemaTest` complete cleanly, hang, or hit a genuine
  (separate) failure once given enough uncontended time —
  `DefaultCatalogAndSchemaTest`'s partial output showed a repeating
  (9x observed before timeout) `jakarta.xml.bind`/JAXBContext
  `ServiceProvider loading Facility` log cycle during SessionFactory
  bootstrap, which may just be legitimately slow JAXB-based schema-metadata
  work repeated once per distinct entity-mapping scenario in the test class
  (each ~40s apart) rather than a hang — not confirmed either way.

## Next step

Rerun each class individually (not concurrently) with a generous timeout
(1200s+) on an idle host, `-Dcraton.trace=1` for stack traces on any
`@@FAIL`, to get a clean `@@RESULT` and, for `FunctionTests`, the exact NPE
call site.
