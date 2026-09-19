import java.lang.reflect.Field;
import java.nio.Buffer;
import java.nio.CharBuffer;

/**
 * `Buffer.address` on CharBuffers whose `offset` is NOT zero.
 *
 * Two things this gets right that BUFALL.java got wrong:
 *
 *  1. **Static type.** The nio natives are registered on the CONCRETE class
 *     with the concrete return descriptor (`java/nio/CharBuffer.flip
 *     ()Ljava/nio/CharBuffer;`). Calling `flip()` through a `java.nio.Buffer`
 *     reference emits `invokevirtual java/nio/Buffer.flip()Ljava/nio/Buffer;`,
 *     which those registrations do NOT match — so the probe silently measured
 *     the real JDK and reported OK no matter what CratonVM does. Every call
 *     below goes through a `CharBuffer`-typed reference.
 *
 *  2. **A non-zero offset.** A real `HeapCharBuffer` sets
 *     `address = ARRAY_CHAR_BASE_OFFSET + offset * 2`, so only a buffer with
 *     `offset != 0` can distinguish "preserved the real address" from "wrote
 *     the constant 16". `wrap(array, off, len)` and `slice()` both produce one;
 *     `allocate()` never does.
 *
 * `subSequence` is included because it is the one native that ALLOCATES a
 * buffer carrying the parent's offset.
 *
 * On HotSpot every line reads OK.
 */
public class CBSLICE {

    static Field ADDRESS;
    static Field OFFSET;
    static int fails = 0;

    static long addr(Buffer b) {
        try {
            return ADDRESS.getLong(b);
        } catch (Throwable t) {
            return Long.MIN_VALUE;
        }
    }

    static int off(Buffer b) {
        try {
            return OFFSET.getInt(b);
        } catch (Throwable t) {
            return -1;
        }
    }

    /** What a real Heap*Buffer would carry: base offset + offset * scale. */
    static long want(Buffer b) {
        return 16L + (long) off(b) * 2L;
    }

    static void check(String label, CharBuffer b) {
        long got = addr(b);
        long want = want(b);
        if (got != want) {
            fails++;
            System.out.println(label + " BAD address=" + got + " want=" + want + " offset=" + off(b));
        } else {
            System.out.println(label + " OK address=" + got + " offset=" + off(b));
        }
    }

    static void bulk(String label, Runnable r) {
        try {
            r.run();
            System.out.println(label + " OK");
        } catch (Throwable t) {
            fails++;
            System.out.println(label + " BAD threw " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        ADDRESS = Buffer.class.getDeclaredField("address");
        ADDRESS.setAccessible(true);
        OFFSET = CharBuffer.class.getDeclaredField("offset");
        OFFSET.setAccessible(true);

        char[] backing = new char[512];
        java.util.Arrays.fill(backing, 'x');

        // offset = 64 via wrap(array, off, len).
        CharBuffer w = CharBuffer.wrap(backing, 64, 200);
        check("wrap(off=64) fresh", w);
        w.mark();
        check("wrap mark()", w);
        w.reset();
        check("wrap reset()", w);
        w.flip();
        check("wrap flip()", w);
        w.clear();
        check("wrap clear()", w);
        w.rewind();
        check("wrap rewind()", w);

        // offset = parent offset + position via slice().
        CharBuffer parent = CharBuffer.wrap(backing);
        parent.position(100);
        parent.limit(300);
        CharBuffer s = parent.slice();
        check("slice fresh", s);
        s.flip();
        check("slice flip()", s);
        s.clear();
        check("slice clear()", s);
        s.rewind();
        check("slice rewind()", s);

        // subSequence is the native that allocates a buffer carrying the
        // parent's offset -- the one a flat `address = 16` gets wrong.
        CharBuffer sub = s.subSequence(10, 60);
        check("subSequence", sub);
        CharBuffer subOfSub = sub.subSequence(5, 20);
        check("subSequence of subSequence", subOfSub);

        // The payoff: a bulk put READS the source's address, so a wrong one
        // either throws or silently copies from the wrong place.
        bulk("put(sliced) copies the right chars", () -> {
            CharBuffer src = CharBuffer.wrap(backing, 0, 512);
            for (int i = 0; i < 512; i++) {
                src.put(i, (char) ('a' + (i % 26)));
            }
            src.position(100);
            src.limit(200);
            CharBuffer sl = src.slice();
            CharBuffer dst = CharBuffer.allocate(256);
            dst.put(sl);
            if (dst.position() != 100) {
                throw new RuntimeException("moved " + dst.position());
            }
            dst.flip();
            for (int i = 0; i < 100; i++) {
                char want = (char) ('a' + ((100 + i) % 26));
                if (dst.get(i) != want) {
                    throw new RuntimeException("char " + i + " = " + dst.get(i) + " want " + want);
                }
            }
        });

        bulk("subSequence().toString() is the right text", () -> {
            CharBuffer src = CharBuffer.wrap(backing, 0, 512);
            for (int i = 0; i < 512; i++) {
                src.put(i, (char) ('a' + (i % 26)));
            }
            src.position(100);
            src.limit(300);
            CharBuffer sl = src.slice();
            String got = sl.subSequence(10, 20).toString();
            StringBuilder want = new StringBuilder();
            for (int i = 110; i < 120; i++) {
                want.append((char) ('a' + (i % 26)));
            }
            if (!got.equals(want.toString())) {
                throw new RuntimeException("got '" + got + "' want '" + want + "'");
            }
        });

        System.out.println("CBSLICEDONE fails=" + fails);
    }
}
