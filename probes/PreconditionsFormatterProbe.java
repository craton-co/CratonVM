import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.IntBuffer;
import java.nio.LongBuffer;
import java.nio.charset.StandardCharsets;
import java.util.Objects;

/**
 * Ground truth for {@code jdk.internal.util.Preconditions}' exception-formatter
 * contract, on both sides of the split that
 * {@code docs/known-issues/preconditions-ignores-the-exception-formatter.md}
 * describes:
 *
 * <ul>
 *   <li><b>String-domain callers</b> pass {@code Preconditions.SIOOBE_FORMATTER},
 *       so the thrown class must be {@link StringIndexOutOfBoundsException};</li>
 *   <li><b>{@link Objects}-domain callers</b> (which is how every NIO buffer
 *       slice/get/put range check arrives) pass a {@code null} formatter, and
 *       the JDK's own fallback for that is a plain
 *       {@link IndexOutOfBoundsException} — never an
 *       {@link ArrayIndexOutOfBoundsException}, which is a <em>subclass</em>
 *       and therefore breaks a {@code catch} in the direction that matters;</li>
 *   <li><b>Array-domain callers</b> reach {@code Preconditions.AIOOBE_FORMATTER}
 *       and legitimately do get an {@link ArrayIndexOutOfBoundsException}.</li>
 * </ul>
 *
 * Every row prints the exception's <em>class</em> and its <em>message</em>. The
 * class is the half that changes control flow; the message is the half that
 * proves the formatter — and not a hand-rolled stand-in — produced it.
 *
 * Run against HotSpot to regenerate the oracle:
 * {@snippet : java probes/PreconditionsFormatterProbe.java }
 */
public class PreconditionsFormatterProbe {

    static final String PLAIN = "Hello, World";

    static int n = 0;

    interface Thrower {
        Object run() throws Throwable;
    }

    static void row(String shape, Thrower body) {
        n++;
        try {
            Object value = body.run();
            System.out.println(n + " " + shape + " => NO-THROW " + value);
        } catch (Throwable t) {
            System.out.println(n + " " + shape + " => " + t.getClass().getName()
                    + " | " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("--- String domain: SIOOBE_FORMATTER ---");
        row("substring(-1)", () -> PLAIN.substring(-1));
        row("substring(len+1)", () -> PLAIN.substring(PLAIN.length() + 1));
        row("substring(3,2)", () -> PLAIN.substring(3, 2));
        row("substring(-1,3)", () -> PLAIN.substring(-1, 3));
        row("substring(0,len+1)", () -> PLAIN.substring(0, PLAIN.length() + 1));
        row("substring(MIN,MAX)", () -> PLAIN.substring(Integer.MIN_VALUE, Integer.MAX_VALUE));
        row("charAt(-1)", () -> PLAIN.charAt(-1));
        row("charAt(len)", () -> PLAIN.charAt(PLAIN.length()));
        row("codePointAt(len)", () -> PLAIN.codePointAt(PLAIN.length()));

        byte[] utf8 = PLAIN.getBytes(StandardCharsets.UTF_8);
        row("new String(utf8,-1,2,\"UTF-8\")", () -> new String(utf8, -1, 2, "UTF-8"));
        row("new String(utf8,0,999,\"UTF-8\")", () -> new String(utf8, 0, 999, "UTF-8"));
        row("new String(utf8,-1,2,UTF_8)", () -> new String(utf8, -1, 2, StandardCharsets.UTF_8));
        row("new String(utf8,0,999,UTF_8)", () -> new String(utf8, 0, 999, StandardCharsets.UTF_8));
        char[] chars = PLAIN.toCharArray();
        row("new String(char[],-1,2)", () -> new String(chars, -1, 2));
        row("new String(char[],0,999)", () -> new String(chars, 0, 999));
        row("getChars(-1,2,dst,0)", () -> {
            char[] dst = new char[16];
            PLAIN.getChars(-1, 2, dst, 0);
            return "ok";
        });
        row("getBytes(0,999,dst,0)", () -> {
            byte[] dst = new byte[16];
            PLAIN.getBytes(0, 999, dst, 0);
            return "ok";
        });
        row("indexOf(int,int,int) beyond", () -> PLAIN.indexOf('o', 5, 999));
        row("subSequence(-1,3)", () -> PLAIN.subSequence(-1, 3));
        StringBuilder sb = new StringBuilder(PLAIN);
        row("sb.charAt(len)", () -> sb.charAt(sb.length()));
        row("sb.codePointAt(len)", () -> sb.codePointAt(sb.length()));
        row("sb.substring(3,2)", () -> sb.substring(3, 2));

        System.out.println("--- Objects domain: null formatter ---");
        row("Objects.checkIndex(-1,5)", () -> Objects.checkIndex(-1, 5));
        row("Objects.checkIndex(5,5)", () -> Objects.checkIndex(5, 5));
        row("Objects.checkFromToIndex(3,2,5)", () -> Objects.checkFromToIndex(3, 2, 5));
        row("Objects.checkFromToIndex(-1,3,5)", () -> Objects.checkFromToIndex(-1, 3, 5));
        row("Objects.checkFromToIndex(0,6,5)", () -> Objects.checkFromToIndex(0, 6, 5));
        row("Objects.checkFromIndexSize(-1,2,5)", () -> Objects.checkFromIndexSize(-1, 2, 5));
        row("Objects.checkFromIndexSize(0,6,5)", () -> Objects.checkFromIndexSize(0, 6, 5));
        row("Objects.checkFromIndexSize(4,2,5)", () -> Objects.checkFromIndexSize(4, 2, 5));

        System.out.println("--- NIO domain: reaches Objects/Preconditions ---");
        row("heap ByteBuffer.slice(-1,2)", () -> ByteBuffer.allocate(8).slice(-1, 2));
        row("heap ByteBuffer.slice(0,99)", () -> ByteBuffer.allocate(8).slice(0, 99));
        row("heap ByteBuffer.get(-1)", () -> ByteBuffer.allocate(8).get(-1));
        row("heap ByteBuffer.get(8)", () -> ByteBuffer.allocate(8).get(8));
        row("heap ByteBuffer.put(-1,b)", () -> ByteBuffer.allocate(8).put(-1, (byte) 1));
        row("heap ByteBuffer.put(8,b)", () -> ByteBuffer.allocate(8).put(8, (byte) 1));
        row("heap ByteBuffer.getInt(-1)", () -> ByteBuffer.allocate(8).getInt(-1));
        row("heap ByteBuffer.getInt(6)", () -> ByteBuffer.allocate(8).getInt(6));
        row("direct ByteBuffer.slice(-1,2)", () -> ByteBuffer.allocateDirect(8).slice(-1, 2));
        row("direct ByteBuffer.slice(0,99)", () -> ByteBuffer.allocateDirect(8).slice(0, 99));
        row("direct ByteBuffer.get(-1)", () -> ByteBuffer.allocateDirect(8).get(-1));
        row("IntBuffer.slice(0,99)", () -> IntBuffer.allocate(8).slice(0, 99));
        row("IntBuffer.get(-1)", () -> IntBuffer.allocate(8).get(-1));
        row("LongBuffer.slice(-1,2)", () -> LongBuffer.allocate(8).slice(-1, 2));
        row("CharBuffer.allocate(8).slice(0,99)", () -> CharBuffer.allocate(8).slice(0, 99));
        row("CharBuffer.allocate(8).charAt(9)", () -> CharBuffer.allocate(8).charAt(9));
        row("CharBuffer.wrap(str).subSequence(-1,2)",
                () -> CharBuffer.wrap(PLAIN).subSequence(-1, 2));
        row("CharBuffer.wrap(str).subSequence(0,99)",
                () -> CharBuffer.wrap(PLAIN).subSequence(0, 99));

        System.out.println("--- NIO contract neighbours ---");
        // `Buffer`'s absolute accessors check against the LIMIT, not the
        // capacity — a buffer that has been flipped rejects indices its
        // backing array could still serve.
        row("flipped ByteBuffer.get(pastLimit)", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.put(new byte[] { 1, 2, 3, 4 });
            b.flip();
            return b.get(6);
        });
        row("flipped ByteBuffer.put(pastLimit,b)", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.put(new byte[] { 1, 2, 3, 4 });
            b.flip();
            return b.put(6, (byte) 9);
        });
        row("ByteBuffer.put(itself)", () -> {
            ByteBuffer b = ByteBuffer.wrap(new byte[16]);
            return b.put(b);
        });
        row("ByteBuffer.get() past limit", () -> {
            ByteBuffer b = ByteBuffer.allocate(2);
            b.get();
            b.get();
            return b.get();
        });
        row("ByteBuffer.put(b) past limit", () -> {
            ByteBuffer b = ByteBuffer.allocate(1);
            b.put((byte) 1);
            return b.put((byte) 2);
        });
        row("ByteBuffer.get(dst,0,99)", () -> ByteBuffer.allocate(8).get(new byte[4], 0, 99));
        row("ByteBuffer.get(dst,-1,2)", () -> ByteBuffer.allocate(8).get(new byte[4], -1, 2));
        row("ByteBuffer.put(src,0,99)", () -> ByteBuffer.allocate(8).put(new byte[4], 0, 99));

        System.out.println("--- Array domain: AIOOBE stays AIOOBE ---");
        row("int[] load oob", () -> {
            int[] a = new int[4];
            return a[9];
        });
        row("System.arraycopy oob", () -> {
            System.arraycopy(new int[4], 0, new int[4], 0, 9);
            return "ok";
        });

        System.out.println("--- catch-shape assertions ---");
        System.out.println("substring(-1) caught by catch(StringIndexOutOfBoundsException): "
                + caughtAsSioobe());
        System.out.println("Objects.checkIndex(-1,5) NOT caught by catch(ArrayIndexOutOfBoundsException): "
                + notCaughtAsAioobe());

        System.out.println("PROBE-DONE");
    }

    static boolean caughtAsSioobe() {
        try {
            PLAIN.substring(-1);
            return false;
        } catch (StringIndexOutOfBoundsException e) {
            return true;
        } catch (IndexOutOfBoundsException e) {
            return false;
        }
    }

    static boolean notCaughtAsAioobe() {
        try {
            Objects.checkIndex(-1, 5);
            return false;
        } catch (ArrayIndexOutOfBoundsException e) {
            return false;
        } catch (IndexOutOfBoundsException e) {
            return true;
        }
    }
}
