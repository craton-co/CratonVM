// Interpreter round i1 wave 2, lane L7 -- the three interpreter->compiled call
// doors (execute_jit_call / _decoded / _oneshot) now share ONE body run and
// signal drain (jit_bridge.rs `run_jit_body`). This probe drives every outcome
// that drain classifies, through all three doors, long enough for each callee
// to be compiled:
//
//   * a normal return, including the int-family narrowing of a `boolean`,
//     `byte`, `char` and `short` return and a genuine Long.MIN_VALUE return
//     (the deopt-sentinel collision: must be pushed as a value, never re-run);
//   * implicit NPE / AIOOBE / ArithmeticException raised in the compiled
//     callee and caught by the callee's OWN handler, and the same three
//     escaping to the caller;
//   * a side effect committed before a throw, which must happen exactly once.
//
// Doors: static calls (stack-popping door), instance calls (decoded door),
// and a lambda body invoked through its functional interface (one-shot door).
//
// Expected: the output is identical to HotSpot 25 (`java L7JitDoorOutcomes`),
// every line, including the final checksum. Compare with
// `cratonvm L7JitDoorOutcomes` (default flags) and with CRATONVM_DISABLE_JIT=1.
// Timing is printed to stderr only, so stdout stays deterministic.
//
// Wave 3: this probe caught `sideEffects=240004` with a wrong checksum -- a
// stale deopt flag left by a serviced callee trap made an OSR'd `main` bail at
// a legitimate Long.MIN_VALUE return and replay round 2000, and the direct
// lambda arm re-ran the impl after an implicit exception. See
// docs/internal/fixed-bugs/interpreter-L7-stale-deopt-flag-replays-an-osr-iteration-FIXED-20260923.md.
//
// Wave 4: the wave-3 binary printed `sideEffects=240003` and the same wrong
// checksum -- a third stale-flag writer, `interpreter.rs::execute`'s first-call
// JIT door, which never drained the deopt flag. With default flags this probe
// must now print HotSpot's lines, and `CRATONVM_DBG_DEOPT=1` must print no
// `OSR-exit bail rejected (uncommon trap, no frame)` line for `main`.

import java.util.function.IntUnaryOperator;

public class L7JitDoorOutcomes {
    static int sideEffects;

    // ---- static callees (stack-popping door) ----
    static boolean sIsOdd(int x) { return (x & 1) != 0; }
    static byte sLowByte(int x) { return (byte) x; }
    static char sLowChar(int x) { return (char) x; }
    static short sLowShort(int x) { return (short) x; }
    static long sMinOrValue(int x) { return (x % 3 == 0) ? Long.MIN_VALUE : x; }

    static int sCaughtInside(int[] a, int i, int d) {
        try {
            sideEffects++;
            return a[i] / d;
        } catch (ArrayIndexOutOfBoundsException e) {
            return -1;
        } catch (ArithmeticException e) {
            return -2;
        } catch (NullPointerException e) {
            return -3;
        }
    }

    static int sEscapes(int[] a, int i, int d) {
        sideEffects++;
        return a[i] / d;
    }

    // ---- instance callees (decoded door) ----
    int bias = 7;
    int iCaughtInside(int[] a, int i, int d) {
        try {
            sideEffects++;
            return a[i] / d + bias;
        } catch (ArrayIndexOutOfBoundsException e) {
            return -11;
        } catch (ArithmeticException e) {
            return -12;
        } catch (NullPointerException e) {
            return -13;
        }
    }
    long iMin(int x) { return (x & 7) == 0 ? Long.MIN_VALUE : -x; }

    static long checksum;

    static void mix(long v) { checksum = checksum * 31 + v; }

    public static void main(String[] args) {
        long t0 = System.nanoTime();
        int[] data = {10, 20, 30, 40};
        L7JitDoorOutcomes self = new L7JitDoorOutcomes();
        IntUnaryOperator lam = x -> {
            sideEffects++;
            if (x % 5 == 4) {
                int[] z = null;
                return z[0];            // NPE escaping the lambda body
            }
            return 100 / (x % 5);       // ArithmeticException when x % 5 == 0
        };
        int escapedStatic = 0, escapedLambda = 0;
        for (int round = 0; round < 60_000; round++) {
            int i = round % 6;          // 4 and 5 are out of bounds
            int d = round % 4;          // 0 divides by zero
            int[] arr = (round % 11 == 0) ? null : data;
            mix(sIsOdd(round) ? 1 : 0);
            mix(sLowByte(round * 37));
            mix(sLowChar(-round));
            mix(sLowShort(round * 4099));
            mix(sMinOrValue(round));
            mix(sCaughtInside(arr, i, d));
            try {
                mix(sEscapes(arr, i, d));
            } catch (RuntimeException e) {
                escapedStatic++;
                mix(e.getClass().getSimpleName().length());
            }
            mix(self.iCaughtInside(arr, i, d));
            mix(self.iMin(round));
            try {
                mix(lam.applyAsInt(round));
            } catch (RuntimeException e) {
                escapedLambda++;
                mix(e.getClass().getSimpleName().length());
            }
        }
        System.out.println("sideEffects=" + sideEffects);
        System.out.println("escapedStatic=" + escapedStatic);
        System.out.println("escapedLambda=" + escapedLambda);
        System.out.println("checksum=" + checksum);
        System.err.println("elapsed_ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
