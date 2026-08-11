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

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        InvokeAttributionProbe p = new InvokeAttributionProbe();
        for (int round = 0; round < 3; round++) {
            long t0 = System.nanoTime();
            long a = p.spin(n);
            long call = System.nanoTime() - t0;

            t0 = System.nanoTime();
            long b = p.spinNoCall(n);
            long plain = System.nanoTime() - t0;

            p.acc += (int) (a ^ b);
            System.out.println("ROUND " + round
                    + " withCall_ns=" + (call / n)
                    + " noCall_ns=" + (plain / n)
                    + " invokeDelta_ns=" + ((call - plain) / n)
                    + " sink=" + p.acc);
        }
    }
}
