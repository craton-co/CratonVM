// REncodingFidelity — the platform encodings, and the bytes that follow from
// them.
//
// JEP 400 (JDK 18) pinned `file.encoding` to UTF-8 and pinned ONLY
// `file.encoding`. `native.encoding`, `sun.jnu.encoding` and the three stream
// encodings still follow the platform: the locale's codeset on Unix, the ANSI
// code page (or the attached console's code page) on Windows. CratonVM pinned
// all five to UTF-8 under a comment asserting JDK 18 had done so, so under a
// C/POSIX locale it answered `UTF-8` where HotSpot answers `ANSI_X3.4-1968`
// and wrote UTF-8 bytes for a character HotSpot replaces with `?`.
//
// This vector is a DIFF against HotSpot in the same environment, which is what
// makes it locale-independent: it never asserts a particular encoding, only
// that both VMs name the same one and then emit the same bytes through it.
//
//   docs/known-issues/jdk-only/
//     bug-printstream-charset-answers-the-abstract-base-20260825.md §5, §6.1
import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.nio.charset.Charset;

public class REncodingFidelity {

    static int checks = 0;

    static void ck(String key, Object value) {
        checks++;
        System.out.println("CK REncodingFidelity " + key + "=" + value);
    }

    static void prop(String key) {
        ck("prop." + key, System.getProperty(key));
    }

    static void charset(String label, Charset c) {
        ck("cset." + label, c == null ? "null" : c.name());
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(String.format("%02x", x & 0xff));
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        // JEP 400: this one IS pinned, and must stay pinned.
        prop("file.encoding");
        // These four are the platform's, and were the defect.
        prop("native.encoding");
        prop("sun.jnu.encoding");
        prop("stdout.encoding");
        prop("stderr.encoding");
        prop("stdin.encoding");
        // JDK 19 replaced the `sun.`-prefixed spellings; both read null on a
        // stock JDK 25 and must read null here. Seeding them was drift in the
        // same family, in the other direction.
        prop("sun.stdout.encoding");
        prop("sun.stderr.encoding");

        // `Charset.defaultCharset()` follows `file.encoding` and so is UTF-8
        // everywhere; the stream charsets follow the stream properties. A VM
        // that returns one constant for all three passes the property checks
        // above and still fails here.
        charset("defaultCharset", Charset.defaultCharset());
        charset("System.out", System.out.charset());
        charset("System.err", System.err.charset());
        charset("newPrintStream", new PrintStream(new ByteArrayOutputStream()).charset());

        // The object must be CONCRETE, not the abstract `java.nio.charset.
        // Charset` base — `newEncoder()` is abstract there and every caller
        // that encodes through the stream dies with `AbstractMethodError`
        // rather than doing anything. Report the outcome, not the class name:
        // the class name is a JDK implementation detail, the encoder working
        // is the contract.
        String[] labels = { "out", "err", "default" };
        Charset[] sets = { System.out.charset(), System.err.charset(), Charset.defaultCharset() };
        for (int i = 0; i < sets.length; i++) {
            String r;
            try {
                r = sets[i].newEncoder() != null && sets[i].newDecoder() != null ? "ok" : "null";
            } catch (Throwable t) {
                r = t.getClass().getName();
            }
            ck("coder." + labels[i], r);
        }

        // The bytes. `Ж` is unmappable in US-ASCII (`?`), two bytes in UTF-8
        // and one in windows-1251 — so this separates the three answers the
        // property could have had, through the encoder the stream carries.
        String s = "[Ж]";
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(sink, true, System.out.charset());
        ps.print(s);
        ps.flush();
        ck("bytes.viaStreamCharset", hex(sink.toByteArray()));
        ck("bytes.viaGetBytes", hex(s.getBytes(System.out.charset())));

        // End to end, through the real `System.out`: whatever HotSpot puts on
        // fd 1 for this line, CratonVM must put there too. Deliberately NOT
        // hex — this is the only check that exercises the encoder the VM's own
        // `print` natives use rather than one the vector constructed.
        ck("direct", s);

        System.out.println("CK REncodingFidelity checks=" + checks);
        System.out.println("PASS REncodingFidelity (" + checks + " checks)");
    }
}
