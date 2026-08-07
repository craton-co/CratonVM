import java.io.StringReader;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.StandardCharsets;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * The real-world consumers of {@code CharBuffer.wrap(...)} — the callers the
 * CratonVM wrap native was written for (Tomcat's {@code MessageBytes.toBytes},
 * icu4j's {@code ICUResourceBundleReader.getStringV2}) plus every neighbour
 * that would notice a change in the returned <em>class</em>.
 *
 * <p>Companion to {@link CharBufferWrapProbe}, which covers the factories and
 * the range checks. This one exists because
 * {@code charbuffer-wrap-string-subsequence-does-not-bounds-check-FIXED-20260806.md}
 * changed what {@code wrap(CharSequence)} returns — from a synthetic
 * array-backed buffer to the real {@code java.nio.StringCharBuffer} HotSpot
 * returns — and the whole risk of that change lives in these rows. The
 * {@code SCB wrap(str,0,n)} block is the pre-flight: the 3-arg {@code wrap}
 * has always produced a real StringCharBuffer, so those rows measure exactly
 * what the 1-arg form inherits.
 *
 * <p>Every line must match HotSpot. Regenerate the oracle with:
 * {@snippet : java probes/CharBufferUsersProbe.java }
 */
public class CharBufferUsersProbe {
    static final String PLAIN = "Hello, World";

    static void row(String tag, java.util.concurrent.Callable<Object> c) {
        try {
            System.out.println(tag + " => " + c.call());
        } catch (Throwable t) {
            System.out.println(tag + " => " + t.getClass().getName() + " | " + t.getMessage());
        }
    }

    static String hex(ByteBuffer bb) {
        StringBuilder sb = new StringBuilder();
        for (int i = bb.position(); i < bb.limit(); i++) sb.append(String.format("%02x", bb.get(i)));
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        CharSequence sb = new StringBuilder(PLAIN);

        row("Charset.encode(wrap(str))",
                () -> hex(StandardCharsets.UTF_8.encode(CharBuffer.wrap(PLAIN))));
        row("encoder.encode(wrap(str))",
                () -> hex(StandardCharsets.UTF_8.newEncoder().encode(CharBuffer.wrap(PLAIN))));
        row("encoder.encode(wrap(sb))",
                () -> hex(StandardCharsets.UTF_8.newEncoder().encode(CharBuffer.wrap(sb))));
        row("encoder.encode(wrap(str,0,n))",
                () -> hex(StandardCharsets.UTF_8.newEncoder()
                        .encode(CharBuffer.wrap(PLAIN, 0, PLAIN.length()))));
        row("encoder.encode(wrap(sb,0,n))",
                () -> hex(StandardCharsets.UTF_8.newEncoder()
                        .encode(CharBuffer.wrap(sb, 0, sb.length()))));
        row("ISO encoder.encode(wrap(str))",
                () -> hex(StandardCharsets.ISO_8859_1.newEncoder().encode(CharBuffer.wrap(PLAIN))));
        row("encoder.encode(wrap(str).subSequence(2,7))",
                () -> hex(StandardCharsets.UTF_8.newEncoder()
                        .encode(CharBuffer.wrap(PLAIN).subSequence(2, 7))));
        row("encoder.encode(wrap(chars))",
                () -> hex(StandardCharsets.UTF_8.newEncoder()
                        .encode(CharBuffer.wrap(PLAIN.toCharArray()))));

        row("wrap(str).toString()", () -> CharBuffer.wrap(PLAIN).toString());
        row("wrap(sb).toString()", () -> CharBuffer.wrap(sb).toString());
        row("wrap(str).length()", () -> CharBuffer.wrap(PLAIN).length());
        row("wrap(str).charAt(4)", () -> CharBuffer.wrap(PLAIN).charAt(4));
        row("wrap(str).get(4)", () -> CharBuffer.wrap(PLAIN).get(4));
        row("wrap(str).remaining()", () -> CharBuffer.wrap(PLAIN).remaining());
        row("wrap(str).slice().toString()", () -> CharBuffer.wrap(PLAIN).slice().toString());
        row("wrap(str).duplicate().toString()", () -> CharBuffer.wrap(PLAIN).duplicate().toString());
        row("wrap(str).equals(wrap(str))",
                () -> CharBuffer.wrap(PLAIN).equals(CharBuffer.wrap(PLAIN)));
        row("wrap(str).compareTo(wrap(str))",
                () -> CharBuffer.wrap(PLAIN).compareTo(CharBuffer.wrap(PLAIN)));
        row("wrap(str).isReadOnly()", () -> CharBuffer.wrap(PLAIN).isReadOnly());
        row("wrap(str).hasArray()", () -> CharBuffer.wrap(PLAIN).hasArray());
        row("wrap(chars).hasArray()", () -> CharBuffer.wrap(PLAIN.toCharArray()).hasArray());
        row("wrap(chars).array().length", () -> CharBuffer.wrap(PLAIN.toCharArray()).array().length);
        row("wrap(chars) aliases caller array", () -> {
            char[] a = PLAIN.toCharArray();
            CharBuffer b = CharBuffer.wrap(a);
            a[0] = 'J';
            return b.toString();
        });
        row("wrap(chars).subSequence(0,5)",
                () -> CharBuffer.wrap(PLAIN.toCharArray()).subSequence(0, 5).toString());
        row("wrap(chars).subSequence(0,99)",
                () -> CharBuffer.wrap(PLAIN.toCharArray()).subSequence(0, 99).toString());
        row("alloc(8).subSequence(0,4).capacity()",
                () -> CharBuffer.allocate(8).subSequence(0, 4).capacity());

        // PRE-FLIGHT: the 3-arg wrap already returns a real StringCharBuffer on
        // CratonVM today, so these rows measure exactly what the 1-arg form
        // would inherit if it returned one too. Any divergence here is a
        // blocker that must be fixed BEFORE flipping wrap(CharSequence).
        row("SCB wrap(str,0,n).getClass", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).getClass().getName());
        row("SCB wrap(str,0,n).toString", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).toString());
        row("SCB wrap(str,0,n).length", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).length());
        row("SCB wrap(str,0,n).charAt(4)", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).charAt(4));
        row("SCB wrap(str,0,n).charAt(99)", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).charAt(99));
        row("SCB wrap(str,0,n).get(4)", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).get(4));
        row("SCB wrap(str,0,n).get()", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).get());
        row("SCB wrap(str,0,n).remaining", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).remaining());
        row("SCB wrap(str,0,n).hasArray", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).hasArray());
        row("SCB wrap(str,0,n).isReadOnly", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).isReadOnly());
        row("SCB wrap(str,0,n).slice", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).slice().toString());
        row("SCB wrap(str,0,n).duplicate", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).duplicate().toString());
        row("SCB wrap(str,0,n).subSequence(2,5)",
                () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).subSequence(2, 5).toString());
        row("SCB wrap(str,0,n).equals(self-shape)",
                () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length())
                        .equals(CharBuffer.wrap(PLAIN, 0, PLAIN.length())));
        row("SCB wrap(str,0,n) matcher", () -> {
            Matcher m = Pattern.compile("W(or)ld").matcher(CharBuffer.wrap(PLAIN, 0, PLAIN.length()));
            return m.find() + ":" + (m.find(0) ? m.group(1) : "-");
        });
        row("SCB wrap(str,0,n) append", () -> new StringBuilder()
                .append(CharBuffer.wrap(PLAIN, 0, PLAIN.length())).toString());
        row("SCB wrap(sb,0,n).toString", () -> CharBuffer.wrap(sb, 0, sb.length()).toString());
        row("SCB wrap(sb,0,n).charAt(4)", () -> CharBuffer.wrap(sb, 0, sb.length()).charAt(4));
        row("SCB wrap(str,0,n).chars().count", () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).chars().count());

        // java.util.regex over a wrapped CharSequence
        row("Pattern.matcher(wrap(str))", () -> {
            Matcher m = Pattern.compile("W(or)ld").matcher(CharBuffer.wrap(PLAIN));
            return m.find() + ":" + (m.find(0) ? m.group(1) : "-");
        });
        // Appendable/Readable
        row("Reader.read(alloc(16))", () -> {
            CharBuffer dst = CharBuffer.allocate(16);
            int n = new StringReader(PLAIN).read(dst);
            dst.flip();
            return n + ":" + dst.toString();
        });
        row("StringBuilder.append(wrap(str))",
                () -> new StringBuilder().append(CharBuffer.wrap(PLAIN)).toString());
        row("String.valueOf(wrap(str))", () -> String.valueOf(CharBuffer.wrap(PLAIN)));

        // mark/reset read java.nio.Buffer.mark as bytecode, so they are the
        // witness for slot 0 of the indexed layout landing on it. A buffer with
        // no mark must throw; one whose mark holds a stray pointer will not.
        row("wrap(char[]).reset() with no mark", () -> {
            CharBuffer.wrap(PLAIN.toCharArray()).reset();
            return "no-throw";
        });
        row("allocate(8).reset() with no mark", () -> {
            CharBuffer.allocate(8).reset();
            return "no-throw";
        });
        row("mark/get/reset round trip", () -> {
            CharBuffer b = CharBuffer.wrap(PLAIN.toCharArray());
            b.get();
            b.mark();
            b.get();
            b.get();
            b.reset();
            return "pos=" + b.position();
        });
        // Buffer's constructor is the only caller of its own position()/limit()
        // validation that CratonVM did not reach, and StringCharBuffer
        // .subSequence depends on it raising IllegalArgumentException.
        row("wrap(str).subSequence(3,2)", () -> CharBuffer.wrap(PLAIN).subSequence(3, 2).toString());
        row("wrap(str,0,n).subSequence(3,2)",
                () -> CharBuffer.wrap(PLAIN, 0, PLAIN.length()).subSequence(3, 2).toString());
        row("allocate(5).limit(3).position(4)", () -> CharBuffer.allocate(5).limit(3).position(4));

        // icu4j ICUResourceBundleReader.getStringV2 shape
        row("asCharBuffer().subSequence(0,5).toString()", () -> {
            ByteBuffer bytes = ByteBuffer.allocate(24);
            for (int i = 0; i < 12; i++) bytes.putChar(i * 2, PLAIN.charAt(i));
            return bytes.asCharBuffer().subSequence(0, 5).toString();
        });
        row("asCharBuffer().subSequence(0,99)", () -> {
            ByteBuffer bytes = ByteBuffer.allocate(24);
            for (int i = 0; i < 12; i++) bytes.putChar(i * 2, PLAIN.charAt(i));
            return bytes.asCharBuffer().subSequence(0, 99).toString();
        });
        System.out.println("PROBE-DONE");
    }
}
