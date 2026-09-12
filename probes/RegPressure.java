/**
 * The one gate `c2-the-per-frame-contract-residency-20260912.md` §6a leaves open
 * on `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK`: a body with genuine REGISTER
 * PRESSURE.
 *
 * The worry is specific and mechanical rather than vague. `ir_gp_file()` is five
 * registers deep, and `ir_reserve_carried_enabled`'s pass runs LAST, over
 * whatever the main residency loop left:
 *
 *     let mut free: Vec<u8> = ir_gp_file().iter().copied()
 *         .filter(|r| !taken.contains(r)).collect();
 *
 * The crossblock arm admits more values in that main loop, so it spends the file
 * earlier. On a method where the live set already exceeds five, that could take
 * registers away from LOOP-CARRIED values -- read every iteration -- and give
 * them to single-use ones read once. That is the worst trade available, and
 * every probe measured so far (`FieldLoop`, `FibCall`, `ManyExits`, all under
 * 1.3 KB) has enough slack that it never comes up.
 *
 * `mix` is built so it must come up:
 *
 *   * **Six loop-carried accumulators** (`a`..`g`), all live across the whole
 *     loop, against a five-register file. Plus `i`, the receiver and `n`. The
 *     carried set alone over-subscribes the file, so `reserve_carried` has a
 *     real choice to lose.
 *   * **Three cross-block single-use values** (`p`, `q`, `r`) computed at the top
 *     of the body and each consumed in exactly one arm of the branch below --
 *     so all three are `single_use`, none is reachable by `plan_carries`'
 *     one-node window, and all three are exactly what the flag admits.
 *
 * If the flag is safe under pressure, `sumWide`-style throughput is unchanged or
 * better. If the starvation is real, this is the shape that shows it, and the
 * census says so directly: `carried_reserved` falls while `single_use` falls
 * with it.
 *
 * `probe.acc` selects the accumulator count so the file can be over-subscribed
 * by a little or a lot from one binary; the checksum folds every accumulator so
 * none can be dropped.
 */
public class RegPressure {

    int f1 = 3, f2 = 5, f3 = 7, f4 = 11, f5 = 13, f6 = 17;

    long mix(int n) {
        long a = 0, b = 0, c = 0, d = 0, e = 0, g = 0;
        for (int i = 0; i < n; i++) {
            // Cross-block, single-use: defined here, each read in ONE arm only.
            int p = i * 3 + f1;
            int q = i * 5 + f2;
            int r = i * 7 + f3;
            if ((i & 1) == 0) {
                a += p;
                b += f4;
            } else {
                c += q;
                d += r;
            }
            e += f5;
            g += f6;
        }
        return a + b * 3 + c * 5 + d * 7 + e * 11 + g * 13;
    }

    /** Same loop, three accumulators: the file is tight but not over-subscribed. */
    long mixNarrow(int n) {
        long a = 0, c = 0, e = 0;
        for (int i = 0; i < n; i++) {
            int p = i * 3 + f1;
            int q = i * 5 + f2;
            if ((i & 1) == 0) {
                a += p;
            } else {
                c += q;
            }
            e += f5;
        }
        return a + c * 5 + e * 11;
    }

    public static void main(String[] args) {
        RegPressure probe = new RegPressure();
        int reps = Integer.getInteger("probe.reps", 400);
        int n = Integer.getInteger("probe.n", 20000);
        boolean narrow = Boolean.getBoolean("probe.narrow");

        long acc = 0;
        // Warm the invocation-count door before the clock starts.
        for (int w = 0; w < 60; w++) {
            acc += narrow ? probe.mixNarrow(1000) : probe.mix(1000);
        }

        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            acc += narrow ? probe.mixNarrow(n) : probe.mix(n);
        }
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }
}
