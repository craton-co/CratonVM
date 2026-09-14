/**
 * C2CallCostProbe -- how much a SURVIVING call costs in an optimizing-tier body.
 *
 * The callee carries a `tableswitch`, which every inline resolver in the tree
 * refuses (`no!("tableswitch/lookupswitch")`) for both the single-pass and the
 * optimizing tier. So neither tier can inline it, and the arms differ only in
 * which BODY the caller runs -- which is exactly the comparison
 * `CRATONVM_C2_ACCEPT=never` against `=always` is supposed to make.
 *
 * The switch is over a value the loop actually varies, so it cannot be folded;
 * the callee is otherwise as cheap as a callee gets, so the measurement is
 * dominated by the call and not by the callee's work.
 *
 * MEASURED 2026-09-09: ~35 ms single-pass against ~55 ms optimizing, stable.
 * So a surviving call in the caller's OWN code costs the optimizing tier about
 * 5 ns more than the single-pass tier pays -- real, and nowhere near enough to
 * explain SpliceStaticProbe. That is what sent the investigation to the pcs
 * INSIDE spliced bodies, where the cost was a name resolution rather than a
 * call. See SpliceCallProbe.
 *
 * Usage: C2CallCostProbe [reps]     default 4,000,000
 */
public class C2CallCostProbe {
    static int pick(int k) {
        switch (k & 7) {
            case 0: return 3;
            case 1: return 5;
            case 2: return 7;
            case 3: return 11;
            case 4: return 13;
            case 5: return 17;
            case 6: return 19;
            default: return 23;
        }
    }

    static int step(int acc, int i) {
        return acc * 31 + pick(acc ^ i);
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm = step(warm, i);
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(acc, i);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. c2callcost (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
