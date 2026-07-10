# `\p{java*}` predefined regex character classes unsupported — breaks `java.util.Scanner` entirely

Status: RESOLVED — fixed 2026-07-10
Severity: was **High** — blast radius was not WildFly-specific; broke `java.util.Scanner` unconditionally for
every program that constructs one, since the failure happened in `Scanner`'s static initializer.
First confirmed: 2026-07-10, Azure worktree `test/wildfly-full-suite-20260707`, dev@4723e059 (round-5 binary,
`frozen-cratonvm-wildfly-bugbash-v4-20260710`)
Fixed: 2026-07-10, branch `fix/regex-java-predefined-classes-20260710`, worktree
`/data/data/wt/wt-regex-java-predefined-20260710` on the Azure build host

## Symptom

Any class that touches `java.util.Scanner` (even just `new Scanner(System.in)`, before any method is
called on it) throws `NoClassDefFoundError` wrapping an `ExceptionInInitializerError`, because `Scanner`'s
class-init fails while compiling one of its own predefined static `Pattern`s:

```text
[CLINIT-TRACE 1] at java/util/Scanner.<clinit> (Scanner.java:415) bci=19
Caused by: java/lang/IllegalArgumentException: PatternSyntaxException: Error compiling regex: Regex error: error parsing pattern 0
```

First surfaced in the WildFly suite as a client-side (test-runner-side, not server-side) failure in
`EarClassLoadingTestCase`:

```text
java.lang.reflect.InvocationTargetException: java.lang.NoClassDefFoundError: java/util/Scanner
	at org.jboss.arquillian.container.impl.MapObject.populate(MapObject.java:54)
	at org.jboss.arquillian.container.impl.ContainerImpl.createDeployableConfiguration(...)
	at org.jboss.arquillian.container.impl.ContainerImpl.setup(...)
```

## Root cause — confirmed

`java.util.Scanner`'s static initializer (`Scanner.java:414-416` in JDK 25) compiles:

```java
private static final Pattern WHITESPACE_PATTERN = Pattern.compile("\\p{javaWhitespace}+");
```

`\p{javaWhitespace}` is one of the Java-specific *predefined character classes* documented in
`java.util.regex.Pattern` (the `\p{javaLowerCase}`, `\p{javaUpperCase}`, `\p{javaWhitespace}`,
`\p{javaMirrored}`, `\p{javaDigit}`, `\p{javaIdentifierIgnorable}`, `\p{javaJavaIdentifierStart}`,
`\p{javaJavaIdentifierPart}`, `\p{javaUnicodeIdentifierStart}`, `\p{javaUnicodeIdentifierPart}`,
`\p{javaSpaceChar}`, `\p{javaDefined}`, `\p{javaTitleCase}`, `\p{javaAlphabetic}` family). These are **not**
Unicode general categories, scripts, or blocks — each one is a direct alias for the matching
`java.lang.Character.isXxx(int)` predicate, and they exist only because `java.util.regex` special-cases
them internally (see HotSpot's `Pattern.java` `CharPropertyNames` table).

CratonVM's Java→Rust regex translation layer, `translate_java_regex()` in
`native-builtins/src/lib.rs` (~line 43744), rewrites two other Java-specific `\p{...}` prefix forms so the
`regex`/`fancy-regex` crates can understand them:

- `\p{InBlockName}` → `\p{BlockName}` (Unicode block)
- `\p{IsScriptName}` → `\p{ScriptName}` (Unicode script)
- `\p{all}` (special-cased separately)

It has **no rewrite rule for the `\p{java*}` family at all**. Such patterns fall through untouched to
`regex::Regex::new(...)`, which fails (Rust's `regex` crate has no Unicode property named
`"javaWhitespace"`), then to the `fancy_regex` fallback, which fails identically — both surface as the
generic `Regex error: error parsing pattern 0`, wrapped into `PatternSyntaxException` /
`IllegalArgumentException` by `compile_java_regex_uncached`.

### Isolated confirmation (no WildFly, no Scanner)

```java
Pattern.compile("\\p{javaWhitespace}+");        // Scanner.java:415 (WHITESPACE_PATTERN)
Pattern.compile("[\\p{javaDigit}&&[^0-9]]");    // Scanner.java:422-423 (NON_ASCII_DIGIT)
```

Both fail identically under CratonVM with the exact same error text as the `Scanner.<clinit>` trace above;
an unrelated pattern from the same file, `Pattern.compile("(?s).*")` (Scanner's `FIND_ANY_PATTERN`, a plain
Rust-regex-compatible construct with no Java-specific class), compiles fine — isolating the defect
specifically to the `\p{java*}` family, not the surrounding `Pattern.compile` plumbing.

## Blast radius

`Scanner.<clinit>` fails on `WHITESPACE_PATTERN` (line 415) before it ever reaches `NON_ASCII_DIGIT` (line
422), so **every single construction of `java.util.Scanner`, for any purpose** (stdin reading, delimited
tokenizing, file/String parsing) throws `NoClassDefFoundError: java/util/Scanner` — this is not scoped to
WildFly at all; it is a fundamental, extremely widely-used JDK class. Any other JDK or third-party code
that compiles a `\p{java*}` pattern directly (rare, since most application code uses `\s`/`\d`/`\w` instead
of the java-specific forms) would hit the same failure.

### Confirmed scale — round-5 full-suite rerun, 2026-07-10

Reran all 1531 non-passed/never-tested WildFly classes (2 shards, same binary) after writing this doc.
**947 of 1531 classes (61.9%)** hit this exact bug directly (`NoClassDefFoundError: java/util/Scanner`,
928 classified `FAIL` + 19 `TIMEOUT`) — making it by far the single dominant blocker of this round, ahead
of every other cause combined.

The blast radius is larger still: Maven Surefire's own forked-JVM bookkeeping thread
(`org.apache.maven.surefire.booter.PpidChecker`, which runs on a `ScheduledThreadPoolExecutor` inside
every forked test JVM to detect a dead parent process) also lazily triggers `Scanner.<clinit>` the first
time it runs. Confirmed directly via a `.dumpstream` file from this run:

```text
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=java/util/Scanner
  cause=java/lang/IllegalArgumentException PatternSyntaxException: Error compiling regex: Regex error: error parsing pattern 0
  [CLINIT-TRACE 0] at java/util/concurrent/ThreadPoolExecutor$Worker.run (ThreadPoolExecutor.java:614)
  ...
  [CLINIT-TRACE 5] at org/apache/maven/surefire/booter/ForkedBooter$2.run (ForkedBooter.java:214)
  [CLINIT-TRACE 6] at org/apache/maven/surefire/booter/PpidChecker.isProcessAlive (PpidChecker.java:123)
```

Across the run window, **1171 of 1236 (94.7%)** `.dumpstream` files written by forked test JVMs in
`/data/data/cratonvm/apps/wildfly` contain this exact clinit failure — i.e. it fires in nearly every single
forked JVM CratonVM launches under this harness, independent of which test class is under test. When it
fires early enough in the fork's lifetime (which it usually does, since `PpidChecker`'s check runs on a
short fixed schedule right after fork startup), the uncaught `ExceptionInInitializerError` in that
background thread kills the fork before it ever prints `Running <class>` — Surefire's parent process (on
real JDK 17, unaffected) then sees a fork that died mid-protocol and reports one of two downstream
symptoms depending on exactly when the pipe was cut:

- `org.apache.maven.surefire.booter.SurefireBooterForkException: The forked VM terminated without properly
  saying goodbye. VM crash or System.exit called?` — classified `CRASH` by this harness. **20/20 CRASH
  classes this round** showed zero evidence of ever reaching test execution (no `Running org.wildfly...`
  line), consistent with all 20 being this same fork-startup crash rather than 20 distinct issues.
- `org.apache.maven.surefire.booter.SurefireBooterForkException: There was an error in the forked process
  / Test mechanism :: java.lang.Object cannot be cast to java.lang.Integer` — Maven's `ForkStarter` failing
  to decode a value from the corrupted/truncated wire-protocol stream left by the fork dying mid-write.
  **58 instances this round**, all likewise missing any `Running org.wildfly...` evidence.

Net: of the 1531 classes rerun, roughly **1025 (947 + 20 + 58 ≈ 67%)** show direct or downstream evidence
of this one bug. The remaining non-passing classes are dominated by an unrelated, pre-existing
shared-host/shared-checkout race (`WFLYLNCHR0001`/`WFLYLNCHR0003`: `target/wildfly` provisioning directory
missing or incomplete when a class runs — not a CratonVM bug, see harness notes), not additional CratonVM
defects.

## Original fix sketch (superseded by the actual fix below)

Extend `translate_java_regex()` with a rewrite table for the `\p{java*}` family. Since the `regex` crate has
no equivalent Unicode property, the practical fix is to rewrite these constructs into function-checked
character classes evaluated against `Character`'s exact predicate semantics, e.g. by expanding
`\p{javaWhitespace}` into an explicit alternation/class matching exactly what
`Character.isWhitespace(int)` accepts (which is *not* the same set as Unicode's `White_Space` property —
notably it excludes non-breaking spaces ` `, ` `, ` ` — so a naive `\s`/`\p{White_Space}`
substitution would silently diverge from JDK behavior for those code points), rather than delegating to any
single existing Unicode property name.


## Fix

Extended `translate_java_regex()` in `native-builtins/src/lib.rs` (~line 43952) with a new branch
that recognizes `\p{java*}` / `\P{java*}` and rewrites each to an explicit Rust-regex-crate class
body via a new lookup table, `map_java_predefined_class()` (~line 44259, right after the existing
`map_java_unicode_block()`). The fast-path pre-check that lets `translate_java_regex` bail out
early for patterns with nothing to rewrite was also extended to trigger on `\p{java`/`\P{java`
(it previously only checked for `\p{In`/`\p{Is`/`\Q`/`\p{all}`).

All 18 names from `java.util.regex.Pattern`'s real `CharPredicates.forProperty()` table (verified
against JDK 25's `src.zip`, not recalled from memory — the actual key list differs subtly from the
older simplified spec, e.g. it's `javaTitleCase` not `javaTitlecase`, and
`javaJavaIdentifierStart`/`Part` follow older simpler rules while `javaUnicodeIdentifierStart`/`Part`
follow the newer `ID_Start`/`ID_Continue`-based UAX31 profile — these are NOT interchangeable) are
mapped to the exact semantics documented on the corresponding `java.lang.Character.isXxx(int)`
method:

| `\p{java*}` name | `Character` predicate | Rust class body |
|---|---|---|
| `javaLowerCase` | `isLowerCase` | `\p{Lowercase}` |
| `javaUpperCase` | `isUpperCase` | `\p{Uppercase}` |
| `javaAlphabetic` | `isAlphabetic` | `\p{Alphabetic}` |
| `javaIdeographic` | `isIdeographic` | `\p{Ideographic}` |
| `javaTitleCase` | `isTitleCase` | `\p{Lt}` |
| `javaDigit` | `isDigit` | `\p{Nd}` |
| `javaDefined` | `isDefined` | union of all general categories except `Cn` |
| `javaLetter` | `isLetter` | `\p{L}` |
| `javaLetterOrDigit` | `isLetterOrDigit` | `\p{L}\p{Nd}` |
| `javaJavaIdentifierStart` | `isJavaIdentifierStart` | `\p{L}\p{Nl}\p{Sc}\p{Pc}` |
| `javaJavaIdentifierPart` | `isJavaIdentifierPart` | `\p{L}\p{Nl}\p{Nd}\p{Mc}\p{Mn}\p{Sc}\p{Pc}\p{Cf}` + ignorable ranges |
| `javaUnicodeIdentifierStart` | `isUnicodeIdentifierStart` | `\p{ID_Start}\u{2E2F}` |
| `javaUnicodeIdentifierPart` | `isUnicodeIdentifierPart` | `\p{ID_Start}\p{ID_Continue}\u{2E2F}\p{Cf}` + ignorable ranges |
| `javaIdentifierIgnorable` | `isIdentifierIgnorable` | `\x00-\x08\x0E-\x1B\x7F-\x9F\p{Cf}` |
| `javaSpaceChar` | `isSpaceChar` | `\p{Z}` |
| `javaWhitespace` | `isWhitespace` | explicit codepoints/ranges (see below) |
| `javaISOControl` | `isISOControl` | `\x00-\x1F\x7F-\x9F` |
| `javaMirrored` | `isMirrored` | `\p{Bidi_Mirrored}` |

`javaWhitespace` (the one Scanner's `WHITESPACE_PATTERN` actually needs) is deliberately **not**
expressed as `\p{Z}` minus the three non-breaking-space exceptions via a `--`/intersection
operator — it's spelled out as an explicit union of codepoints/ranges
(`\x09-\x0D\x1C-\x1F\x20\u{1680}\u{2000}-\u{2006}\u{2008}-\u{200A}\u{2028}\u{2029}\u{205F}\u{3000}`)
so the translation doesn't depend on character-class set-operator support. (Separately: the Rust
`regex` crate *does* support `&&`/`--`/`~~` set operators inside `[...]`, which is why Scanner's
other predefined pattern, `NON_ASCII_DIGIT` = `[\p{javaDigit}&&[^0-9]]`, needed no change beyond
`\p{javaDigit}` itself — the surrounding `&&[^0-9]]` syntax already compiled.)

**Known, documented residual:** the translation does not honor `Pattern.CASE_INSENSITIVE`
widening `javaLowerCase`/`javaUpperCase`/`javaTitleCase` into a tri-case union (real JDK does this
only when both the flag and one of these three specific classes are used together in the same
pattern — rare in practice, and not hit by `Scanner` or by anything found in the WildFly suite).

### Verification

- Added `mod java_predefined_class_tests` (16 tests) in `native-builtins/src/lib.rs` next to the
  existing `java_replacement_tests` module, using `compile_java_regex` end-to-end (not just the
  translation step) to check: all 18 names compile in both `\p{...}` and `\P{...}` form; both of
  Scanner's actual patterns (`WHITESPACE_PATTERN`, `NON_ASCII_DIGIT`); `javaWhitespace` vs
  `javaSpaceChar`'s differing non-breaking-space handling; `javaLetter` vs `javaAlphabetic`'s
  differing treatment of `Nl` (letter-number) codepoints; `javaJavaIdentifierStart` vs
  `javaUnicodeIdentifierStart`'s differing treatment of currency symbols; `javaDefined` against a
  permanently-reserved noncharacter (U+FFFE). All 16 pass.
- Isolated repro from this doc (`ScannerTest.java`, `new Scanner(System.in)`) now loads and runs
  cleanly under the fixed binary — confirmed the pre-fix frozen binary
  (`frozen-cratonvm-wildfly-bugbash-v4-20260710`, the exact binary this doc's discovery run used)
  still reproduces the original `NoClassDefFoundError`/`PatternSyntaxException` failure, so this
  isn't a case of the bug having silently gone stale.
- A fuller functional test (`Scanner("hello   world\t42\n3.14 foo").next()` tokenizing loop +
  `hasNextInt()`/`nextInt()` summing) produces byte-identical output to real JDK 25 (`java` from
  `/home/victor/jdk25`).
- Reran the exact WildFly class first cited in this doc, `EarClassLoadingTestCase`, through the real
  Maven/Surefire harness (`apps/wildfly-suite-runner/run-suite-linux.sh`) with the fixed binary: it
  no longer hits the Scanner/regex failure at all — it now fails purely on the separate, pre-existing,
  non-CratonVM `WFLYLNCHR0003` harness provisioning gap (`target/wildfly` not built for this module),
  exactly the "not a CratonVM bug" category this doc's own Blast Radius section already called out.
- Ran a further 25-class sample (`org.jboss.as.test.integration.domain.*` and
  `org.jboss.as.test.integration.batch.*`, 28 test-method attempts) through the same harness: **zero**
  occurrences of the Scanner/regex-translation signature (`Scanner`, `error parsing pattern 0`,
  `javaWhitespace`) across all logs. All 25 failures are `WFLYLNCHR0001`/`WFLYLNCHR0003` (55 + 1
  occurrences) — the same pre-existing provisioning-gap category, unrelated to this fix.
- Did not rerun the full 1531-class round-5 slice (the original discovery worktree,
  `test/wildfly-full-suite-20260707`, no longer exists on the shared Azure host) — the isolated,
  functional, and real-Maven-harness verification above gives high confidence the fix is correct and
  complete for the `\p{java*}` family itself; a full-suite rerun would mostly re-measure the
  already-documented, unrelated `WFLYLNCHR0001`/`WFLYLNCHR0003` provisioning-gap noise.

## Repro

```bash
cat > /tmp/ScannerTest.java << 'EOF'
import java.util.Scanner;
public class ScannerTest {
    public static void main(String[] args) {
        Scanner s = new Scanner(System.in);
        System.out.println("Scanner class loaded OK: " + s.getClass().getName());
    }
}
EOF
javac -d /tmp /tmp/ScannerTest.java
<cratonvm-binary> --java-home <real-jdk-home> -cp /tmp ScannerTest < /dev/null
# -> NoClassDefFoundError: java/util/Scanner, caused by IllegalArgumentException: PatternSyntaxException
#    at java/util/Scanner.<clinit> (Scanner.java:415)
```

## Evidence

```text
WildFly discovery: EarClassLoadingTestCase, round-5 rerun, 2026-07-10, worktree
test/wildfly-full-suite-20260707, binary frozen-cratonvm-wildfly-bugbash-v4-20260710
(built from dev@4723e059, post WFLYLOG0078 fix)

Isolated repro: /tmp/ScannerTest.java, /tmp/PatternTest.java on the Azure host, same binary
```
