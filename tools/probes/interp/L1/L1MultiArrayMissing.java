// Interpreter round i1 wave 2, lane L1 — `multianewarray` whose element class
// is missing must throw NoClassDefFoundError, before anything is allocated
// (JVMS §6.5 multianewarray: the class reference is resolved per §5.4.3.1,
// which for an array class resolves its element class). CratonVM used to
// swallow the resolution failure, allocate the arrays with an unknown
// component class, and carry on.
//
// SETUP: `javac L1MultiArrayMissing.java`, then DELETE
// `L1MultiArrayMissing$Gone.class` before running.
//
// Run interpreter-only (CratonVM `--nojit`, HotSpot `-Xint`) and with the JIT
// on (the `loop` row repeats the failing site so a JIT run compiles `full` and
// goes through `jit_multianewarray_n`). Diff stdout against HotSpot 25, which
// prints:
//
//   full    java.lang.NoClassDefFoundError: L1MultiArrayMissing$Gone
//   partial java.lang.NoClassDefFoundError: L1MultiArrayMissing$Gone
//   prim    [[I 2x3
//   loop    java.lang.NoClassDefFoundError: L1MultiArrayMissing$Gone same=19999
public class L1MultiArrayMissing {
    static class Gone {}

    // multianewarray [[LL1MultiArrayMissing$Gone; 2
    static Object full(int a, int b) {
        return new Gone[a][b];
    }

    // multianewarray [[[LL1MultiArrayMissing$Gone; 2 (fewer dims than brackets)
    static Object partial(int a, int b) {
        return new Gone[a][b][];
    }

    static String attempt(int which, int a, int b) {
        try {
            Object o = which == 0 ? full(a, b) : partial(a, b);
            return "allocated " + o.getClass().getName();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        System.out.println("full    " + attempt(0, 2, 2));
        System.out.println("partial " + attempt(1, 2, 3));
        int[][] ok = new int[2][3];
        System.out.println("prim    " + ok.getClass().getName() + " " + ok.length + "x" + ok[0].length);
        String last = null;
        int same = 0;
        for (int i = 0; i < 20000; i++) {
            String r = attempt(0, 1, 1);
            if (r.equals(last)) {
                same++;
            }
            last = r;
        }
        System.out.println("loop    " + last + " same=" + same);
    }
}
