import java.lang.reflect.Array;

/**
 * Ground truth for {@code java.lang.reflect.Array}'s TYPE contract — the half
 * that is not about bounds.
 *
 * The bounds half was closed on 2026-08-06 (see
 * {@code docs/internal/array-index-out-of-bounds-has-no-detail-message-FIXED-20260806.md}).
 * What is left is conversion: {@code Array.getInt} on a {@code long[]} must
 * throw, {@code Array.getLong} on an {@code int[]} must succeed by widening,
 * and a primitive getter on a reference array has its own message again. Five
 * of this VM's registrations share one untyped body, so the *requested* type is
 * not available at the call site at all — which is why the whole matrix has to
 * be measured rather than argued from the two rows a narrower probe showed.
 *
 * Every cell prints class and message. The widening rules are JLS §5.1.2 with
 * {@code char} as the usual exception, but do not take that on trust: the JDK's
 * {@code Array} does its own thing at the edges, and the point of this file is
 * the measurement.
 *
 * Run against HotSpot to regenerate the oracle:
 * {@snippet : java probes/ReflectArrayContractProbe.java }
 */
public class ReflectArrayContractProbe {

    static int n = 0;

    interface Thrower {
        Object run() throws Throwable;
    }

    static void row(String shape, Thrower body) {
        n++;
        try {
            Object value = body.run();
            System.out.println(n + " " + shape + " => OK " + value);
        } catch (Throwable t) {
            System.out.println(n + " " + shape + " => " + t.getClass().getName()
                    + " | " + t.getMessage());
        }
    }

    /** One array of each kind, length 4, so index 0 is always in range. */
    static Object[] arrays() {
        return new Object[] {
            new boolean[4], new byte[4], new char[4], new short[4],
            new int[4], new long[4], new float[4], new double[4],
            new Object[4], new String[4], new int[4][4],
        };
    }

    static String[] arrayNames() {
        return new String[] {
            "boolean[]", "byte[]", "char[]", "short[]",
            "int[]", "long[]", "float[]", "double[]",
            "Object[]", "String[]", "int[][]",
        };
    }

    public static void main(String[] args) {
        Object[] as = arrays();
        String[] names = arrayNames();

        System.out.println("--- getters x array type (index 0, always in range) ---");
        for (int i = 0; i < as.length; i++) {
            final Object a = as[i];
            String t = names[i];
            // The VALUE for a reference array would be an identity hash, which
            // differs per run and per VM by design; name its class instead.
            row("get(" + t + ")        ", () -> {
                Object v = Array.get(a, 0);
                return v == null ? "null" : v.getClass().getName();
            });
            row("getBoolean(" + t + ") ", () -> Array.getBoolean(a, 0));
            row("getByte(" + t + ")    ", () -> Array.getByte(a, 0));
            row("getChar(" + t + ")    ", () -> Array.getChar(a, 0));
            row("getShort(" + t + ")   ", () -> Array.getShort(a, 0));
            row("getInt(" + t + ")     ", () -> Array.getInt(a, 0));
            row("getLong(" + t + ")    ", () -> Array.getLong(a, 0));
            row("getFloat(" + t + ")   ", () -> Array.getFloat(a, 0));
            row("getDouble(" + t + ")  ", () -> Array.getDouble(a, 0));
        }

        System.out.println("--- setters x array type (index 0) ---");
        for (int i = 0; i < as.length; i++) {
            final Object a = as[i];
            String t = names[i];
            row("setBoolean(" + t + ")", () -> { Array.setBoolean(a, 0, true); return "ok"; });
            row("setByte(" + t + ")   ", () -> { Array.setByte(a, 0, (byte) 1); return "ok"; });
            row("setChar(" + t + ")   ", () -> { Array.setChar(a, 0, 'x'); return "ok"; });
            row("setShort(" + t + ")  ", () -> { Array.setShort(a, 0, (short) 1); return "ok"; });
            row("setInt(" + t + ")    ", () -> { Array.setInt(a, 0, 1); return "ok"; });
            row("setLong(" + t + ")   ", () -> { Array.setLong(a, 0, 1L); return "ok"; });
            row("setFloat(" + t + ")  ", () -> { Array.setFloat(a, 0, 1f); return "ok"; });
            row("setDouble(" + t + ") ", () -> { Array.setDouble(a, 0, 1d); return "ok"; });
        }

        System.out.println("--- set(Object) x array type ---");
        Object[] values = { Boolean.TRUE, Byte.valueOf((byte) 1), Character.valueOf('x'),
                Short.valueOf((short) 1), Integer.valueOf(1), Long.valueOf(1L),
                Float.valueOf(1f), Double.valueOf(1d), "str", null, new Object() };
        String[] valueNames = { "Boolean", "Byte", "Character", "Short", "Integer",
                "Long", "Float", "Double", "String", "null", "Object" };
        for (int i = 0; i < as.length; i++) {
            final Object a = as[i];
            for (int v = 0; v < values.length; v++) {
                final Object val = values[v];
                row("set(" + names[i] + "," + valueNames[v] + ")",
                        () -> { Array.set(a, 0, val); return "ok"; });
            }
        }

        System.out.println("--- bounds vs type: which check wins ---");
        // Both wrong at once. The classes differ (AIOOBE vs IAE), so this is
        // control flow, not wording.
        row("getInt(long[4], 9)   ", () -> Array.getInt(new long[4], 9));
        row("getInt(Object[4], 9) ", () -> Array.getInt(new Object[4], 9));
        row("getInt(long[4], -1)  ", () -> Array.getInt(new long[4], -1));
        row("setInt(long[4], 9)   ", () -> { Array.setInt(new long[4], 9, 1); return "ok"; });
        row("set(int[4], 9, \"x\") ", () -> { Array.set(new int[4], 9, "x"); return "ok"; });
        row("getInt(\"hello\", 9)  ", () -> Array.getInt("hello", 9));
        row("getInt(null, 9)      ", () -> Array.getInt(null, 9));
        row("set(null, 9, \"x\")   ", () -> { Array.set(null, 9, "x"); return "ok"; });

        System.out.println("--- non-array and null receivers ---");
        row("get(\"hello\",0)      ", () -> Array.get("hello", 0));
        row("getInt(\"hello\",0)   ", () -> Array.getInt("hello", 0));
        row("setInt(\"hello\",0)   ", () -> { Array.setInt("hello", 0, 1); return "ok"; });
        row("getLength(\"hello\")  ", () -> Array.getLength("hello"));
        row("getLength(null)      ", () -> Array.getLength(null));
        row("get(null,0)          ", () -> Array.get(null, 0));
        row("getBoolean(null,0)   ", () -> Array.getBoolean(null, 0));
        row("setBoolean(null,0)   ", () -> { Array.setBoolean(null, 0, true); return "ok"; });

        System.out.println("--- newInstance ---");
        row("newInstance(int,-1)  ", () -> Array.newInstance(int.class, -1));
        row("newInstance(int,0)   ", () -> Array.getLength(Array.newInstance(int.class, 0)));
        row("newInstance(void,1)  ", () -> Array.newInstance(void.class, 1));
        row("newInstance(null,1)  ", () -> Array.newInstance(null, 1));
        row("newInstance(int,{2,-1})",
                () -> Array.newInstance(int.class, new int[] { 2, -1 }));
        row("newInstance(int,{})  ", () -> Array.newInstance(int.class, new int[] {}));

        System.out.println("--- widening actually converts, not just permits ---");
        // A permissive implementation that returns the raw element would pass a
        // class check and still be wrong. Pin the VALUE.
        row("getLong(int[]{7})    ", () -> {
            int[] a = { 7, 0, 0, 0 };
            return Array.getLong(a, 0);
        });
        row("getDouble(int[]{7})  ", () -> {
            int[] a = { 7, 0, 0, 0 };
            return Array.getDouble(a, 0);
        });
        row("getInt(char[]{'A'})  ", () -> {
            char[] a = { 'A', 0, 0, 0 };
            return Array.getInt(a, 0);
        });
        row("getInt(byte[]{-1})   ", () -> {
            byte[] a = { -1, 0, 0, 0 };
            return Array.getInt(a, 0);
        });
        row("getFloat(long[]{7})  ", () -> {
            long[] a = { 7, 0, 0, 0 };
            return Array.getFloat(a, 0);
        });
        row("get(byte[]{-1})class ", () -> {
            byte[] a = { -1, 0, 0, 0 };
            return Array.get(a, 0).getClass().getName() + "=" + Array.get(a, 0);
        });
        row("get(char[]{'A'})class", () -> {
            char[] a = { 'A', 0, 0, 0 };
            return Array.get(a, 0).getClass().getName() + "=" + Array.get(a, 0);
        });
        row("setInt(long[]) reads ", () -> {
            long[] a = new long[4];
            Array.setInt(a, 0, -5);
            return a[0];
        });
        row("setChar(int[]) reads ", () -> {
            int[] a = new int[4];
            Array.setChar(a, 0, 'A');
            return a[0];
        });
        row("setByte(double[])    ", () -> {
            double[] a = new double[4];
            Array.setByte(a, 0, (byte) -3);
            return a[0];
        });
        row("set(long[],Integer)  ", () -> {
            long[] a = new long[4];
            Array.set(a, 0, Integer.valueOf(9));
            return a[0];
        });

        System.out.println("REFLECT-ARRAY-PROBE-DONE");
    }
}
