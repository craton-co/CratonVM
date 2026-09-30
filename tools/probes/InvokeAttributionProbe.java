/**
 * Calibrates what a `--stack-sample-ms` sample at `pc=0 last_pc=0` means.
 *
 * The interpreter's sampling hook sits at the top of the dispatch loop, so a
 * sample is emitted on the first loop iteration after the sampler thread
 * re-arms the request. An `invokevirtual` pushes the callee frame and
 * `continue`s, so the first iteration that can observe the request after an
 * invoke sees the CALLEE at pc=0 with nothing executed. If the invoke
 * operation itself (resolution, argument coercion, frame push) is expensive,
 * its time is therefore reported against the callee's ENTRY, not against any
 * body.
 *
 * This probe makes that testable. `callee()` is one bytecode of work behind an
 * `invokevirtual`, so essentially all of `spin()`'s time is invoke overhead by
 * construction. Sample it:
 *
 *     cratonvm --nojit --stack-sample-ms 100 -cp . InvokeAttributionProbe 40000000
 *
 * and aggregate the leaf frames. If the pc=0 reading is right, the profile is
 * dominated by `InvokeAttributionProbe.callee` at `pc=0 last_pc=0` even though
 * `callee` does almost nothing, and `spin` — where a body-weighted profiler
 * would put the loop — takes the remainder.
 *
 * The printed ns/op is the per-iteration cost (one invokevirtual + one add),
 * which is also the calibration constant for converting an entry-sample share
 * into a per-invoke price on a real workload.
 */
public class InvokeAttributionProbe {

    private int acc;

    int callee(int x) {
        return x + 1;
    }

    long spin(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += callee(i);
        }
        return s;
    }

    /** Same loop, no call: the control that says how much of `spin` is the invoke. */
    long spinNoCall(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i + 1;
        }
        return s;
    }

    /**
     * `args[1]` selects which arm runs, so a native profiler can record ONE of
     * them at a time and the difference between the two profiles is the invoke
     * path. Recording both in one process mixes them and no symbol can be
     * attributed. Default `both` keeps the self-timing form above usable.
     */
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        String mode = args.length > 1 ? args[1] : "both";
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 3;
        InvokeAttributionProbe p = new InvokeAttributionProbe();
        for (int round = 0; round < rounds; round++) {
            long call = 0;
            long plain = 0;
            long a = 0;
            long b = 0;
            // ARM ORDER ALTERNATES. The page's exit criterion asks for the
            // delta "in both arm orders", because whichever arm runs first
            // pays the round's cold caches and whatever the host was doing
            // when the round began; a fixed order charges that to the same
            // arm every time and the delta inherits it.
            boolean callFirst = (round & 1) == 0;
            for (int step = 0; step < 2; step++) {
                boolean doCall = (step == 0) == callFirst;
                if (doCall) {
                    if (mode.equals("nocall")) {
                        continue;
                    }
                    long t0 = System.nanoTime();
                    a = p.spin(n);
                    call = System.nanoTime() - t0;
                } else {
                    if (mode.equals("call")) {
                        continue;
                    }
                    long t0 = System.nanoTime();
                    b = p.spinNoCall(n);
                    plain = System.nanoTime() - t0;
                }
            }
            p.acc += (int) (a ^ b);
            System.out.println("ROUND " + round + " mode=" + mode
                    + " order=" + (callFirst ? "call-first" : "nocall-first")
                    + " withCall_ns=" + (call / n)
                    + " noCall_ns=" + (plain / n)
                    + " invokeDelta_ns=" + ((call - plain) / n)
                    + " sink=" + p.acc);
        }
    }
}
