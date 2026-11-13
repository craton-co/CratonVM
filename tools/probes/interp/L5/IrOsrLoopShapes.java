// Lane L2 probe (interpreter round i1 wave 15): long-running loops of the
// shapes the optimizing (IR) OSR tier compiles, each run twice so the
// optimizing OSR door builds and re-enters its body.
//
// Wave 15 gave that tier's back-edge polls a mode exit
// (`ir_lower::Lowerer::back_edge_mode_exit_state`): in its slow path only, the
// poll tests the helper's verdict and leaves at the loop header when an agent
// needs the frame interpreted. With no agent attached the verdict is always 0,
// so every answer below must be HotSpot's; a wrong value means the new slow
// path, its pad or the deopt stub it adds disturbed the loop state. The
// shapes: a counted sum, a two-local swap (parallel phi copies), a nested
// loop, a loop inside a try block (the exit must not refuse such a body), a
// long + double pair, and a loop carrying a reference.
//
// No setup; run under --compatible with and without --nojit.
//
// HotSpot 25 prints (deterministic, each line twice):
//   sum 4499998500000
//   swap 0 1 1500000
//   nested 1331334000
//   try 9000000000000
//   mixed 9000000 750000.0
//   ref x
public class IrOsrLoopShapes {
    static final int N = 3_000_000;

    static long sum() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += i;
        }
        return s;
    }

    static String swap() {
        int a = 0;
        int b = 1;
        long c = 0;
        for (int i = 0; i < N; i++) {
            int t = a;
            a = b;
            b = t;
            c += a;
        }
        return a + " " + b + " " + c;
    }

    static long nested() {
        long s = 0;
        for (int i = 0; i < 2000; i++) {
            for (int j = 0; j < i; j++) {
                s += j;
            }
        }
        return s;
    }

    static long tryLoop() {
        long s = 0;
        try {
            for (int i = 0; i < N; i++) {
                s += 2L * i + 1;
            }
        } catch (RuntimeException e) {
            s = -1;
        }
        return s;
    }

    static String mixed() {
        long l = 0;
        double d = 0.0;
        for (int i = 0; i < N; i++) {
            l += 3;
            d += 0.25;
        }
        return l + " " + d;
    }

    static Object ref() {
        String s = "x";
        Object o = null;
        for (int i = 0; i < N; i++) {
            if (i == N - 1) {
                o = s;
            }
        }
        return o;
    }

    public static void main(String[] args) {
        for (int round = 0; round < 2; round++) {
            System.out.println("sum " + sum());
            System.out.println("swap " + swap());
            System.out.println("nested " + nested());
            System.out.println("try " + tryLoop());
            System.out.println("mixed " + mixed());
            System.out.println("ref " + ref());
        }
    }
}
