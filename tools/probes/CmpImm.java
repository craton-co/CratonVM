/**
 * The counted loop whose bound is a LITERAL — `CRATONVM_JIT_IR_CMP_IN_PLACE`'s
 * immediate form, and `CRATONVM_JIT_IR_ADD_LEA`'s increment.
 *
 * <p>`LoopCtl.spin` takes its bound as a parameter, so its back edge compares
 * two values and the best the emitter can do is read them where they are. Most
 * Java loops do not look like that: `for (int i = 0; i &lt; 100; i++)` names one
 * value and one constant, and the constant belongs in the instruction. This
 * probe is `LoopCtl.spin` with that one change, so the two are directly
 * comparable — same four independent accumulators, so the body is
 * throughput-bound rather than latency-bound and the loop control is a real
 * fraction of it rather than something hiding under a serial recurrence.
 *
 * <p>Two kernels, because the encoding has two forms and they are not the same
 * size: `wide` compares against 20000, which needs `81 /7 id` (six bytes), and
 * `tight` compares against 100, which fits `83 /7 ib` (three). Both replace the
 * same three instructions.
 *
 * <p>Every kernel also increments `i` by one into a value with a register of
 * its own, which is the `LEA` case: `lea r14d,[rbx+1]` against `mov rax,rbx;
 * add eax,1; mov r14,rax`. `down` is the same shape through `Op::Sub`, where
 * the constant arrives at the same encoder negated.
 */
public class CmpImm {

    /** Bound outside `imm8`: `cmp ebx, 20000` is `81 /7 id`. */
    static int wide() {
        int a = 0, b = 0, c = 0, d = 0;
        for (int i = 0; i < 20000; i++) {
            a ^= i;
            b += i;
            c |= i;
            d -= i;
        }
        return a + b + c + d;
    }

    /** Bound inside `imm8`: `cmp ebx, 100` is `83 /7 ib`. */
    static int tight() {
        int a = 0, b = 0, c = 0, d = 0;
        for (int r = 0; r < 200; r++) {
            for (int i = 0; i < 100; i++) {
                a ^= i;
                b += i;
                c |= i;
                d -= i;
            }
        }
        return a + b + c + d;
    }

    /** Counting down: the same `LEA` encoder, reached through `Op::Sub`. */
    static int down() {
        int a = 0, b = 0, c = 0, d = 0;
        for (int i = 20000; i > 0; i--) {
            a ^= i;
            b += i;
            c |= i;
            d -= i;
        }
        return a + b + c + d;
    }

    private static int run(String kind) {
        if (kind.equals("tight")) {
            return tight();
        }
        if (kind.equals("down")) {
            return down();
        }
        return wide();
    }

    public static void main(String[] x) {
        int reps = Integer.parseInt(x[0]);
        String kind = x.length > 1 ? x[1] : "wide";
        long t = 0;
        for (int w = 0; w < 50; w++) {
            t += run(kind);
        }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 7; r++) {
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) {
                t += run(kind);
            }
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
        }
        // 20000 inner iterations per call in every kernel, so the three are
        // priced in the same unit.
        System.out.println("ns_per_iter=" + (best / (double) (reps * 20000L)) + " checksum=" + t);
    }
}
