import java.lang.reflect.Field;
import java.nio.Buffer;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.DoubleBuffer;
import java.nio.FloatBuffer;
import java.nio.IntBuffer;
import java.nio.LongBuffer;
import java.nio.ShortBuffer;

/**
 * Buffer-family sweep for the `address`-clobber defect.
 *
 * `java.nio.Buffer`'s hierarchy-wide field order is
 *   mark(0) position(1) limit(2) capacity(3) address(4)
 * and CratonVM's nio natives keep a parallel INDEXED layout whose slot 4 is
 * `mark`.  On a real-JDK buffer that indexed write lands on `address`.
 *
 * The CharBuffer half of this was fixed first (see CB*.java); this file asks
 * the same question of every buffer family and every mutator, because the
 * indexed write happens in one shared helper that all of them route through.
 *
 * Two independent checks per case:
 *   - `address` still reads its correct value after each mutator
 *   - the bulk `put(<same-kind>Buffer)` path, which is the only bulk op that
 *     actually READS `address` (via ScopedMemoryAccess/Unsafe.copyMemory)
 *
 * On HotSpot every line reads OK.
 */
public class BUFALL {

    static Field ADDRESS;
    static int fails = 0;

    static long addr(Buffer b) {
        try {
            return ADDRESS.getLong(b);
        } catch (Throwable t) {
            return Long.MIN_VALUE;
        }
    }

    static void ok(String label) {
        System.out.println(label + " OK");
    }

    static void bad(String label, String why) {
        fails++;
        System.out.println(label + " BAD " + why);
    }

    /** Every mutator on Buffer must leave `address` untouched. */
    static void mutators(String kind, Buffer b) {
        long want = addr(b);
        if (want == Long.MIN_VALUE) {
            bad(kind + ".address", "unreadable");
            return;
        }
        b.limit(b.capacity());
        b.position(0);
        step(kind, b, want, "fresh");
        b.position(4);
        step(kind, b, want, "position(4)");
        b.mark();
        step(kind, b, want, "mark()");
        b.position(8);
        b.reset();
        step(kind, b, want, "reset()");
        b.flip();
        step(kind, b, want, "flip()");
        b.clear();
        step(kind, b, want, "clear()");
        b.rewind();
        step(kind, b, want, "rewind()");
        b.limit(10);
        step(kind, b, want, "limit(10)");
        b.duplicate();
        step(kind, b, want, "duplicate()");
        b.clear();
        Buffer s = b.slice();
        long sw = addr(s);
        // A slice of a heap buffer starts at the same offset here (position 0),
        // so its address must equal the parent's -- and must NOT be -1.
        if (sw == -1L) {
            bad(kind + ".slice().address", "is -1");
        } else {
            ok(kind + ".slice().address=" + sw);
        }
    }

    static void step(String kind, Buffer b, long want, String after) {
        long got = addr(b);
        if (got != want) {
            bad(kind + ".address after " + after, "got=" + got + " want=" + want);
        } else {
            ok(kind + ".address after " + after);
        }
    }

    /** The bulk put that reads `address`; the tell that localises the defect. */
    static void bulk(String label, Runnable r) {
        try {
            r.run();
            ok(label);
        } catch (Throwable t) {
            bad(label, "threw " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        ADDRESS = Buffer.class.getDeclaredField("address");
        ADDRESS.setAccessible(true);

        mutators("ByteBuffer", ByteBuffer.allocate(64));
        mutators("CharBuffer", CharBuffer.allocate(64));
        mutators("ShortBuffer", ShortBuffer.allocate(64));
        mutators("IntBuffer", IntBuffer.allocate(64));
        mutators("LongBuffer", LongBuffer.allocate(64));
        mutators("FloatBuffer", FloatBuffer.allocate(64));
        mutators("DoubleBuffer", DoubleBuffer.allocate(64));

        bulk("ByteBuffer.put(ByteBuffer)", () -> {
            ByteBuffer s = ByteBuffer.allocate(128);
            for (int i = 0; i < 100; i++) {
                s.put((byte) i);
            }
            s.flip();
            ByteBuffer d = ByteBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });
        bulk("CharBuffer.put(CharBuffer)", () -> {
            CharBuffer s = CharBuffer.allocate(128);
            for (int i = 0; i < 100; i++) {
                s.put('w');
            }
            s.flip();
            CharBuffer d = CharBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });
        bulk("ShortBuffer.put(ShortBuffer)", () -> {
            ShortBuffer s = ShortBuffer.allocate(128);
            for (int i = 0; i < 100; i++) {
                s.put((short) i);
            }
            s.flip();
            ShortBuffer d = ShortBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });
        bulk("IntBuffer.put(IntBuffer)", () -> {
            IntBuffer s = IntBuffer.allocate(128);
            for (int i = 0; i < 100; i++) {
                s.put(i);
            }
            s.flip();
            IntBuffer d = IntBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });
        bulk("LongBuffer.put(LongBuffer)", () -> {
            LongBuffer s = LongBuffer.allocate(128);
            for (int i = 0; i < 100; i++) {
                s.put(i);
            }
            s.flip();
            LongBuffer d = LongBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });
        bulk("DoubleBuffer.put(DoubleBuffer)", () -> {
            DoubleBuffer s = DoubleBuffer.allocate(128);
            for (int i = 0; i < 100; i++) {
                s.put(i);
            }
            s.flip();
            DoubleBuffer d = DoubleBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });
        // A SLICED source is the case a flat `address = 16` gets wrong: the
        // slice's address must be 16 + offset*scale, not a constant.
        bulk("CharBuffer.put(sliced CharBuffer)", () -> {
            CharBuffer big = CharBuffer.allocate(256);
            for (int i = 0; i < 200; i++) {
                big.put('s');
            }
            big.position(50);
            big.limit(150);
            CharBuffer s = big.slice();
            CharBuffer d = CharBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });
        bulk("ByteBuffer.put(sliced ByteBuffer)", () -> {
            ByteBuffer big = ByteBuffer.allocate(256);
            for (int i = 0; i < 200; i++) {
                big.put((byte) i);
            }
            big.position(50);
            big.limit(150);
            ByteBuffer s = big.slice();
            ByteBuffer d = ByteBuffer.allocate(256);
            d.put(s);
            if (d.position() != 100) {
                throw new RuntimeException("moved " + d.position());
            }
        });

        System.out.println("BUFALLDONE fails=" + fails);
    }
}
