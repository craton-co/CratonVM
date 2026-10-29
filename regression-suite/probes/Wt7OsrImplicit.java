// WALKTHROW21-7: implicit exceptions caught inside a hot loop that runs in its OSR body.
// Each kernel is called ONCE with a long trip count, so its loop is entered through OSR.
// `fact` lines are answers; `full` lines carry messages and top frames and are compared
// against HotSpot with -XX:-OmitStackTraceInFastThrow (skip them under a fast-throw arm).
public class Wt7OsrImplicit {
    static final int N = 300_000;
    static int[] ARR = new int[8];
    static Object[] OBJS = new Object[8];
    static int[] NULL_ARR = null;
    static int[][] JAG = new int[16][];
    static final class Box { int v; Box next; }

    static String top(Throwable e) {
        StackTraceElement[] st = e.getStackTrace();
        if (st.length == 0) return "<none>";
        return st[0].getMethodName() + ":" + st[0].getLineNumber() + " d" + st.length;
    }

    static void full(String row, Throwable e) {
        System.out.println("full " + row + " " + e.getClass().getName() + " msg=" + e.getMessage() + " top=" + top(e));
    }

    static long aioobe() {
        long acc = 0; int caught = 0; Throwable first = null, last = null;
        for (int i = 0; i < N; i++) {
            try {
                acc += ARR[i % 11];
            } catch (ArrayIndexOutOfBoundsException e) {
                caught++; acc += i & 3;
                if (first == null) first = e; last = e;
            }
        }
        System.out.println("fact aioobe acc=" + acc + " caught=" + caught);
        full("aioobe-first", first); full("aioobe-last", last);
        return acc;
    }

    static long npeArray() {
        long acc = 0; int caught = 0; Throwable last = null;
        for (int i = 0; i < N; i++) {
            int[] a = (i % 5 == 0) ? NULL_ARR : ARR;
            try {
                acc += a[i & 7];
            } catch (NullPointerException e) {
                caught++; acc -= 3; last = e;
            }
        }
        System.out.println("fact npe-array acc=" + acc + " caught=" + caught);
        full("npe-array-last", last);
        return acc;
    }

    static long div0() {
        long acc = 0; int caught = 0; Throwable last = null;
        for (int i = 0; i < N; i++) {
            try {
                acc += 1000 / (i % 7);
            } catch (ArithmeticException e) {
                caught++; acc += 11; last = e;
            }
        }
        System.out.println("fact div0 acc=" + acc + " caught=" + caught);
        full("div0-last", last);
        return acc;
    }

    static long ldiv0() {
        long acc = 0; int caught = 0;
        for (int i = 0; i < N; i++) {
            try {
                acc += 1_000_000L % (long) (i % 9);
            } catch (ArithmeticException e) {
                caught++; acc ^= i;
            }
        }
        System.out.println("fact ldiv0 acc=" + acc + " caught=" + caught);
        return acc;
    }

    static long fieldNpe() {
        long acc = 0; int caught = 0; Throwable last = null;
        Box b = new Box(); b.v = 3; Box c = b;
        for (int i = 0; i < N; i++) {
            Box x = (i % 6 == 0) ? null : c;
            try {
                acc += x.v;
            } catch (NullPointerException e) {
                caught++; acc += 5; last = e;
            }
        }
        System.out.println("fact field-npe acc=" + acc + " caught=" + caught);
        full("field-npe-last", last);
        return acc;
    }

    static long storeAioobe() {
        long acc = 0; int caught = 0;
        for (int i = 0; i < N; i++) {
            try {
                OBJS[i % 9] = Integer.valueOf(i & 127);
                acc += 1;
            } catch (ArrayIndexOutOfBoundsException e) {
                caught++; acc += 2;
            }
        }
        System.out.println("fact store-aioobe acc=" + acc + " caught=" + caught);
        return acc;
    }

    // The handler mutates loop state and the catch is a superclass.
    static long superCatchState() {
        long acc = 0; int caught = 0; int step = 1;
        for (int i = 0; i < N; i += step) {
            try {
                acc += ARR[(i >> 1) % 10] + step;
                step = 1;
            } catch (RuntimeException e) {
                caught++; step = 2; acc += e instanceof IndexOutOfBoundsException ? 1 : 100;
            }
        }
        System.out.println("fact super-state acc=" + acc + " caught=" + caught + " step=" + step);
        return acc;
    }

    // Nested trys: the inner catches only NPE, the AIOOBE goes to the outer handler.
    static long nested() {
        long acc = 0; int inner = 0, outer = 0;
        for (int i = 0; i < N; i++) {
            try {
                try {
                    int[] a = (i % 13 == 0) ? NULL_ARR : ARR;
                    acc += a[i % 9];
                } catch (NullPointerException e) {
                    inner++; acc += 1;
                }
            } catch (ArrayIndexOutOfBoundsException e) {
                outer++; acc += 2;
            }
        }
        System.out.println("fact nested acc=" + acc + " inner=" + inner + " outer=" + outer);
        return acc;
    }

    // The catch does not take the exception: it must leave the OSR body and reach the caller.
    static long escapes(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += ARR[i % 8] + 1000 / (i % 50_000 == 49_999 ? 0 : 1);
            } catch (ArrayIndexOutOfBoundsException e) {
                acc -= 1;
            }
        }
        return acc;
    }

    // Rethrow from the handler, caught one frame up.
    static long rethrow(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += ARR[i % 10];
            } catch (ArrayIndexOutOfBoundsException e) {
                if (i > n - 3) throw new IllegalStateException("re " + i, e);
                acc += 4;
            }
        }
        return acc;
    }

    // A 2-D access: the outer load may NPE (null row), the inner may AIOOBE.
    static long jagged() {
        for (int k = 0; k < JAG.length; k++) JAG[k] = (k % 4 == 0) ? null : new int[k];
        long acc = 0; int npe = 0, oob = 0;
        for (int i = 0; i < N; i++) {
            try {
                acc += JAG[i & 15][i % 5];
            } catch (NullPointerException e) {
                npe++;
            } catch (ArrayIndexOutOfBoundsException e) {
                oob++;
            }
        }
        System.out.println("fact jagged acc=" + acc + " npe=" + npe + " oob=" + oob);
        return acc;
    }

    // Values live across the throw must survive into the handler.
    static long liveAcross() {
        long acc = 0; int caught = 0;
        for (int i = 0; i < N; i++) {
            int a = i * 3, b = i ^ 0x55;
            long c = (long) i << 7;
            try {
                acc += ARR[i % 9] + a;
            } catch (ArrayIndexOutOfBoundsException e) {
                caught++; acc += a + b + c;
            }
        }
        System.out.println("fact live-across acc=" + acc + " caught=" + caught);
        return acc;
    }

    public static void main(String[] args) {
        for (int k = 0; k < 8; k++) ARR[k] = k * 7 + 1;
        long t = 0;
        t += aioobe();
        t += npeArray();
        t += div0();
        t += ldiv0();
        t += fieldNpe();
        t += storeAioobe();
        t += superCatchState();
        t += nested();
        try {
            t += escapes(N);
            System.out.println("fact escapes none");
        } catch (ArithmeticException e) {
            System.out.println("fact escapes " + e.getMessage());
            full("escapes", e);
        }
        try {
            t += rethrow(N);
            System.out.println("fact rethrow none");
        } catch (IllegalStateException e) {
            System.out.println("fact rethrow " + e.getMessage() + " cause=" + e.getCause().getClass().getName());
            full("rethrow-cause", e.getCause());
        }
        t += jagged();
        t += liveAcross();
        System.out.println("fact total " + t);
    }
}
