/**
 * The shape the OSR door exists for and the optimizing tier has never seen: a
 * method entered ONCE, whose whole cost is a loop. No second invocation, so the
 * method-entry door never fires and a back edge is the only route to compiled
 * code. `CratonBench`'s `arithmetic` phase in miniature — the census that
 * motivated this wiring reported `0 offered / 0 admitted / 0 lowered` for it.
 *
 * **The kernel is its own method, and that is not tidiness.** Two things in a
 * `main` disqualify it from the optimizing tier outright, and an earlier
 * version of this probe had both: `"x=" + v` compiles to an `invokedynamic
 * makeConcatWithConstants` (`ir_compatible` refuses any method containing one),
 * and `Integer.getInteger(...).intValue()` is a call-site intrinsic the IR tier
 * cannot emit. A probe that reads its own parameters and prints its own answer
 * disqualifies the very method it exists to get compiled — and it reads as "the
 * door is not wired" rather than as a broken probe.
 *
 * `kernel` calls nothing. The checksum mixes both loop-carried values in an
 * order-sensitive way, so an entry that seeds them wrongly — or seeds one and
 * not the other — cannot produce it by accident.
 */
public class OsrTierProbe {
    static long kernel(int n) {
        long sum = 0;
        int acc = 1;
        for (int i = 0; i < n; i++) {
            sum += (i & 0xFF);
            acc = acc * 31 + (i & 7);
        }
        return sum * 1000003L + acc;
    }

    public static void main(String[] args) {
        // Scaled off `args.length` rather than a system property: reading one
        // needs `Integer.getInteger`, and this method is on the path to the
        // kernel's compile.
        int n = 4_000_000 * (args.length + 1);
        long ck = kernel(n);
        System.out.print("CK ck=");
        System.out.print(ck);
        System.out.print(" n=");
        System.out.println(n);
    }
}
