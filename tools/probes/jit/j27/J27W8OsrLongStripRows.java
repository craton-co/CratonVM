/**
 * j27 wave 8 (lane W8OSR): fact rows for the OSR-form strip mine of a LONG
 * counted loop (CRATONVM_JIT_IR_STRIP_MINE=1 CRATONVM_JIT_IR_STRIP_MINE_OSR=1
 * CRATONVM_JIT_IR_STRIP_MINE_LONG=1: the saturating long trip distance in
 * jit/src/ir_optimize/strip_mine/osr.rs), page
 * docs/feature-designs/jit-osr-exit-and-recompile.md.
 *
 * Each row is its own cold method, called once with a loop long enough to be
 * entered through the OSR door: a plain range, a range ending exactly at
 * Long.MAX_VALUE (stride 1 lands on the bound), stride 3 just below it, a
 * range across 2^31 (a distance narrowed to int would be wrong there), a range
 * starting at Long.MIN_VALUE, and an empty range entered with i > n. Every
 * answer is Java long arithmetic, so the expected lines are HotSpot's on any
 * JVM.
 *
 * Expected HotSpot 25 output (computed with Java long wrap-around arithmetic):
 *
 *   fact osrl-plain-0-3000000 8833698474470001505
 *   fact osrl-top-1 8174176532021305185
 *   fact osrl-top-step3 6029846543080957473
 *   fact osrl-cross31 -4995494980812054687
 *   fact osrl-min 8833698474470001505
 *   fact osrl-empty 1
 *
 * (osrl-min equals osrl-plain: each of the 3000000 terms gains Long.MIN_VALUE
 * times an odd power of 31, and an even count of them cancels mod 2^64.)
 *
 * java J27W8OsrLongStripRows               facts only (deterministic)
 * java -Dj27.time=true J27W8OsrLongStripRows  also "time <row> <ns>" lines
 */
public class J27W8OsrLongStripRows {
    static final long LMAX = Long.MAX_VALUE;
    static final long LMIN = Long.MIN_VALUE;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    static long plain(long i0, long n) {
        long a = 1;
        for (long i = i0; i < n; i++) a = a * 31 + i;
        return a;
    }

    static long top1(long i0, long n) {
        long a = 1;
        for (long i = i0; i < n; i++) a = a * 31 + i;
        return a;
    }

    static long topStep3(long i0, long n) {
        long a = 1;
        for (long i = i0; i < n; i += 3) a = a * 31 + i;
        return a;
    }

    static long cross31(long i0, long n) {
        long a = 1;
        for (long i = i0; i < n; i++) a = a * 31 + i;
        return a;
    }

    static long fromMin(long i0, long n) {
        long a = 1;
        for (long i = i0; i < n; i++) a = a * 31 + i;
        return a;
    }

    static long empty(long i0, long n) {
        long a = 1;
        for (long i = i0; i < n; i++) a = a * 31 + i;
        return a;
    }

    interface Row {
        long run();
    }

    static void row(String name, Row r) {
        long t0 = System.nanoTime();
        long v = r.run();
        long t1 = System.nanoTime();
        System.out.println("fact " + name + " " + v);
        if (TIME) {
            System.out.println("time " + name + " " + (t1 - t0));
        }
    }

    public static void main(String[] args) {
        row("osrl-plain-0-3000000", () -> plain(0L, 3_000_000L));
        row("osrl-top-1", () -> top1(LMAX - 3_000_000L, LMAX));
        row("osrl-top-step3", () -> topStep3(LMAX - 9_000_000L, LMAX - 2));
        row("osrl-cross31", () -> cross31(2_147_483_647L - 1_500_000L, 2_147_483_647L + 1_500_000L));
        row("osrl-min", () -> fromMin(LMIN, LMIN + 3_000_000L));
        row("osrl-empty", () -> empty(50L, 10L));
    }
}
