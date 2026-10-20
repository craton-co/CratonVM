// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Which charset does `System.out` actually encode with?
 *
 * `file.encoding` has defaulted to UTF-8 since JEP 400 (Java 18), but
 * `stdout.encoding` / `stderr.encoding` did NOT follow it: they are derived
 * from the host (`jdk/internal/util/SystemProps.java` falls back to
 * `native.encoding`), and `java.lang.System`'s own property table says only
 * that the runtime "can be started with the system property set to UTF-8" —
 * any other command-line value is unspecified. So on a host whose console or
 * locale is not UTF-8, a conforming VM prints `?` for a character the console
 * charset cannot represent.
 *
 * Section 2 is the load-bearing part: it spells every non-ASCII character as
 * ASCII-only hex, both as a code point and as the bytes `System.out.charset()`
 * would put on the wire. Those lines diff cleanly between two VMs no matter
 * what either one's stdout encoding is, so they RECORD the divergence instead
 * of suffering it. Section 3 is the raw text, and is the only part that can
 * mangle.
 *
 * What a correct VM prints:
 *
 *   - every line of section 1 agrees with `java`'s own answers on the same
 *     host, in the same shell, with the same redirection;
 *   - `stream out.charset` equals `prop stdout.encoding`;
 *   - `verdict representable` is `true` under a UTF-8 console and `false`
 *     under a legacy one — and on a legacy console `enc latin1 bytes` reads
 *     `3f` (a literal `?`), not `c3 a9`.
 *
 * The divergence this exists for — CratonVM pins `stdout.encoding`,
 * `stderr.encoding` and `native.encoding` to `UTF-8` unconditionally
 * (`vm/src/vm/vm_init.rs`, `native-builtins/src/system_bootstrap.rs`):
 *
 *   prop stdout.encoding   HotSpot Cp1251 / cp437 / ANSI_X3.4-1968   CratonVM UTF-8
 *   enc latin1 bytes       HotSpot 3f                                CratonVM c3 a9
 *   raw mixed              HotSpot hello, ??? world                  CratonVM hello, (utf-8) world
 *
 * Reproduce — the second pair is the interesting one, because it forces a
 * non-UTF-8 stdout on Linux, without needing a Windows console:
 *
 *   javac -d probes probes/StdoutEncoding.java
 *
 *   java                        -cp probes StdoutEncoding
 *   cratonvm                    -cp probes StdoutEncoding
 *
 *   LC_ALL=C java               -cp probes StdoutEncoding   # HotSpot follows the locale
 *   LC_ALL=C cratonvm           -cp probes StdoutEncoding   # CratonVM does not
 *
 *   java     -Dstdout.encoding=UTF-8 -Dstderr.encoding=UTF-8 -cp probes StdoutEncoding
 *   cratonvm -Dstdout.encoding=UTF-8 -Dstderr.encoding=UTF-8 -cp probes StdoutEncoding
 *
 * Section 1 and 2 are byte-identical between the two VMs on a UTF-8 host, so
 * this is also a plain `--diff-hotspot` target:
 *
 *   cratonvm --diff-hotspot -cp probes StdoutEncoding
 *
 * On Windows, run the first pair from a console window and then again with
 * `> out.txt`, and diff the two runs of each VM against themselves: HotSpot's
 * answer is allowed to change when stdout stops being a console, and
 * CratonVM's cannot.
 *
 * The payload is built from code points rather than written as literals, so
 * `javac -encoding` cannot change what the probe measures and no editor can
 * quietly re-encode the three characters the whole page turns on.
 *
 * See docs/known-issues/stdout-encoding-differs-from-hotspot-on-windows-20260901.md
 */
import java.io.ByteArrayOutputStream;
import java.io.Console;
import java.io.PrintStream;
import java.nio.charset.Charset;
import java.nio.charset.StandardCharsets;
import java.util.Locale;

public class StdoutEncoding {

    /** Every value is fetched behind this, so one missing API cannot empty the report. */
    interface Q {
        String get() throws Throwable;
    }

    static void row(String key, Q q) {
        String v;
        try {
            v = q.get();
            if (v == null) v = "null";
        } catch (Throwable t) {
            v = "<" + t.getClass().getName() + ">";
        }
        System.out.printf("%-24s %s%n", key, v);
    }

    static String prop(String k) {
        String v = System.getProperty(k);
        return v == null ? "null" : v;
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) {
            if (sb.length() > 0) sb.append(' ');
            sb.append(String.format("%02x", x & 0xff));
        }
        return sb.toString();
    }

    /** Charset identity: the name AND the implementation class, which have diverged before. */
    static String cs(Charset c) {
        return c == null ? "null" : c.name() + " (" + c.getClass().getName() + ")";
    }

    // The three shapes. Built from code points rather than written as literals
    // or as \\u escapes, so this file is pure ASCII on disk and javac's
    // -encoding cannot change what the probe measures.
    static final String LATIN1 = new String(Character.toChars(0x00E9)); // LATIN SMALL LETTER E WITH ACUTE
    static final String CJK = new String(Character.toChars(0x4E2D));    // CJK UNIFIED IDEOGRAPH
    static final String ASTRAL = new String(Character.toChars(0x1F600));// GRINNING FACE, a surrogate pair

    static void payload(String name, String s) {
        StringBuilder cps = new StringBuilder();
        for (int i = 0; i < s.length(); ) {
            int cp = s.codePointAt(i);
            if (cps.length() > 0) cps.append(' ');
            cps.append(String.format("U+%04X", cp));
            i += Character.charCount(cp);
        }
        row("cp " + name, cps::toString);
        row("enc " + name + " bytes", () -> hex(s.getBytes(System.out.charset())));
        row("enc " + name + " survives", () -> {
            byte[] b = s.getBytes(System.out.charset());
            return String.valueOf(new String(b, System.out.charset()).equals(s));
        });
    }

    public static void main(String[] args) {
        System.out.println("== StdoutEncoding v1 ==");

        System.out.println("-- 1. what the VM says --");
        row("prop stdout.encoding", () -> prop("stdout.encoding"));
        row("prop stderr.encoding", () -> prop("stderr.encoding"));
        row("prop stdin.encoding", () -> prop("stdin.encoding"));
        row("prop file.encoding", () -> prop("file.encoding"));
        row("prop native.encoding", () -> prop("native.encoding"));
        row("prop sun.jnu.encoding", () -> prop("sun.jnu.encoding"));
        row("prop sun.stdout.enc", () -> prop("sun.stdout.encoding"));
        row("default charset", () -> cs(Charset.defaultCharset()));
        row("stream out.charset", () -> cs(System.out.charset()));
        row("stream err.charset", () -> cs(System.err.charset()));
        row("stream new PS.charset", () -> cs(new PrintStream(new ByteArrayOutputStream()).charset()));
        row("console present", () -> String.valueOf(System.console() != null));
        row("console charset", () -> {
            Console c = System.console();
            return c == null ? "n/a (no console)" : cs(c.charset());
        });
        row("console isTerminal", () -> {
            Console c = System.console();
            return c == null ? "n/a (no console)" : String.valueOf(c.isTerminal());
        });
        row("os.name", () -> prop("os.name"));

        System.out.println("-- 2. payload in ASCII (this section cannot mangle) --");
        payload("latin1", LATIN1);
        payload("cjk", CJK);
        payload("astral", ASTRAL);
        String all = LATIN1 + CJK + ASTRAL;
        row("verdict representable", () -> {
            byte[] b = all.getBytes(System.out.charset());
            return String.valueOf(new String(b, System.out.charset()).equals(all));
        });
        row("verdict canEncode", () -> String.valueOf(System.out.charset().newEncoder().canEncode(all)));

        System.out.println("-- 3. raw text (encoding-dependent; the only lines that can mangle) --");
        String mixed = "hello, " + all + " world";
        System.out.println("raw mixed      " + mixed);
        System.out.println("raw upper      " + mixed.toUpperCase(Locale.ROOT));
        System.out.println("raw sb         " + new StringBuilder().append('b').append(ASTRAL).append('a'));
        System.out.println("raw replchar   " + new String(new byte[] { (byte) 0xff, 0x28 }, StandardCharsets.UTF_8));
        System.err.println("raw stderr     " + mixed);

        System.out.println("== end ==");
    }
}
