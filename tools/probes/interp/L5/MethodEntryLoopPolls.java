// Lane L3 probe (interpreter round i1 wave 15): loops in methods that are
// CALLED many times, so they run in method-entry compiled bodies, closed by a
// `goto` back edge (javac `for`), a fused-constant conditional back edge
// (javac `do { } while`) and a count-down tested with `ifne`.
//
// Wave 15 admits the loop-exit test in METHOD-ENTRY bodies too
// (`METHOD_ENTRY_MODE_EXITS_ENABLED`, jit/src/x64/safepoint.rs): each back-edge
// poll's slow path now tests the helper's verdict (`TEST AL, 2; JNZ`) and the
// conditional back edges record an exit map at the branch. The loops allocate
// so that collections happen while they run and the polls take that slow
// path. With no agent attached the verdict is always 0, so every answer below
// must be HotSpot's; a wrong sum means the slow path or the new maps disturbed
// the loop state.
//
// No setup; run under --compatible with and without --nojit.
//
// HotSpot 25 prints (deterministic):
//   up-to 2497500000
//   do-while 4995000000
//   count-down 2502500000
public class MethodEntryLoopPolls {
    static final int CALLS = 5_000;
    static final int N = 1_000;
    static Object sink;

    static long upTo(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
            if ((i & 15) == 0) {
                sink = new int[64];
            }
        }
        return s;
    }

    static long doWhile(int n) {
        long s = 0;
        int i = 0;
        do {
            s += 2L * i;
            if ((i & 15) == 0) {
                sink = new int[64];
            }
            i++;
        } while (i < n);
        return s;
    }

    static long countDown(int n) {
        long s = 0;
        int i = n;
        do {
            s += i;
            if ((i & 15) == 0) {
                sink = new int[64];
            }
            i--;
        } while (i != 0);
        return s;
    }

    public static void main(String[] args) {
        long a = 0;
        long b = 0;
        long c = 0;
        for (int r = 0; r < CALLS; r++) {
            a += upTo(N);
            b += doWhile(N);
            c += countDown(N);
        }
        System.out.println("up-to " + a);
        System.out.println("do-while " + b);
        System.out.println("count-down " + c);
    }
}
