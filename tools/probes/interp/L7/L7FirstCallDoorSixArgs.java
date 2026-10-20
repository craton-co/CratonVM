/*
 * Interpreter round i1, wave 5 (orchestrator): `execute`'s first-call JIT door
 * used to admit at most 4 arguments on Windows (6 elsewhere) while every other
 * door admits `8 - needs_heap`, so a hot 5-7-argument method reached through
 * `invoke_method_shared` (reflection here) ran interpreted. The door now uses
 * the shared limit and encoder. Stdout is deterministic; run it with default
 * flags and with `--nojit`. HotSpot 25 prints exactly:
 *
 *   reflect6=1194955840
 *   direct6=1194955840
 *   reflect7L=1499990001550000
 *   mixed=true
 */
import java.lang.reflect.Method;

public class L7FirstCallDoorSixArgs {
    static int six(int a, int b, int c, int d, int e, int f) {
        return a * 31 + b * 17 + c * 13 + d * 7 + e * 3 + f;
    }

    static long seven(long a, int b, int c, int d, int e, int f, long g) {
        return a + b + c + d + e + f + g;
    }

    static String mixed(Object o, double d, float f, int i, long l, String s) {
        return o + "|" + d + "|" + f + "|" + i + "|" + l + "|" + s;
    }

    public static void main(String[] args) throws Exception {
        Method m6 = L7FirstCallDoorSixArgs.class.getDeclaredMethod(
                "six", int.class, int.class, int.class, int.class, int.class, int.class);
        Method m7 = L7FirstCallDoorSixArgs.class.getDeclaredMethod(
                "seven", long.class, int.class, int.class, int.class, int.class, int.class,
                long.class);
        Method mm = L7FirstCallDoorSixArgs.class.getDeclaredMethod(
                "mixed", Object.class, double.class, float.class, int.class, long.class,
                String.class);
        int n = 200_000;
        int r = 0;
        for (int i = 0; i < n; i++) {
            r += (Integer) m6.invoke(null, i, i + 1, i + 2, i + 3, i + 4, i + 5);
        }
        System.out.println("reflect6=" + r);
        int d = 0;
        for (int i = 0; i < n; i++) {
            d += six(i, i + 1, i + 2, i + 3, i + 4, i + 5);
        }
        System.out.println("direct6=" + d);
        long s = 0;
        for (int i = 0; i < 100_000; i++) {
            s += (Long) m7.invoke(null, (long) i * 100_000L, 1, 2, 3, 4, 5, 10_000_000_000L - i);
        }
        System.out.println("reflect7L=" + s);
        String first = null;
        boolean same = true;
        for (int i = 0; i < 50_000; i++) {
            String v = (String) mm.invoke(null, "o", 1.5d, 2.5f, 7, 9L, "s");
            if (first == null) {
                first = v;
            } else if (!first.equals(v)) {
                same = false;
            }
        }
        System.out.println("mixed=" + (same && first.equals("o|1.5|2.5|7|9|s")));
    }
}
