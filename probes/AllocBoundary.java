import java.lang.reflect.Field;

/** Where does `allocateMemory` stop throwing IllegalArgumentException and start
 *  throwing OutOfMemoryError? The JDK's `checkSize` refuses above a cap that is
 *  not the same as "the malloc failed", and the two are different contracts for
 *  a caller. Measured, because CratonVM has to reproduce the boundary, not a
 *  guess about it. */
public class AllocBoundary {
    public static void main(String[] a) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) f.get(null);
        long[] sizes = {
            0L, 1L, 16L,
            1L << 20, 1L << 30,
            1L << 40, 1L << 47, 1L << 48, 1L << 49,
            1L << 55, 1L << 61, 1L << 62,
            Long.MAX_VALUE / 2, Long.MAX_VALUE - 1, Long.MAX_VALUE,
            -1L, Long.MIN_VALUE,
        };
        for (long s : sizes) {
            String out;
            try {
                long p = u.allocateMemory(s);
                out = (p == 0 ? "returned 0" : "allocated");
                if (p != 0) u.freeMemory(p);
            } catch (Throwable t) {
                out = "THREW " + t.getClass().getName();
            }
            System.out.println("allocateMemory " + s + " |" + out + "|");
        }
        System.out.println("DONE AllocBoundary");
    }
}
