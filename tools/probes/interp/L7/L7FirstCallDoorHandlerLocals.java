// Interpreter round i1 wave 5, lane L7 -- `interpreter.rs::execute`'s
// first-call JIT door now shares `jit_bridge::run_jit_body_raw` with the other
// three doors, and routes a throwable that escaped the compiled body into a
// handler that reads a local the body assigned through `run_jit_callee_handler`
// (which resumes on the precise frame the body published) instead of the
// frame-push sink (which resumed the handler on a freshly pushed frame whose
// locals are only the ARGUMENTS, so `acc` below read as 0).
//
// Every `work` call is made through reflection (`Method.invoke`), which enters
// the method through `invoke_method_shared` -> `execute`, i.e. through the
// first-call door, once `work` is compiled. The callee `check` throws on every
// seventh call; `work`'s own handler reads `acc` (assigned before the `try`)
// and `bump` (assigned inside it).
//
// Expected: stdout identical to HotSpot 25 (`java L7FirstCallDoorHandlerLocals`),
// every line, under default flags and with `--nojit`. Before the fix a compiled
// `work` returned `1000 + 0 + 0` from the handler and the checksum differed.
// Timing goes to stderr only.

import java.lang.reflect.Method;

public class L7FirstCallDoorHandlerLocals {
    static int sideEffects;

    static void check(int n) {
        sideEffects++;
        if (n % 7 == 0) {
            throw new IllegalStateException("n=" + n);
        }
    }

    public static int work(int n) {
        int acc = n * 3 + 1;
        int bump = 0;
        try {
            bump = n & 15;
            check(n);
            acc += bump;
        } catch (IllegalStateException e) {
            return 1000 + acc + bump;
        }
        return acc;
    }

    public static long wide(long a, long b) {
        long mix = a ^ (b << 3);
        try {
            check((int) (a & 0x7fffffff));
            mix += 5;
        } catch (IllegalStateException e) {
            return mix - 1;
        }
        return mix;
    }

    public static void main(String[] args) throws Exception {
        long start = System.nanoTime();
        Method work = L7FirstCallDoorHandlerLocals.class.getMethod("work", int.class);
        Method wide = L7FirstCallDoorHandlerLocals.class.getMethod("wide", long.class, long.class);
        long sum = 0;
        long caught = 0;
        for (int round = 0; round < 4; round++) {
            for (int i = 1; i <= 50_000; i++) {
                int r = (Integer) work.invoke(null, i);
                if (i % 7 == 0 && r == 1000 + (i * 3 + 1) + (i & 15)) {
                    caught++;
                }
                sum = sum * 31 + r;
                long w = (Long) wide.invoke(null, (long) i, (long) (i * 7));
                sum = sum * 31 + w;
            }
            System.out.println("round " + round + " sum=" + sum + " caught=" + caught);
        }
        // A direct cross-check against the arithmetic the handler owes.
        int expect7 = 1000 + (7 * 3 + 1) + (7 & 15);
        System.out.println("work(7)=" + work.invoke(null, 7) + " expected=" + expect7);
        System.out.println("sideEffects=" + sideEffects);
        System.err.println("elapsed_ms=" + (System.nanoTime() - start) / 1_000_000);
    }
}
