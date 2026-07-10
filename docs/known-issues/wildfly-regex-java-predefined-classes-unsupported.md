# `\p{java*}` predefined regex character classes unsupported — breaks `java.util.Scanner` entirely

Status: OPEN — new, found 2026-07-10 during round-5 sanity check (post WFLYLOG0078 fix)
Severity: **High** — blast radius is not WildFly-specific; breaks `java.util.Scanner` unconditionally for
every program that constructs one, since the failure happens in `Scanner`'s static initializer.
First confirmed: 2026-07-10, Azure worktree `test/wildfly-full-suite-20260707`, dev@4723e059 (round-5 binary,
`frozen-cratonvm-wildfly-bugbash-v4-20260710`)

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

## Fix sketch (not implemented — root cause identified for whoever picks this up)

Extend `translate_java_regex()` with a rewrite table for the `\p{java*}` family. Since the `regex` crate has
no equivalent Unicode property, the practical fix is to rewrite these constructs into function-checked
character classes evaluated against `Character`'s exact predicate semantics, e.g. by expanding
`\p{javaWhitespace}` into an explicit alternation/class matching exactly what
`Character.isWhitespace(int)` accepts (which is *not* the same set as Unicode's `White_Space` property —
notably it excludes non-breaking spaces ` `, ` `, ` ` — so a naive `\s`/`\p{White_Space}`
substitution would silently diverge from JDK behavior for those code points), rather than delegating to any
single existing Unicode property name.

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
