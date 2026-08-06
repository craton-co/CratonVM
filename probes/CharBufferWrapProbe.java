import java.nio.CharBuffer;

/**
 * `java.nio.CharBuffer`, and in particular what {@code CharBuffer.wrap} of a
 * {@code CharSequence} produces.
 *
 * Filed as
 * {@code docs/known-issues/charbuffer-wrap-string-subsequence-does-not-bounds-check.md}:
 * {@code CharBuffer.wrap("Hello, World").subSequence(0, 99)} returned a
 * 99-character buffer instead of throwing, reading 87 code units past the end
 * of the wrapped sequence and handing them back as content.
 *
 * The first block is the diagnosis the record guessed at rather than measured:
 * what class the receiver actually is, and whether the real {@code Buffer}
 * accessors can read its {@code position}/{@code limit}/{@code capacity}. Run
 * against HotSpot to regenerate the oracle:
 * {@snippet : java probes/CharBufferWrapProbe.java }
 */
public class CharBufferWrapProbe {

    static final String PLAIN = "Hello, World";

    static int n = 0;

    interface Thrower {
        Object run() throws Throwable;
    }

    static void row(String shape, Thrower body) {
        n++;
        try {
            Object value = body.run();
            System.out.println(n + " " + shape + " => " + render(value));
        } catch (Throwable t) {
            System.out.println(n + " " + shape + " => " + t.getClass().getName()
                    + " | " + t.getMessage());
        }
    }

    /** Render a CharSequence as its content plus its own idea of its state. */
    static String render(Object value) {
        if (value instanceof CharBuffer b) {
            return "CharBuffer[" + quote(b.toString()) + "] pos=" + b.position()
                    + " lim=" + b.limit() + " cap=" + b.capacity()
                    + " rem=" + b.remaining() + " len=" + b.length();
        }
        if (value instanceof CharSequence s) {
            return s.getClass().getSimpleName() + "[" + quote(s.toString())
                    + "] len=" + s.length();
        }
        return String.valueOf(value);
    }

    /**
     * A {@code CharSequence} that is neither a {@code String} nor anything the
     * VM special-cases — the shape Tomcat's {@code CharChunk} has, and the
     * reason a native ever stood in front of {@code CharBuffer.wrap}.
     */
    record CustomSeq(String backing) implements CharSequence {
        @Override public int length() {
            return backing.length();
        }
        @Override public char charAt(int index) {
            return backing.charAt(index);
        }
        @Override public CharSequence subSequence(int start, int end) {
            return new CustomSeq(backing.substring(start, end));
        }
        @Override public String toString() {
            return backing;
        }
    }

    static String hex(java.nio.ByteBuffer b) {
        StringBuilder sb = new StringBuilder();
        for (int i = b.position(); i < b.limit(); i++) {
            sb.append(String.format("%02X", b.get(i)));
        }
        return sb.toString();
    }

    /** Escape so a stray NUL from an out-of-range read is visible in a diff. */
    static String quote(String s) {
        StringBuilder sb = new StringBuilder(s.length() + 2).append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7F) {
                sb.append(c);
            } else {
                sb.append(String.format("\\u%04X", (int) c));
            }
        }
        return sb.append('"').toString();
    }

    public static void main(String[] args) {
        System.out.println("--- what wrap() actually returns ---");
        CharBuffer wrapped = CharBuffer.wrap(PLAIN);
        System.out.println("wrap(String).getClass()        = " + wrapped.getClass().getName());
        System.out.println("wrap(String) state             = " + render(wrapped));
        System.out.println("wrap(String).hasArray()        = " + wrapped.hasArray());
        System.out.println("wrap(String).isReadOnly()      = " + wrapped.isReadOnly());
        System.out.println("wrap(String).isDirect()        = " + wrapped.isDirect());
        CharBuffer allocated = CharBuffer.allocate(12);
        System.out.println("allocate(12).getClass()        = " + allocated.getClass().getName());
        System.out.println("allocate(12) state             = " + render(allocated));
        CharBuffer wrappedArray = CharBuffer.wrap(PLAIN.toCharArray());
        System.out.println("wrap(char[]).getClass()        = " + wrappedArray.getClass().getName());
        System.out.println("wrap(char[]) state             = " + render(wrappedArray));

        System.out.println("--- wrap(String): the reported defect ---");
        row("wrap(str).subSequence(0,5)", () -> CharBuffer.wrap(PLAIN).subSequence(0, 5));
        row("wrap(str).subSequence(0,len)", () -> CharBuffer.wrap(PLAIN).subSequence(0, 12));
        row("wrap(str).subSequence(3,3)", () -> CharBuffer.wrap(PLAIN).subSequence(3, 3));
        row("wrap(str).subSequence(-1,2)", () -> CharBuffer.wrap(PLAIN).subSequence(-1, 2));
        row("wrap(str).subSequence(0,99)", () -> CharBuffer.wrap(PLAIN).subSequence(0, 99));
        row("wrap(str).subSequence(3,2)", () -> CharBuffer.wrap(PLAIN).subSequence(3, 2));
        row("wrap(str).subSequence(len+1,len+1)", () -> CharBuffer.wrap(PLAIN).subSequence(13, 13));
        row("wrap(str).subSequence(MIN,MAX)",
                () -> CharBuffer.wrap(PLAIN).subSequence(Integer.MIN_VALUE, Integer.MAX_VALUE));

        System.out.println("--- subSequence is relative to POSITION ---");
        row("wrap(str).pos(5).subSequence(0,3)", () -> {
            CharBuffer b = CharBuffer.wrap(PLAIN);
            b.position(5);
            return b.subSequence(0, 3);
        });
        row("wrap(str).pos(5).subSequence(0,8)", () -> {
            CharBuffer b = CharBuffer.wrap(PLAIN);
            b.position(5);
            return b.subSequence(0, 8);
        });
        row("wrap(str).pos(5) rem", () -> {
            CharBuffer b = CharBuffer.wrap(PLAIN);
            b.position(5);
            return b.remaining();
        });
        row("wrap(str).lim(6).subSequence(0,7)", () -> {
            CharBuffer b = CharBuffer.wrap(PLAIN);
            b.limit(6);
            return b.subSequence(0, 7);
        });

        System.out.println("--- the same shapes on the other two CharBuffer kinds ---");
        row("allocate(12).subSequence(0,99)", () -> CharBuffer.allocate(12).subSequence(0, 99));
        row("allocate(12).subSequence(-1,2)", () -> CharBuffer.allocate(12).subSequence(-1, 2));
        row("wrap(char[]).subSequence(0,99)",
                () -> CharBuffer.wrap(PLAIN.toCharArray()).subSequence(0, 99));
        row("wrap(char[]).subSequence(0,5)",
                () -> CharBuffer.wrap(PLAIN.toCharArray()).subSequence(0, 5));
        row("wrap(str,2,7).subSequence(0,99)", () -> CharBuffer.wrap(PLAIN, 2, 7).subSequence(0, 99));
        row("wrap(str,2,7).subSequence(0,3)", () -> CharBuffer.wrap(PLAIN, 2, 7).subSequence(0, 3));

        System.out.println("--- allocate(): the kind that already works, for contrast ---");
        row("allocate(12).slice()", () -> CharBuffer.allocate(12).slice());
        row("allocate(12).duplicate()", () -> CharBuffer.allocate(12).duplicate());
        row("allocate(12).asReadOnlyBuffer()", () -> CharBuffer.allocate(12).asReadOnlyBuffer());
        row("allocate(12).subSequence(0,5)", () -> CharBuffer.allocate(12).subSequence(0, 5));
        row("allocate(12).get(99)", () -> CharBuffer.allocate(12).get(99));
        row("wrap(char[]).slice()", () -> CharBuffer.wrap(PLAIN.toCharArray()).slice());
        row("wrap(char[]).get(99)", () -> CharBuffer.wrap(PLAIN.toCharArray()).get(99));
        row("wrap(char[]).put(0,'x')", () -> CharBuffer.wrap(PLAIN.toCharArray()).put(0, 'x'));

        System.out.println("--- neighbours that must not regress ---");
        row("wrap(str).charAt(0)", () -> CharBuffer.wrap(PLAIN).charAt(0));
        row("wrap(str).charAt(11)", () -> CharBuffer.wrap(PLAIN).charAt(11));
        row("wrap(str).charAt(12)", () -> CharBuffer.wrap(PLAIN).charAt(12));
        row("wrap(str).charAt(-1)", () -> CharBuffer.wrap(PLAIN).charAt(-1));
        row("wrap(str).length()", () -> CharBuffer.wrap(PLAIN).length());
        row("wrap(str).toString()", () -> CharBuffer.wrap(PLAIN).toString());
        row("wrap(str).get()", () -> CharBuffer.wrap(PLAIN).get());
        row("wrap(str).get(3)", () -> CharBuffer.wrap(PLAIN).get(3));
        row("wrap(str).get(99)", () -> CharBuffer.wrap(PLAIN).get(99));
        row("wrap(str).slice()", () -> CharBuffer.wrap(PLAIN).slice());
        row("wrap(str).duplicate()", () -> CharBuffer.wrap(PLAIN).duplicate());
        row("wrap(str).asReadOnlyBuffer()", () -> CharBuffer.wrap(PLAIN).asReadOnlyBuffer());
        row("wrap(str).put('x')", () -> CharBuffer.wrap(PLAIN).put('x'));
        row("wrap(str).subSequence(0,5).toString()",
                () -> CharBuffer.wrap(PLAIN).subSequence(0, 5).toString());
        row("wrap(str).subSequence(2,7).charAt(0)",
                () -> CharBuffer.wrap(PLAIN).subSequence(2, 7).charAt(0));
        row("wrap(str).subSequence(2,7).length()",
                () -> CharBuffer.wrap(PLAIN).subSequence(2, 7).length());
        row("wrap(sb).toString()", () -> CharBuffer.wrap(new StringBuilder(PLAIN)).toString());
        row("wrap(sb).subSequence(0,99)",
                () -> CharBuffer.wrap(new StringBuilder(PLAIN)).subSequence(0, 99));

        System.out.println("--- Buffer.position/limit validation ---");
        // `StringCharBuffer.subSequence` relies on these throwing: it builds
        // its result through the `Buffer` constructor and CATCHES the
        // IllegalArgumentException to raise `IndexOutOfBoundsException`.
        row("wrap(str).position(99)", () -> CharBuffer.wrap(PLAIN).position(99));
        row("wrap(str).position(-1)", () -> CharBuffer.wrap(PLAIN).position(-1));
        row("wrap(str).limit(99)", () -> CharBuffer.wrap(PLAIN).limit(99));
        row("wrap(str).limit(-1)", () -> CharBuffer.wrap(PLAIN).limit(-1));
        row("allocate(5).limit(3).position(4)",
                () -> CharBuffer.allocate(5).limit(3).position(4));
        row("wrap(char[]).position(99)", () -> CharBuffer.wrap(PLAIN.toCharArray()).position(99));

        System.out.println("--- the callers the wrap() natives were written for ---");
        // Tomcat's `MessageBytes.toBytes` is `encoder.encode(CharBuffer.wrap(charChunk))`,
        // where the argument is a CharSequence that is NOT a String. That is
        // the whole reason a native stood in front of `wrap` — so it is the
        // thing to check after removing it.
        row("encode(wrap(String))", () -> hex(java.nio.charset.StandardCharsets.UTF_8
                .newEncoder().encode(CharBuffer.wrap(PLAIN))));
        row("encode(wrap(StringBuilder))", () -> hex(java.nio.charset.StandardCharsets.UTF_8
                .newEncoder().encode(CharBuffer.wrap(new StringBuilder(PLAIN)))));
        row("encode(wrap(CustomSeq))", () -> hex(java.nio.charset.StandardCharsets.UTF_8
                .newEncoder().encode(CharBuffer.wrap(new CustomSeq(PLAIN)))));
        row("encode(wrap(char[]))", () -> hex(java.nio.charset.StandardCharsets.UTF_8
                .newEncoder().encode(CharBuffer.wrap(PLAIN.toCharArray()))));
        row("encode(wrap(String,2,7))", () -> hex(java.nio.charset.StandardCharsets.UTF_8
                .newEncoder().encode(CharBuffer.wrap(PLAIN, 2, 7))));
        row("decode->subSequence->toString", () -> java.nio.charset.StandardCharsets.UTF_8
                .newDecoder().decode(java.nio.ByteBuffer.wrap(
                        PLAIN.getBytes(java.nio.charset.StandardCharsets.UTF_8)))
                .subSequence(2, 7).toString());
        row("wrap(CustomSeq).toString()", () -> CharBuffer.wrap(new CustomSeq(PLAIN)).toString());
        row("wrap(CustomSeq).charAt(1)", () -> CharBuffer.wrap(new CustomSeq(PLAIN)).charAt(1));
        row("wrap(CustomSeq).length()", () -> CharBuffer.wrap(new CustomSeq(PLAIN)).length());
        row("asCharBuffer().subSequence(0,5).toString()", () -> java.nio.ByteBuffer
                .wrap(new byte[] { 0, 'a', 0, 'b', 0, 'c', 0, 'd', 0, 'e', 0, 'f' })
                .asCharBuffer().subSequence(0, 5).toString());
        row("asCharBuffer().subSequence(0,99)", () -> java.nio.ByteBuffer
                .wrap(new byte[] { 0, 'a', 0, 'b', 0, 'c', 0, 'd', 0, 'e', 0, 'f' })
                .asCharBuffer().subSequence(0, 99));

        System.out.println("PROBE-DONE");
    }
}
