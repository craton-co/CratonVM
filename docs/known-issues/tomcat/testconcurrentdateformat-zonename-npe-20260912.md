# `TestConcurrentDateFormat.testFormatReturnsGMTAfterParseCET` — NPE inside the JDK's own `SimpleDateFormat.matchZoneString` — OPEN, not root-caused

## Status
**OPEN.** New finding, not in any existing tomcat known-issues page. Not yet
checked against a HotSpot control on this fixture.

## Measured
2026-09-12, `dev@c0bebbde5`, local Windows box, real JDK 25, `-Parallel 2`,
both JIT-tier arms (`CRATONVM_C2_SUPERSEDE=0`, `CRATONVM_JIT_FORCE_C2=1`) —
identical failure in both, so not JIT-tier-dependent.

## Symptom
```
1) testFormatReturnsGMTAfterParseCET(org.apache.tomcat.util.http.TestConcurrentDateFormat)
java.lang.NullPointerException: Cannot invoke "String.length()" because "zoneName" is null
	at java.text.SimpleDateFormat.matchZoneString(SimpleDateFormat.java:1747)
	at java.text.SimpleDateFormat.subParseZoneString(SimpleDateFormat.java:1783)
	at java.text.SimpleDateFormat.subParse(SimpleDateFormat.java:2214)
	at java.text.SimpleDateFormat.parse(SimpleDateFormat.java:1579)
	at java.text.DateFormat.parse(DateFormat.java:425)
```
1 of 2 sub-tests in the class. The crash is inside `java.text` itself, not
Tomcat's `ConcurrentDateFormat` wrapper under test — the stack has no
CratonVM native frame in it at all, which is what makes this worth a
dedicated page rather than folding it into a generic "flaky" bucket: this is
real JDK 25 bytecode (real-JDK mode, not synthetic), reading through its own
locale/timezone display-name table and getting a `null` entry where it
expects a string.

## Working hypothesis, not verified
`matchZoneString` walks a `String[][]` zone-string table (locale-specific
time-zone display names, e.g. "GMT", "CET", "Central European Time") looking
for the longest match at the current parse position; a `null` entry in that
table is not something `SimpleDateFormat` itself is written to expect (hence
the NPE rather than a graceful "no match"). The test name
(`ReturnsGMTAfterParseCET`) suggests the parse is specifically exercising a
CET-zone display name — if CratonVM's real-JDK integration supplies this
table (via whatever backs `sun.util.locale.provider`/`zi` timezone resource
loading in this fork) with a hole for that specific locale/zone entry, this
would be a resource-loading gap in that layer rather than a `SimpleDateFormat`
bug proper. **Not traced further** — this needs a look at whatever
CratonVM-side code answers timezone-display-name lookups for real-JDK mode,
not the `java.text` classes themselves (those are unmodified JDK bytecode).

## Not yet done
- Confirm on HotSpot with the identical classpath/locale (Windows OS locale
  is Russian on this box per
  `windows-local-environment-artifacts.md` — worth checking whether the
  locale, not the zone, is the actual trigger, since that page already
  documents this host's non-English locale surfacing in unrelated tests).
- Identify which CratonVM component supplies the zone-string table under
  real-JDK mode and whether it has a documented gap for this zone/locale
  combination.

## Repro
```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Category all -Start <index> -Count 1 -RunName repro -TimeoutSec 60
# org.apache.tomcat.util.http.TestConcurrentDateFormat
```
