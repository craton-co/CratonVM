// Lane L4 probe (interpreter round i1 wave 14): loops closed by a CONDITIONAL
// back edge, run long enough to be OSR-compiled mid-loop.
//
// Wave 14 switched the conditional / switch back-edge OSR exit back on
// (`BRANCH_MODE_EXITS_ENABLED`, jit/src/x64/safepoint.rs): an OSR-tier body's
// conditional back-edge poll now records an exit map at the branch and, in its
// slow path only, tests the poll helper's verdict. With no agent attached the
// verdict is always 0, so every answer below must be HotSpot's; a wrong sum
// means the OSR body's slow path or its new exit map disturbed the loop state.
// The shapes: javac's `do { } while` (fused int constant compare), a count-down
// tested with `ifne`, a `long` do-while (fused `lcmp`), and a `double` do-while
// (fused `dcmpg`).
//
// No setup; run under --compatible with and without --nojit.
//
// HotSpot 25 prints (deterministic):
//   do-while 3000000 4499998500000
//   count-down 0 4500001500000
//   long-do-while 9000000 13499995500000
//   double-do-while 2000000 1000000.0
//   stable true
public class ConditionalBackEdgeLoops {
    static final int N = 3_000_000;

    static String doWhile() {
        int i = 0;
        long s = 0;
        do {
            s += i;
            i++;
        } while (i < N);
        return i + " " + s;
    }

    static String countDown() {
        int i = N;
        long s = 0;
        do {
            s += i;
        } while (--i != 0);
        return i + " " + s;
    }

    static String longDoWhile() {
        long j = 0;
        long s = 0;
        do {
            s += j;
            j += 3;
        } while (j < 9_000_000L);
        return j + " " + s;
    }

    static String doubleDoWhile() {
        double d = 0.0;
        int k = 0;
        do {
            d += 0.5;
            k++;
        } while (d < 1_000_000.0);
        return k + " " + d;
    }

    public static void main(String[] args) {
        String[] first = null;
        boolean stable = true;
        for (int round = 0; round < 3; round++) {
            String[] got = {doWhile(), countDown(), longDoWhile(), doubleDoWhile()};
            if (first == null) {
                first = got;
            } else {
                for (int k = 0; k < got.length; k++) {
                    if (!got[k].equals(first[k])) {
                        stable = false;
                        System.err.println("round " + round + " shape " + k + ": " + got[k]);
                    }
                }
            }
        }
        System.out.println("do-while " + first[0]);
        System.out.println("count-down " + first[1]);
        System.out.println("long-do-while " + first[2]);
        System.out.println("double-do-while " + first[3]);
        System.out.println("stable " + stable);
    }
}
