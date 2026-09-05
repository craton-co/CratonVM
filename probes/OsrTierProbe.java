/**
 * The shape the OSR door exists for and the optimizing tier has never seen: a
 * method entered ONCE, whose whole cost is a loop. There is no second
 * invocation, so the method-entry door never fires and a back edge is the only
 * route to compiled code.
 *
 * This is `CratonBench`'s `arithmetic` phase in miniature, and the census that
 * motivated the wiring reported `0 offered / 0 admitted / 0 lowered` for it.
 *
 * The checksum is order-sensitive and mixes both loop-carried values, so an
 * entry that seeds them wrongly — or seeds one and not the other — cannot
 * produce it by accident.
 */
public class OsrTierProbe {
    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 40_000_000);
        long sum = 0;
        int acc = 1;
        for (int i = 0; i < n; i++) {
            sum += (i & 0xFF);
            acc = acc * 31 + (i & 7);
        }
        System.out.println("CK sum=" + sum + " acc=" + acc + " n=" + n);
    }
}
