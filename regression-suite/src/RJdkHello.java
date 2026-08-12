import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.util.Locale;

/**
 * JDK-only corpus: bootstrap, {@code System}, {@code String}, {@code PrintStream}.
 *
 * The smallest possible "did the real JDK boot" vector. Under {@code --jdk-only}
 * every class touched here must come from real boot-image bytes: no fabricated
 * {@code System}, no {@code SyntheticStub} behind {@code String.length()}.
 *
 * Determinism: no wall-clock, no hashes, no paths, no locale-sensitive
 * formatting (every {@code format} pins {@link Locale#ROOT}).
 */
public class RJdkHello {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** System.out / err / in must exist and be the real stream types. */
    static void systemStreams() {
        check(System.out != null, "System.out is null");
        check(System.err != null, "System.err is null");
        check(System.in != null, "System.in is null");
        check(System.out instanceof PrintStream, "System.out is not a PrintStream");
        check(System.err instanceof PrintStream, "System.err is not a PrintStream");
        // Well-known properties. The VALUES are host-specific, so nothing here
        // is pinned to a literal and nothing is printed -- but presence alone is
        // not an assertion: a VM whose property table answers "" for every key,
        // or whose System.getProperty is a hardcoded switch that disagrees with
        // its own Properties table, satisfies `!= null` exactly as well as a
        // correct one does. What is asserted instead is (a) non-emptiness,
        // (b) the closed set or cross-accessor identity each key is SPECIFIED to
        // agree with, and (c) that the single-key accessor and the whole-table
        // accessor answer the same thing.
        String javaVersion = System.getProperty("java.version");
        check(javaVersion != null && !javaVersion.isEmpty(), "java.version absent or empty");
        check(Character.isDigit(javaVersion.charAt(0)),
                "java.version must start with the feature number");

        String lineSep = System.getProperty("line.separator");
        check(lineSep != null && !lineSep.isEmpty(), "line.separator absent or empty");
        // System.lineSeparator() is SPECIFIED to be the line.separator property.
        check(lineSep.equals(System.lineSeparator()),
                "line.separator and System.lineSeparator() must agree");
        // Not a host pin: this is every value either can legally take.
        check(lineSep.equals("\n") || lineSep.equals("\r\n"), "line.separator shape");

        String pathSep = System.getProperty("path.separator");
        check(pathSep != null && pathSep.length() == 1, "path.separator absent or not one char");
        // File.pathSeparator is SPECIFIED to be this property's value.
        check(pathSep.equals(java.io.File.pathSeparator),
                "path.separator and File.pathSeparator must agree");
        check(pathSep.equals(":") || pathSep.equals(";"), "path.separator shape");

        // The two accessors are two views of one table. A VM that special-cases
        // the well-known keys in getProperty() while its Properties object never
        // received them answers differently here -- and every one of the reads
        // above would still have been green.
        java.util.Properties props = System.getProperties();
        check(props != null, "System.getProperties() returned null");
        for (String key : new String[] { "java.version", "line.separator", "path.separator" }) {
            check(System.getProperty(key).equals(props.getProperty(key)),
                    "getProperty and getProperties disagree on " + key);
        }

        check(System.getProperty("cratonvm.no.such.property") == null, "phantom property");
        check(props.getProperty("cratonvm.no.such.property") == null,
                "phantom property in the table");
        check(System.getProperty("cratonvm.no.such.property", "dflt").equals("dflt"),
                "getProperty default");
        check(System.lineSeparator() != null && !System.lineSeparator().isEmpty(),
                "System.lineSeparator");
        check(System.identityHashCode(null) == 0, "identityHashCode(null) != 0");
    }

    /** A PrintStream over a byte sink: every byte we write must come back. */
    static void printStreamRoundTrip() throws Exception {
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(sink, true, "UTF-8");
        ps.print("Hello");
        ps.print(", ");
        ps.print("world");
        ps.print('!');
        ps.print(' ');
        ps.print(42);
        ps.print(' ');
        ps.print(3.5d);
        ps.print(' ');
        ps.print(true);
        ps.print(' ');
        ps.print(new char[] { 'a', 'b' });
        ps.printf(Locale.ROOT, " [%s|%d|%05.2f]", "x", 7, 1.5);
        ps.flush();
        check(!ps.checkError(), "PrintStream reported an error");
        String got = sink.toString("UTF-8");
        check(got.equals("Hello, world! 42 3.5 true ab [x|7|01.50]"), "PrintStream body: " + got);
        System.out.println("CK RJdkHello stream=" + got);
        ps.close();
    }

    /** Core String behaviour, including a non-Latin-1 (UTF-16) body. */
    static void strings() {
        String s = "Hello, world!";
        check(s.length() == 13, "length");
        check(s.charAt(0) == 'H', "charAt");
        check(s.indexOf("world") == 7, "indexOf");
        check(s.substring(7, 12).equals("world"), "substring");
        check(s.toUpperCase(Locale.ROOT).equals("HELLO, WORLD!"), "toUpperCase");
        check(s.replace('l', 'L').equals("HeLLo, worLd!"), "replace");
        check(("Hello" + ", " + "world!").equals(s), "concat");
        check("".isEmpty() && "  ".isBlank(), "isEmpty/isBlank");
        check(String.join("-", "a", "b", "c").equals("a-b-c"), "join");
        check("a,b,,c".split(",").length == 4, "split");
        // Interning / constant-pool identity.
        String lit = "Hello, world!";
        check(lit == s, "string literals must be interned to the same instance");
        check(new String(s.toCharArray()) != s, "new String must be a distinct instance");
        check(new String(s.toCharArray()).equals(s), "new String equality");
        check(new String(s.toCharArray()).intern() == s, "intern");
        // Compact strings: a UTF-16 body must survive a byte round-trip.
        String utf16 = "é中文";
        byte[] b = utf16.getBytes(StandardCharsets.UTF_8);
        check(b.length == 8, "UTF-8 byte length: " + b.length);
        check(new String(b, StandardCharsets.UTF_8).equals(utf16), "UTF-8 round-trip");
        check(utf16.length() == 3, "UTF-16 code-unit length");
        System.out.println("CK RJdkHello hash=" + s.hashCode() + " utf16=" + utf16.hashCode());
    }

    public static void main(String[] args) throws Exception {
        systemStreams();
        printStreamRoundTrip();
        strings();
        System.out.println("CK RJdkHello checks=" + checks);
        System.out.println("PASS RJdkHello (" + checks + " checks)");
    }
}
