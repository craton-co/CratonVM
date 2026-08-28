import java.util.concurrent.atomic.AtomicLong;

/**
 * Differential correctness + throughput for the `AtomicLong` JIT intrinsic.
 *
 * The intrinsic emits a REX.W `LOCK XADD` in place of a registered native
 * dispatch, so it is a machine-code change to 64-bit atomics and the only
 * acceptable evidence is that HotSpot and CratonVM print the SAME checksum.
 * Run both; the last line must match exactly.
 *
 *   javac -d out probes/AtomicLongIntrinsicProbe.java
 *   java      -cp out AtomicLongIntrinsicProbe
 *   cratonvm --java-home $JDK -cp out AtomicLongIntrinsicProbe
 *   CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC=1 cratonvm ... AtomicLongIntrinsicProbe
 *
 * Four things are checked, and each is here because it could break
 * independently:
 *
 *  1. EXACTNESS over edge values. `LOCK XADD` wraps; so does the Java spec, so
 *     `Long.MAX_VALUE + 1` and `Long.MIN_VALUE - 1` must agree. `Long.MIN_VALUE`
 *     is also this VM's exception sentinel on some return paths, which is why
 *     it is in the vector rather than assumed uninteresting.
 *  2. The RECEIVER GUARD. `AtomicLong` is not final; a subclass instance has a
 *     different class id, must MISS the guard, and must deopt to the native —
 *     which has to answer identically. `getAndIncrement` and friends are
 *     `final` in the JDK, so the subclass cannot override them; what is being
 *     proven is that the guard's miss edge is correct, not that an override
 *     wins.
 *  3. ATOMICITY under contention. Four threads, 200 000 increments each: the
 *     final value must be exactly 800 000. A non-atomic emission (a plain
 *     `ADD` instead of `LOCK XADD`) passes 1 and 2 and fails only here.
 *  4. Throughput, printed last and not part of the checksum.
 */
public final class AtomicLongIntrinsicProbe {

    /** A subclass, so its instances carry a different class id. */
    static final class SubLong extends AtomicLong {
        private static final long serialVersionUID = 1L;
        SubLong(long v) { super(v); }
    }

    private static long mix(long acc, long v) {
        return acc * 1000003L + v;
    }

    /** Every accessor, over a vector chosen to cross every boundary. */
    private static long exactness(AtomicLong a) {
        long acc = 0;
        long[] seeds = {
            0L, 1L, -1L, 2L, -2L,
            Long.MAX_VALUE, Long.MIN_VALUE,
            Long.MAX_VALUE - 1, Long.MIN_VALUE + 1,
            0x7FFFFFFFL, 0x80000000L, -0x80000000L,
            0xFFFFFFFFL, 0x100000000L, -1L << 32,
        };
        long[] deltas = { 0L, 1L, -1L, 7L, -7L, Long.MAX_VALUE, Long.MIN_VALUE, 1L << 32 };
        for (long seed : seeds) {
            a.set(seed);
            acc = mix(acc, a.get());
            acc = mix(acc, a.getAndIncrement());
            acc = mix(acc, a.get());
            acc = mix(acc, a.getAndDecrement());
            acc = mix(acc, a.get());
            acc = mix(acc, a.incrementAndGet());
            acc = mix(acc, a.decrementAndGet());
            acc = mix(acc, a.longValue());
            acc = mix(acc, a.intValue());
            for (long d : deltas) {
                a.set(seed);
                acc = mix(acc, a.getAndAdd(d));
                acc = mix(acc, a.get());
                a.set(seed);
                acc = mix(acc, a.addAndGet(d));
                acc = mix(acc, a.get());
            }
        }
        return acc;
    }

    /** Hot loop, so the site is compiled before the checksum is taken again. */
    private static long warm(AtomicLong a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += a.incrementAndGet();
            acc += a.getAndAdd(3);
            acc += a.decrementAndGet();
            acc += a.getAndDecrement();
            acc += a.addAndGet(-2);
            acc += a.get();
        }
        return acc;
    }

    private static long rate(AtomicLong a, int n) {
        long t0 = System.nanoTime();
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += a.incrementAndGet();
            acc += a.decrementAndGet();
        }
        return (System.nanoTime() - t0) / (2L * n) + (acc == Long.MIN_VALUE ? 1 : 0);
    }

    public static void main(String[] args) throws Exception {
        AtomicLong plain = new AtomicLong();
        SubLong sub = new SubLong(0);

        // Cold, then hot, then cold again: the checksum must not depend on
        // whether the site had been compiled when it ran.
        long coldPlain = exactness(plain);
        long coldSub = exactness(sub);
        long w = warm(plain, 200_000) + warm(sub, 50_000);
        long hotPlain = exactness(plain);
        long hotSub = exactness(sub);

        // Atomicity: four threads, 200 000 increments each.
        final AtomicLong shared = new AtomicLong();
        Thread[] ts = new Thread[4];
        for (int i = 0; i < ts.length; i++) {
            ts[i] = new Thread(() -> {
                for (int j = 0; j < 200_000; j++) {
                    shared.incrementAndGet();
                }
            });
        }
        for (Thread t : ts) { t.start(); }
        for (Thread t : ts) { t.join(); }

        System.out.println("coldPlain=" + coldPlain);
        System.out.println("coldSub  =" + coldSub);
        System.out.println("hotPlain =" + hotPlain);
        System.out.println("hotSub   =" + hotSub);
        System.out.println("warmAcc  =" + w);
        System.out.println("contended=" + shared.get() + " (must be 800000)");
        System.out.println("CHECKSUM " + mix(mix(mix(mix(mix(0, coldPlain), coldSub), hotPlain), hotSub), w)
                + " contended=" + shared.get());
        System.out.println("rate inc+dec = " + rate(plain, 2_000_000) + " ns/op");
    }
}
