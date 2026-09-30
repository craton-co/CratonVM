/**
 * The timing harness for the FOLDED-operand deferred carry, on the shape the
 * census says it fires on.
 *
 * `kernel` is `OsrTierProbe.kernel`'s arithmetic verbatim -- `i & 0xFF` and
 * `acc * 31`, both of whose second operands are constants, so both arms fold
 * into the instruction and neither reaches `gp_load_value(RCX, ..)`. That is
 * what lets a value scheduled behind one of them survive in RCX across it, and
 * it is why `CRATONVM_JIT_IR_CARRY_RCX_FOLDED` moves this method and not
 * `PollReach.hotLoop` (whose crossing arm is an `I2L`, which
 * `op_preserves_rcx` already covers).
 *
 * The harness is `PollBench`'s: warm, then best-of-7 over `reps`, reported as
 * ns per inner iteration. `ms=` and `acc=` are printed alongside so
 * `tools/tier-ab/flag-ab.sh` can drive it.
 *
 *   FoldCarryBench <reps>          # or -Dprobe.reps=<reps>, which is what
 *                                  # flag-ab.sh can pass
 */
public class FoldCarryBench {
    static long kernel(int n) {
        long sum = 0;
        int acc = 1;
        for (int i = 0; i < n; i++) {
            sum += (i & 0xFF);
            acc = acc * 31 + (i & 7);
        }
        return sum * 1000003L + acc;
    }

    public static void main(String[] a) {
        // 3000 x 20000 = 60M inner iterations per round, ~60 ms at 1 ns each:
        // long enough that a round is not measuring the clock, short enough that
        // seven of them plus the warm-up is under a second.
        int reps = a.length > 0
                ? Integer.parseInt(a[0])
                : Integer.getInteger("probe.reps", 3000);
        int n = 20000;
        long t = 0;
        for (int w = 0; w < 50; w++) {
            t += kernel(n); // warm both doors
        }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 7; r++) {
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) {
                t += kernel(n);
            }
            long d = System.nanoTime() - t0;
            if (d < best) {
                best = d;
            }
        }
        System.out.println("ns_per_iter=" + (best / (double) (reps * (long) n))
                + " ms=" + (best / 1000000L) + " acc=" + t);
    }
}
