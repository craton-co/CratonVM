# Bug DF05 — `new String(StringBuilder)` char[]/byte[] mismatch ("java/lang/Object cannot be cast to [B")

> **✅ FIXED 2026-06-17** (worktree `C:/craton/CratonVM-tcfull`, branch
> `tomcat-fullsuite-triage`, `native-builtins/src/deprecated_util.rs`). **Root
> cause:** it is not `java.util.regex.Pattern` and not the XML String backing — it
> is the **`new String(StringBuilder)` constructor**. The JDK's
> `String(AbstractStringBuilder, Void)` ctor does
> `byte[] val = asb.getValue(); this.value = Arrays.copyOfRange(val, …)`, assuming
> the compact-string `byte[]` layout, but CratonVM's synthetic StringBuilder backs
> its content with a **`char[]`** → char[]→byte[] mismatch (`ArrayStoreException`
> directly; surfaces as `ClassCastException [B` through Xerces' `checkcast`). The
> Xerces XSD regex engine hits it in `Token$UnionToken.addChild` when it merges
> consecutive pattern chars (`"a"+"b"`→`"ab"`) via `new String(StringBuilder)` —
> hence every multi-char pattern facet failed. **Fix:** register a native for
> `String.<init>(Ljava/lang/StringBuilder;)V` that builds the String from the
> builder's chars (the same `sb_state` accessor `toString()` uses), bypassing the
> byte[]-assuming ctor. Purely additive — the ctor previously always threw.
> (`String(StringBuffer)` already worked and is left alone.)
>
> **Verification:** minimal repro is `new String(new StringBuilder("ab"))` (was
> ArrayStoreException, now OK). Probes `.tooling/drv/{SbProbe,XreRepro2,XsdRegexRepro}.java`
> all green; the Xerces regex matrix (`"ab"`, `"(true|false)"`, both modes) and
> the full XSD-validation path now pass. Tomcat classes: **TestTldParser,
> TestImplicitTldParser, TestTomcatNoServer, TestSchemaValidation all FAIL→PASS**;
> the cast error is eliminated from all 7 affected classes (incl. the residual
> TestTldScanner / TestWebXml / TestWebXmlOrdering, whose remaining FAIL/HANG are
> separate issues — a TLD-scan detail and the documented LinkedHashMap-overlay
> GC-walk throughput hang).

> **NOT DF02 — confirmed 2026-06-17.** Tested the hypothesis that this is the
> register-resident JIT-root bug (DF02). **Refuted:** `TestTldParser` fails
> byte-for-byte identically with `--nojit` (`CRATONVM_DISABLE_JIT=1`): 18 cast
> errors, `Tests run: 6, Failures: 4` both JIT-on and JIT-off. DF02's defining
> property is that `--nojit` always passes. The DF05 logs also contain **zero**
> DF02 gc-guard fingerprints (`all-zero header` / `Stale pointer detected`), and
> the cast fires **deterministically on every regex** (15–27×/class) — not the
> ~1/8000 GC-timing corruption DF02 is. So this is an independent, non-JIT,
> non-GC bug.
>
> **Narrowed (not `Pattern.compile`, not normal Strings):** standalone
> `Pattern.compile("(true|false)")` **succeeds** on CratonVM, as does compiling a
> pattern from a String built via literal / `new String(char[])` /
> `new String(byte[],cs)` / `new String(char[],off,len)` / `StringBuilder` /
> `substring` / `intern` / `String.valueOf(char[])` (repros
> `.tooling/drv/RegexProbe{,2}.java`, all 8 OK). The cast fails **only** for the
> pattern String the **XML/SAX parser hands to Tomcat's Digester** — i.e. a
> String produced by the XML-parsing path whose internal `value` backing is a
> bare `Object` (or non-`byte[]`) where the compact-String / regex path casts it
> to `[B`. Root-causing the fix means tracing how the SAX content/attribute
> handler builds its result Strings; left for that follow-up.

**Severity:** Medium-High (breaks all XML schema / TLD / web.xml validation via
the Digester regex rules).
**Status on CratonVM:** FAIL (validation rejects valid input). **HotSpot:** PASS.
**Run date:** 2026-06-17
**Binary:** dev `77620f55` (worktree `C:/craton/CratonVM-tcfull`).
**Affected classes (6):**
`org.apache.tomcat.util.descriptor.web.TestWebXml`,
`org.apache.tomcat.util.descriptor.tld.TestTldParser`,
`org.apache.tomcat.util.descriptor.tld.TestImplicitTldParser`,
`org.apache.jasper.servlet.TestTldScanner`,
`jakarta.servlet.resources.TestSchemaValidation`,
`org.apache.catalina.startup.TestTomcatNoServer`.

## Symptom

Compiling an ordinary regular expression throws a `ClassCastException` deep
inside the regex engine, surfacing through the XML Digester as an invalid-pattern
error for patterns that are perfectly valid on HotSpot:

```
ERROR [org.apache.tomcat.util.digester.Digester] Parse error at line [1,473] column [46]
  (org/xml/sax/SAXParseException: InvalidRegex: Pattern value 'jdbc:(.*):(.*)' is not a
   valid regular expression. The reported error was: 'java/lang/Object cannot be cast to [B'.)
ERROR ... Pattern value '(true|false)' ... 'java/lang/Object cannot be cast to [B'.
ERROR ... Pattern value '##.+'        ... 'java/lang/Object cannot be cast to [B'.
```

Every pattern fails the same way (`'... cannot be cast to [B'`), so this is not
about the pattern text — it is a structural fault in `Pattern.compile`.

## Root cause (analysis)

Something in CratonVM's `java.util.regex.Pattern` compilation path reads a field
expecting a `byte[]` (`[B`) but finds a plain `Object`. The most likely culprit
is the `Pattern.compile` → `Pattern.normalize`/`RemoveQEQuoting` path or a
String-internals access: modern JDK `String` stores its data in a `byte[] value`
(compact strings) with a `coder` byte. A CratonVM native that assumes the
pattern String's backing is always a `byte[]` (and casts it) will throw
`Object cannot be cast to [B` when the String it receives is backed differently
(e.g. a synthetic String, a `char[]`-backed value, or an `Object[]`). The fault
is in the VM's regex/String bridge, not in Tomcat.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
C:\craton\CratonVM-tcfull\target\release\cratonvm.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore org.apache.tomcat.util.descriptor.web.TestWebXml
```

A 3-line standalone repro should suffice:
`java.util.regex.Pattern.compile("(true|false)").matcher("true").matches();`

## Recommendation

**FIX / investigate.** Find the `as ... [B` / `byte[]` downcast in the
regex-engine or String-backing native and make it handle the non-`byte[]`
backing (or route through the real `String.getBytes`/value accessor). Bounded
once the cast site is located; high value because it gates all descriptor
validation.
