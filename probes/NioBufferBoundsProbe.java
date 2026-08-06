import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * Does an out-of-range `ByteBuffer` access throw, and if not, WHAT does it read?
 *
 * `probes/NioOutOfBoundsClassProbe` found that `ByteBuffer.get(99)` on an
 * 8-byte buffer returns silently where HotSpot throws
 * `IndexOutOfBoundsException`. A missing bounds check is not a message defect,
 * and the first question is not "which exception" but **what memory answered**:
 *
 *   * a returned 0 for every out-of-range index is a clamp or an early-out --
 *     wrong, but contained;
 *   * a returned NON-zero, or a value that changes with the index, means the
 *     read reached storage it was not entitled to.
 *
 * So this prints VALUES, not just "threw / did not throw", and reads well past
 * the end so an accidental neighbour is visible. It also writes a recognisable
 * pattern into a second buffer first: if an out-of-range read on buffer A
 * returns bytes of buffer B, the value says so out loud.
 *
 * Covers the shapes that dispatch differently in this VM: `wrap`, `allocate`,
 * `allocateDirect`, a `slice` (non-zero offset), and the multi-byte accessors,
 * which have their own bounds arithmetic (`limit - 3` for an int, etc.).
 *
 * Run on HotSpot first; every row there should be an exception.
 */
public class NioBufferBoundsProbe {

    static void t(String label, java.util.function.Supplier<Object> f) {
        try {
            System.out.println("  " + label + " => NO-THROW value=" + f.get());
        } catch (Throwable e) {
            String m = e.getMessage();
            System.out.println("  " + label + " => " + e.getClass().getSimpleName()
                    + (m == null ? "" : " msg=" + m));
        }
    }

    public static void main(String[] args) {
        // A neighbour buffer full of a recognisable pattern. If an out-of-range
        // read on `buf` ever returns 0x5A, the read escaped its own storage.
        ByteBuffer neighbour = ByteBuffer.allocate(64);
        for (int i = 0; i < neighbour.capacity(); i++) {
            neighbour.put(i, (byte) 0x5A);
        }

        byte[] backing = new byte[8];
        for (int i = 0; i < backing.length; i++) {
            backing[i] = (byte) (i + 1);
        }

        System.out.println("wrap(byte[8]) len=8, values 01..08");
        ByteBuffer w = ByteBuffer.wrap(backing);
        t("get(0)     in range ", () -> w.get(0));
        t("get(7)     in range ", () -> w.get(7));
        t("get(8)     one past ", () -> w.get(8));
        t("get(9)              ", () -> w.get(9));
        t("get(99)             ", () -> w.get(99));
        t("get(1000)           ", () -> w.get(1000));
        t("get(-1)             ", () -> w.get(-1));
        t("get(MIN_VALUE)      ", () -> w.get(Integer.MIN_VALUE));

        System.out.println("allocate(8)");
        ByteBuffer a = ByteBuffer.allocate(8);
        t("get(99)             ", () -> a.get(99));
        t("put(99,1)           ", () -> { a.put(99, (byte) 1); return "ok"; });

        System.out.println("allocateDirect(8)");
        ByteBuffer d = ByteBuffer.allocateDirect(8);
        t("get(99)             ", () -> d.get(99));
        t("put(99,1)           ", () -> { d.put(99, (byte) 1); return "ok"; });

        System.out.println("multi-byte accessors on wrap(byte[8])");
        ByteBuffer m = ByteBuffer.wrap(backing).order(ByteOrder.BIG_ENDIAN);
        t("getInt(6)  needs 4  ", () -> m.getInt(6));
        t("getInt(99)          ", () -> m.getInt(99));
        t("getLong(4) needs 8  ", () -> m.getLong(4));
        t("getChar(7) needs 2  ", () -> m.getChar(7));

        System.out.println("slice: position(4).slice() has capacity 4");
        ByteBuffer s = ByteBuffer.wrap(backing);
        s.position(4);
        ByteBuffer sl = s.slice();
        t("slice.capacity()    ", () -> sl.capacity());
        t("slice.get(0)        ", () -> sl.get(0));
        t("slice.get(4) past   ", () -> sl.get(4));
        t("slice.get(99)       ", () -> sl.get(99));

        System.out.println("relative get past limit");
        ByteBuffer r = ByteBuffer.wrap(backing);
        r.position(8);
        t("get() at limit      ", () -> r.get());

        System.out.println("neighbour still all 0x5A? "
                + allSame(neighbour, (byte) 0x5A));
        System.out.println("backing unchanged 01..08? " + unchanged(backing));
        System.out.println("NIO-BUFFER-BOUNDS-PROBE-DONE");
    }

    static boolean allSame(ByteBuffer b, byte v) {
        for (int i = 0; i < b.capacity(); i++) {
            if (b.get(i) != v) {
                return false;
            }
        }
        return true;
    }

    static boolean unchanged(byte[] a) {
        for (int i = 0; i < a.length; i++) {
            if (a[i] != (byte) (i + 1)) {
                return false;
            }
        }
        return true;
    }
}
