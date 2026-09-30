/**
 * The shape `c2-the-per-frame-contract-residency-20260912.md` §5 predicts
 * `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` should LOSE on: loop-free, with MORE
 * exits than `FibCall.fib`'s four.
 *
 * The cost model that page arrives at is
 *
 *     cost    = 1 save + N_epilogues restores, paid once per call
 *     benefit = reloads removed, times how often they execute
 *
 * because every register in `ir_gp_file` is callee-saved. `FieldLoop.sum` wins
 * (2 epilogues, benefit multiplied by the trip count) and `FibCall.fib` breaks
 * even (4 epilogues, no loop). Raising the exit count with no loop to repay it
 * should therefore push the same flag negative -- and a cost model that cannot
 * be made to fail is not a cost model.
 *
 * `pick` is built for exactly that: six values computed in the entry block,
 * each read EXACTLY ONCE and each from a different later block, so all six are
 * `single_use` to `plan_register_residency` and none is reachable by
 * `plan_carries`' one-node window. Six returns, so six epilogues. Nothing is
 * loop-carried, so nothing repays the save/restore.
 *
 * The arguments are varied per call so no branch is perfectly predicted and no
 * value is loop-invariant; `acc` is returned through the checksum so the whole
 * thing cannot be folded away.
 */
public class ManyExits {

    static int pick(int a, int b, int c, int d) {
        // Six single-use values, all defined here, all consumed past a branch.
        int p = a * 3 + b;
        int q = b * 5 + c;
        int r = c * 7 + d;
        int s = d * 11 + a;
        int t = a * 13 + c;
        int u = b * 17 + d;
        if ((a & 1) != 0) return p;
        if ((b & 1) != 0) return q;
        if ((c & 1) != 0) return r;
        if ((d & 1) != 0) return s;
        if ((a & 2) != 0) return t;
        return u;
    }

    public static void main(String[] args) {
        int reps = Integer.getInteger("probe.reps", 60);
        int n = Integer.getInteger("probe.n", 200000);

        long acc = 0;
        // Warm the invocation-count door before the clock starts.
        for (int w = 0; w < 200; w++) {
            for (int i = 0; i < 1000; i++) acc += pick(i, i ^ 7, i * 3, i + w);
        }

        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < n; i++) acc += pick(i, i ^ 7, i * 3, i + r);
        }
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }
}
