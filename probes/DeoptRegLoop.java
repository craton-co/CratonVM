/**
 * A loop that DEOPTS, with its loop-carried values live.
 *
 * The IR tier's null check is a deopt, not a throw (`DeoptReason::NullCheck`),
 * so a null receiver inside the loop hands the frame back to the interpreter at
 * that bci — and the interpreter has to be told where the loop counter is. With
 * `CRATONVM_JIT_IR_DEOPT_REGS` that answer is a REGISTER, and with
 * `CRATONVM_JIT_IR_DROP_PHI_HOME` the counter's frame word is never written at
 * all, so a wrong answer here is not a slow resume but a wrong number (or a
 * loop that never ends).
 *
 * Every value the resume depends on is folded into the checksum: the counter,
 * the accumulator, and how many times the null was hit.
 */
public class DeoptRegLoop {
    int fx = 7;

    static long run(DeoptRegLoop[] xs, int n) {
        long acc = 0;
        int caught = 0;
        for (int i = 0; i < n; i++) {
            DeoptRegLoop o = xs[i % xs.length];
            try {
                acc += o.fx;
            } catch (NullPointerException e) {
                caught++;
                acc -= 3;
            }
            acc += (i & 1);
        }
        return acc * 1000003L + caught * 31L + n;
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 4000);
        int reps = Integer.getInteger("probe.reps", 3000);
        // One null in five: frequent enough to deopt constantly, rare enough
        // that the compiled body is still the one doing the work.
        DeoptRegLoop[] xs = new DeoptRegLoop[5];
        for (int i = 0; i < xs.length; i++) {
            xs[i] = (i == 3) ? null : new DeoptRegLoop();
        }
        long v = 0;
        for (int r = 0; r < reps; r++) {
            v = run(xs, n);
        }
        System.out.println("deoptreg=" + v);
    }
}
