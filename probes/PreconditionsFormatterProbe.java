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
        row("int[] load negative", () -> {
            int[] a = new int[4];
            return a[-1];
        });
        row("int[] load on empty", () -> {
            int[] a = new int[0];
            return a[0];
        });
        row("int[] store oob", () -> {
            int[] a = new int[4];
            a[9] = 1;
            return "ok";
        });
        row("int[] store negative", () -> {
            int[] a = new int[4];
            a[-1] = 1;
            return "ok";
        });
        row("byte[] load oob", () -> {
            byte[] a = new byte[4];
            return a[9];
        });
        row("boolean[] load oob", () -> {
            boolean[] a = new boolean[4];
            return a[9];
        });
        row("char[] load oob", () -> {
            char[] a = new char[4];
            return a[9];
        });
        row("short[] load oob", () -> {
            short[] a = new short[4];
            return a[9];
        });
        row("long[] load oob", () -> {
            long[] a = new long[4];
            return a[9];
        });
        row("float[] load oob", () -> {
            float[] a = new float[4];
            return a[9];
        });
        row("double[] load oob", () -> {
            double[] a = new double[4];
            return a[9];
        });
        row("Object[] load oob (aaload)", () -> {
            Object[] a = new Object[4];
            return a[9];
        });
        row("Object[] store oob (aastore)", () -> {
            Object[] a = new Object[4];
            a[9] = "x";
            return "ok";
        });
        row("String[] store negative", () -> {
            String[] a = new String[4];
            a[-3] = "x";
            return "ok";
        });
        row("int[][] outer oob", () -> {
            int[][] a = new int[2][3];
            return a[5];
        });
        row("int[][] inner oob", () -> {
            int[][] a = new int[2][3];
            return a[0][7];
        });

        System.out.println("--- Array domain: reflective access ---");
        row("Array.get(int[4],9)", () -> java.lang.reflect.Array.get(new int[4], 9));
        row("Array.get(int[4],-1)", () -> java.lang.reflect.Array.get(new int[4], -1));
        row("Array.getInt(int[4],9)", () -> java.lang.reflect.Array.getInt(new int[4], 9));
        row("Array.set(int[4],9,v)", () -> {
            java.lang.reflect.Array.set(new int[4], 9, 1);
            return "ok";
        });
        row("Array.setInt(int[4],9,v)", () -> {
            java.lang.reflect.Array.setInt(new int[4], 9, 1);
            return "ok";
        });
        row("Array.get(Object[4],9)", () -> java.lang.reflect.Array.get(new Object[4], 9));
        row("Array.get(Object[4],4)", () -> java.lang.reflect.Array.get(new Object[4], 4));
        row("Array.getLength(int[4])", () -> java.lang.reflect.Array.getLength(new int[4]));
        row("Array.get(null,0)", () -> java.lang.reflect.Array.get(null, 0));
        row("Array.get(\"hello\",0)", () -> java.lang.reflect.Array.get("hello", 0));
        row("Array.getLength(\"hello\")", () -> java.lang.reflect.Array.getLength("hello"));
        row("Array.getInt(Object[4],0)", () -> java.lang.reflect.Array.getInt(new Object[4], 0));
        row("Array.getInt(long[4],0)", () -> java.lang.reflect.Array.getInt(new long[4], 0));
        row("Array.getLong(int[4],0)", () -> java.lang.reflect.Array.getLong(new int[4], 0));
        row("Array.set(int[4],0,\"x\")", () -> {
            java.lang.reflect.Array.set(new int[4], 0, "x");
            return "ok";
        });
        row("Array.set(null,0,v)", () -> {
            java.lang.reflect.Array.set(null, 0, 1);
            return "ok";
        });
        row("Array.setInt(long[4],9,v)", () -> {
            java.lang.reflect.Array.setInt(new long[4], 9, 1);
            return "ok";
        });
        row("Array.newInstance(int,-1)",
                () -> java.lang.reflect.Array.newInstance(int.class, -1));

        System.out.println("--- Array domain: System.arraycopy ---");
        row("arraycopy int last src", () -> {
            System.arraycopy(new int[4], 0, new int[16], 0, 9);
            return "ok";
        });
        row("arraycopy int last dst", () -> {
            System.arraycopy(new int[16], 0, new int[4], 0, 9);
            return "ok";
        });
        row("arraycopy int srcPos<0", () -> {
            System.arraycopy(new int[4], -1, new int[4], 0, 1);
            return "ok";
        });
        row("arraycopy int dstPos<0", () -> {
            System.arraycopy(new int[4], 0, new int[4], -1, 1);
            return "ok";
        });
        row("arraycopy int length<0", () -> {
            System.arraycopy(new int[4], 0, new int[4], 0, -1);
            return "ok";
        });
        row("arraycopy int srcPos<0 AND length<0", () -> {
            System.arraycopy(new int[4], -1, new int[4], 0, -1);
            return "ok";
        });
        row("arraycopy int both pos<0", () -> {
            System.arraycopy(new int[4], -1, new int[4], -2, 1);
            return "ok";
        });
        row("arraycopy int srcPos in, last src out", () -> {
            System.arraycopy(new int[4], 3, new int[16], 0, 2);
            return "ok";
        });
        row("arraycopy byte last src", () -> {
            System.arraycopy(new byte[4], 0, new byte[16], 0, 9);
            return "ok";
        });
        row("arraycopy boolean last src", () -> {
            System.arraycopy(new boolean[4], 0, new boolean[16], 0, 9);
            return "ok";
        });
        row("arraycopy char last dst", () -> {
            System.arraycopy(new char[16], 0, new char[4], 0, 9);
            return "ok";
        });
        row("arraycopy short srcPos<0", () -> {
            System.arraycopy(new short[4], -1, new short[4], 0, 1);
            return "ok";
        });
        row("arraycopy long last src", () -> {
            System.arraycopy(new long[4], 0, new long[16], 0, 9);
            return "ok";
        });
        row("arraycopy float dstPos<0", () -> {
            System.arraycopy(new float[4], 0, new float[4], -1, 1);
            return "ok";
        });
        row("arraycopy double last dst", () -> {
            System.arraycopy(new double[16], 0, new double[4], 0, 9);
            return "ok";
        });
        row("arraycopy Object[] last src", () -> {
            System.arraycopy(new Object[4], 0, new Object[16], 0, 9);
            return "ok";
        });
        row("arraycopy String[] last dst", () -> {
            System.arraycopy(new String[16], 0, new String[4], 0, 9);
            return "ok";
        });
        row("arraycopy String[] srcPos<0", () -> {
            System.arraycopy(new String[4], -1, new String[4], 0, 1);
            return "ok";
        });
        row("arraycopy int[][] last src", () -> {
            System.arraycopy(new int[4][1], 0, new int[16][1], 0, 9);
            return "ok";
        });
        row("arraycopy self overlap oob", () -> {
            int[] a = new int[4];
            System.arraycopy(a, 2, a, 0, 9);
            return "ok";
        });
        row("arraycopy null src", () -> {
            System.arraycopy(null, 0, new int[4], 0, 1);
            return "ok";
        });
        row("arraycopy null dst", () -> {
            System.arraycopy(new int[4], 0, null, 0, 1);
            return "ok";
        });
        row("arraycopy non-array src", () -> {
            System.arraycopy("hello", 0, new int[4], 0, 1);
            return "ok";
        });
        row("arraycopy non-array dst", () -> {
            System.arraycopy(new int[4], 0, "hello", 0, 1);
            return "ok";
        });
        row("arraycopy type mismatch", () -> {
            System.arraycopy(new int[4], 0, new long[4], 0, 1);
            return "ok";
        });
        row("arraycopy prim-to-ref mismatch", () -> {
            System.arraycopy(new int[4], 0, new Object[4], 0, 1);
            return "ok";
        });
        row("arraycopy zero length past end", () -> {
            System.arraycopy(new int[4], 4, new int[4], 4, 0);
            return "ok";
        });

        System.out.println("--- Array domain: arraycopy check precedence ---");
        // Which check wins when two are violated at once. The class differs
        // between them (ASE vs AIOOBE vs NPE), so this is control flow, not
        // just wording.
        row("arraycopy null src + srcPos<0", () -> {
            System.arraycopy(null, -1, new int[4], 0, 1);
            return "ok";
        });
        row("arraycopy non-array src + srcPos<0", () -> {
            System.arraycopy("hello", -1, new int[4], 0, 1);
            return "ok";
        });
        row("arraycopy type mismatch + length oob", () -> {
            System.arraycopy(new int[4], 0, new long[4], 0, 9);
            return "ok";
        });
        row("arraycopy type mismatch + srcPos<0", () -> {
            System.arraycopy(new int[4], -1, new long[4], 0, 1);
            return "ok";
        });
        row("arraycopy ref-to-prim mismatch + oob", () -> {
            System.arraycopy(new Object[4], 0, new int[4], 0, 9);
            return "ok";
        });
        row("arraycopy incompatible refs in bounds", () -> {
            Object[] src = new Object[] { "a", "b" };
            System.arraycopy(src, 0, new Integer[2], 0, 2);
            return "ok";
        });
        row("arraycopy last src wins over last dst", () -> {
            System.arraycopy(new int[4], 0, new int[2], 0, 9);
            return "ok";
        });
        row("arraycopy dstPos<0 wins over last src", () -> {
            System.arraycopy(new int[4], 0, new int[4], -1, 9);
            return "ok";
        });
        row("arraycopy length<0 wins over last src", () -> {
            System.arraycopy(new int[4], 5, new int[4], 0, -1);
            return "ok";
        });
        row("arraycopy srcPos+length overflows int", () -> {
            System.arraycopy(new int[4], Integer.MAX_VALUE, new int[4], 0, 2);
            return "ok";
        });

        System.out.println("--- Array domain: Arrays helpers ---");
        row("Arrays.copyOfRange(int[4],-1,2)",
                () -> java.util.Arrays.copyOfRange(new int[4], -1, 2));
        row("Arrays.copyOfRange(int[4],3,2)",
                () -> java.util.Arrays.copyOfRange(new int[4], 3, 2));
        row("Arrays.fill(int[4],0,9,v)", () -> {
            java.util.Arrays.fill(new int[4], 0, 9, 1);
            return "ok";
        });
        row("Arrays.sort(int[4],0,9)", () -> {
            java.util.Arrays.sort(new int[4], 0, 9);
            return "ok";
        });

        System.out.println("--- Array domain: after JIT warm-up ---");
        int[] warm = new int[4];
        for (int i = 0; i < 200_000; i++) {
            hotLoad(warm, i & 3);
            hotStore(warm, i & 3);
            hotCopy(warm, 4);
        }
        row("hot int[] load oob", () -> hotLoad(new int[4], 9));
        row("hot int[] load negative", () -> hotLoad(new int[4], -1));
        row("hot int[] store oob", () -> {
            hotStore(new int[4], 9);
            return "ok";
        });
        row("hot arraycopy last src", () -> {
            hotCopy(new int[4], 9);
            return "ok";
        });

        System.out.println("--- Array domain: catch shapes ---");
        System.out.println("a[9] caught by catch(ArrayIndexOutOfBoundsException): "
                + caughtAsAioobe());
        System.out.println("arraycopy oob caught by catch(IndexOutOfBoundsException): "
                + arraycopyCaughtAsIoobe());

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

    static int hotLoad(int[] a, int i) {
        return a[i];
    }

    static void hotStore(int[] a, int i) {
        a[i] = i;
    }

    static void hotCopy(int[] a, int len) {
        System.arraycopy(a, 0, new int[16], 0, len);
    }

    static boolean caughtAsAioobe() {
        int[] a = new int[4];
        try {
            int unused = a[9];
            return false;
        } catch (ArrayIndexOutOfBoundsException e) {
            return true;
        } catch (IndexOutOfBoundsException e) {
            return false;
        }
    }

    static boolean arraycopyCaughtAsIoobe() {
        try {
            System.arraycopy(new int[4], 0, new int[4], 0, 9);
            return false;
        } catch (IndexOutOfBoundsException e) {
            return true;
        }
    }
}
