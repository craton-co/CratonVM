import java.lang.reflect.Field;
import java.lang.reflect.Method;

import jdk.internal.misc.Unsafe;

/** L1 R5 follow-up: are the repaired constants CORRECT, or merely non-zero?
 *
 *  `ClinitProbe` only asked "is it zero". That was enough to FIND the defect
 *  and is not enough to ACCEPT the fix: a backfill writing the wrong constant
 *  is also non-zero and would read as repaired.
 *
 *  The oracle is VM-INDEPENDENT, which matters because the right values
 *  legitimately differ between VMs (CratonVM uses a uniform 16-byte header and
 *  no compressed oops, so ARRAY_OBJECT_INDEX_SCALE is 8 here and 4 on a
 *  compressed-oops HotSpot). Diffing values across VMs would flag a correct
 *  answer as a defect. The INVARIANT does not:
 *
 *    sun.misc.Unsafe.ARRAY_<T>_BASE_OFFSET
 *      == jdk.internal.misc.Unsafe.ARRAY_<T>_BASE_OFFSET
 *      == theUnsafe.arrayBaseOffset(<T>[].class)
 *
 *  All three are one number by the JDK's own construction: the legacy
 *  spelling's <clinit> COPIES the internal one, which the native computes.
 *
 *  The internal constants are referenced DIRECTLY, not reflectively. A first
 *  version read them with `setAccessible`, which threw
 *  InaccessibleObjectException on CratonVM and succeeded on HotSpot only
 *  because HotSpot was given `--add-opens` -- a probe artefact that read as a
 *  missing field. `InternalFieldProbe` is what separated those two.
 */
public class UnsafeConstAgree {

    static final String[] T = {
        "BOOLEAN", "BYTE", "SHORT", "CHAR", "INT", "LONG", "FLOAT", "DOUBLE", "OBJECT"
    };
    static final Class<?>[] A = {
        boolean[].class, byte[].class, short[].class, char[].class, int[].class,
        long[].class, float[].class, double[].class, Object[].class
    };
    // Direct references -- resolved at compile time, no reflection.
    static final long[] JBASE = {
        Unsafe.ARRAY_BOOLEAN_BASE_OFFSET, Unsafe.ARRAY_BYTE_BASE_OFFSET,
        Unsafe.ARRAY_SHORT_BASE_OFFSET, Unsafe.ARRAY_CHAR_BASE_OFFSET,
        Unsafe.ARRAY_INT_BASE_OFFSET, Unsafe.ARRAY_LONG_BASE_OFFSET,
        Unsafe.ARRAY_FLOAT_BASE_OFFSET, Unsafe.ARRAY_DOUBLE_BASE_OFFSET,
        Unsafe.ARRAY_OBJECT_BASE_OFFSET
    };
    static final long[] JSCALE = {
        Unsafe.ARRAY_BOOLEAN_INDEX_SCALE, Unsafe.ARRAY_BYTE_INDEX_SCALE,
        Unsafe.ARRAY_SHORT_INDEX_SCALE, Unsafe.ARRAY_CHAR_INDEX_SCALE,
        Unsafe.ARRAY_INT_INDEX_SCALE, Unsafe.ARRAY_LONG_INDEX_SCALE,
        Unsafe.ARRAY_FLOAT_INDEX_SCALE, Unsafe.ARRAY_DOUBLE_INDEX_SCALE,
        Unsafe.ARRAY_OBJECT_INDEX_SCALE
    };

    static long fld(Class<?> c, String n) {
        try {
            Field f = c.getDeclaredField(n);
            f.setAccessible(true);
            return ((Number) f.get(null)).longValue();
        } catch (Throwable t) {
            return Long.MIN_VALUE;
        }
    }

    static long call(Object recv, String m, Class<?> arg) {
        try {
            Method mm = recv.getClass().getMethod(m, Class.class);
            return ((Number) mm.invoke(recv, arg)).longValue();
        } catch (Throwable t) {
            return Long.MIN_VALUE;
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> su = Class.forName("sun.misc.Unsafe");
        Field ft = su.getDeclaredField("theUnsafe");
        ft.setAccessible(true);
        Object legacy = ft.get(null);

        int disagreements = 0;
        for (int i = 0; i < T.length; i++) {
            long sb = fld(su, "ARRAY_" + T[i] + "_BASE_OFFSET");
            long nb = call(legacy, "arrayBaseOffset", A[i]);
            boolean okB = sb == JBASE[i] && JBASE[i] == nb;
            if (!okB) disagreements++;
            System.out.println("BASE  " + T[i] + " agree |" + okB
                    + "| legacy=" + sb + " internal=" + JBASE[i] + " native=" + nb);

            long ss = fld(su, "ARRAY_" + T[i] + "_INDEX_SCALE");
            long ns = call(legacy, "arrayIndexScale", A[i]);
            boolean okS = ss == JSCALE[i] && JSCALE[i] == ns;
            if (!okS) disagreements++;
            System.out.println("SCALE " + T[i] + " agree |" + okS
                    + "| legacy=" + ss + " internal=" + JSCALE[i] + " native=" + ns);
        }

        long sa = fld(su, "ADDRESS_SIZE");
        long na;
        try {
            na = ((Number) su.getMethod("addressSize").invoke(legacy)).longValue();
        } catch (Throwable t) {
            na = Long.MIN_VALUE;
        }
        boolean okA = sa == na;
        if (!okA) disagreements++;
        System.out.println("ADDRESS_SIZE agree |" + okA + "| const=" + sa + " native=" + na);

        // A base offset must also be USABLE. The documented protocol is
        // `offset = ARRAY_<T>_BASE_OFFSET + index * ARRAY_<T>_INDEX_SCALE`;
        // this is the access that was silently short by 16.
        byte[] buf = new byte[8];
        buf[2] = (byte) 0x5A;
        long off = fld(su, "ARRAY_BYTE_BASE_OFFSET") + 2L * fld(su, "ARRAY_BYTE_INDEX_SCALE");
        Object got = su.getMethod("getByte", Object.class, long.class).invoke(legacy, buf, off);
        System.out.println("legacy protocol reads the right byte |"
                + (((Byte) got).byteValue() == (byte) 0x5A) + "|");

        System.out.println("total disagreements |" + disagreements + "|");
        System.out.println("DONE UnsafeConstAgree");
    }
}
