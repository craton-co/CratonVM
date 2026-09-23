/**
 * Does routing OSR to the optimizing tier actually pay?
 *
 * The kernel is its OWN method for the reason `OsrTierProbe` records: a `main`
 * that reads a system property or concatenates its output disqualifies itself
 * from the optimizing tier (`Integer.getInteger(..).intValue()` is a call-site
 * intrinsic, `"x=" + v` is an invokedynamic), and if the loop lives there the
 * loop is disqualified with it. Here `main` may use both freely — it is not the
 * OSR target, `kernel` is.
 *
 * `kernel` calls nothing and indexes no array, which is what the door's
 * `ir_osr_sentinel_free` admission currently requires: no deopt stub and no
 * call-exception stub, so the body cannot return the sentinel and an entry
 * needs no resume plan. That narrowness is the thing being measured as much as
 * the speed.
 *
 * The recurrence is deliberately serial — each iteration needs the previous
 * `acc` — so nothing can vectorise or close-form it, and the loop cannot be
 * folded away.
 */
public class OsrTierBench {
    static long kernel(int n) {
        long sum = 0;
        int acc = 1;
        for (int i = 0; i < n; i++) {
            acc = acc * 31 + (i & 7);
            sum += (acc & 0xFF);
        }
        return sum * 1000003L + acc;
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 200_000_000);
        long t0 = System.nanoTime();
        long ck = kernel(n);
        long ms = (System.nanoTime() - t0) / 1000000L;
        System.out.print("CK ck=");
        System.out.print(ck);
        System.out.print(" n=");
        System.out.print(n);
        System.out.print(" ms=");
        System.out.println(ms);
    }
}
