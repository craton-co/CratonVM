import java.lang.reflect.Method;
import java.math.BigInteger;
import java.util.Arrays;

/** Every `@IntrinsicCandidate` private static on `BigInteger` that this VM
 *  registers a native over, called DIRECTLY.
 *
 *  `native-builtins/src/biginteger_intrinsics.rs` registers five of them with
 *  `NativeKind::Intrinsic`. An `Intrinsic` is exempt at every dispatch door and
 *  exempt from the shadow census BY CONSTRUCTION, so none of these five appears
 *  in the `--jdk-only` registry dump as a bridge-over-bytecode row -- and no
 *  probe that goes through the public API can isolate one, because the public
 *  methods are themselves shadowed by a different native. Reflection is the
 *  only instrument that asks these functions and nothing else.
 *
 *  Arrays are rebuilt for every call so no row can inherit another's mutation.
 */
public class L2IntrinsicProbe {
    static int rows = 0;
    static Method LEFT, RIGHT, SQUARE, IMPLMULADD, MULADD;

    static void row(String label, Object v) {
        System.out.println(label + " |" + v + "|");
        rows++;
    }

    static Method find(String name) {
        for (Method m : BigInteger.class.getDeclaredMethods()) {
            if (m.getName().equals(name)) { m.setAccessible(true); return m; }
        }
        return null;
    }

    static void call(String tag, Method m, Object... args) {
        if (m == null) { row(tag + " ABSENT", "-"); return; }
        try {
            Object r = m.invoke(null, args);
            StringBuilder sb = new StringBuilder();
            if (r instanceof int[]) sb.append("ret=").append(Arrays.toString((int[]) r));
            else sb.append("ret=").append(r);
            for (Object o : args) {
                if (o instanceof int[]) sb.append(" arr=").append(Arrays.toString((int[]) o));
            }
            row(tag, sb.toString());
        } catch (Throwable t) {
            Throwable c = t.getCause() == null ? t : t.getCause();
            row(tag + " THREW", c.getClass().getName());
        }
    }

    static int[] m127() { return new int[] {0x7FFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFE}; }
    static int[] mixed() { return new int[] {0x12345678, 0x9ABCDEF0, 0x0F0F0F0F, 0xF0F0F0F0, 0x00000001}; }

    public static void main(String[] a) {
        LEFT = find("shiftLeftImplWorker");
        RIGHT = find("shiftRightImplWorker");
        SQUARE = find("implSquareToLen");
        IMPLMULADD = find("implMulAdd");
        MULADD = find("mulAdd");
        row("found", (LEFT != null) + "," + (RIGHT != null) + "," + (SQUARE != null)
            + "," + (IMPLMULADD != null) + "," + (MULADD != null));

        // --- the two shift workers, at the sizes their JDK callers use ---
        for (int sc : new int[] {1, 4, 7, 15, 31}) {
            call("R m127 n0 sc" + sc, RIGHT, new int[4], m127(), 0, sc, 3);
            call("R m127 n1 sc" + sc, RIGHT, new int[4], m127(), 1, sc, 3);
            call("R mixed n1 sc" + sc, RIGHT, new int[5], mixed(), 1, sc, 4);
            call("L m127 n0 sc" + sc, LEFT, new int[4], m127(), 0, sc, 3);
            call("L mixed n0 sc" + sc, LEFT, new int[5], mixed(), 0, sc, 4);
            call("L mixed n1 sc" + sc, LEFT, new int[5], mixed(), 1, sc, 3);
        }
        for (int it : new int[] {0, 1, 2, 3}) {
            call("R m127 n1 it" + it, RIGHT, new int[4], m127(), 1, 1, it);
            call("L m127 n0 it" + it, LEFT, new int[4], m127(), 0, 1, it);
        }

        // --- implSquareToLen(int[] x, int len, int[] z, int zlen) ---
        call("SQ m127 len4", SQUARE, m127(), 4, new int[8], 8);
        call("SQ mixed len5", SQUARE, mixed(), 5, new int[10], 10);
        call("SQ small", SQUARE, new int[] {0xFFFFFFFF}, 1, new int[2], 2);
        call("SQ two", SQUARE, new int[] {0xFFFFFFFF, 0xFFFFFFFF}, 2, new int[4], 4);
        call("SQ zero", SQUARE, new int[] {0, 0}, 2, new int[4], 4);

        // --- implMulAdd / mulAdd(int[] out, int[] in, int offset, int len, int k) ---
        for (int k : new int[] {1, 2, 0x7FFFFFFF, 0xFFFFFFFF}) {
            call("IMA k" + k, IMPLMULADD, new int[5], mixed(), 1, 4, k);
            call("MA  k" + k, MULADD, new int[5], mixed(), 1, 4, k);
            call("IMA off0 k" + k, IMPLMULADD, new int[5], mixed(), 0, 4, k);
        }
        call("IMA len0", IMPLMULADD, new int[5], mixed(), 1, 0, 7);
        call("IMA carry", IMPLMULADD, new int[] {0xFFFFFFFF, 0}, new int[] {0xFFFFFFFF}, 1, 1, 2);

        System.out.println("rows " + rows);
        System.out.println("DONE L2IntrinsicProbe");
    }
}
