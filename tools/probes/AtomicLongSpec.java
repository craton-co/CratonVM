import java.util.concurrent.atomic.AtomicLong;

/**
 * Why the real JDK's `java.util.Random` returns all zeros on this VM.
 *
 * <p>Retiring the `java/util/Random` native shadow (`CRATONVM_JDK_RANDOM`) hands
 * the class back to the JDK's own bytecode, whose entire state is
 * `private final AtomicLong seed` driven by `next(int)`:
 *
 * <pre>
 *   do {
 *       oldseed = seed.get();
 *       nextseed = (oldseed * multiplier + addend) &amp; mask;
 *   } while (!seed.compareAndSet(oldseed, nextseed));
 * </pre>
 *
 * <p>All-zero output means that loop never advances the seed, which has exactly
 * three candidate causes, and this probe separates them: the constructor does
 * not store, `get()` does not read back, or `compareAndSet` does not write.
 *
 * <p>Every row is a pure function of constants, so diff it against real
 * HotSpot.
 */
public class AtomicLongSpec {
    public static void main(String[] args) {
        // 1. does the ctor store, and does get() read it back?
        AtomicLong a = new AtomicLong(0x5DEECE66DL);
        System.out.println("CK ctor.get            = " + a.get());

        // 2. does set/get round-trip?
        AtomicLong b = new AtomicLong();
        b.set(1234567890123L);
        System.out.println("CK set.get             = " + b.get());

        // 3. does compareAndSet actually write, and report truthfully?
        AtomicLong c = new AtomicLong(10);
        boolean ok = c.compareAndSet(10, 99);
        System.out.println("CK cas.matching.result = " + ok);
        System.out.println("CK cas.matching.value  = " + c.get());

        boolean bad = c.compareAndSet(10, 555); // 10 is stale now -> must fail
        System.out.println("CK cas.stale.result    = " + bad);
        System.out.println("CK cas.stale.value     = " + c.get());

        // 4. the exact loop java.util.Random.next(int) runs.
        AtomicLong seed = new AtomicLong((42L ^ 0x5DEECE66DL) & ((1L << 48) - 1));
        System.out.println("CK random.initialSeed  = " + seed.get());
        long oldseed;
        long nextseed;
        int spins = 0;
        do {
            oldseed = seed.get();
            nextseed = (oldseed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
            spins++;
            if (spins > 100) {
                System.out.println("CK random.casLoop      = SPUN FOREVER");
                break;
            }
        } while (!seed.compareAndSet(oldseed, nextseed));
        System.out.println("CK random.spins        = " + spins);
        System.out.println("CK random.nextSeed     = " + seed.get());
        System.out.println("CK random.next32       = " + (int) (nextseed >>> (48 - 32)));

        // 5. the other accessors Random and friends lean on.
        AtomicLong d = new AtomicLong(5);
        System.out.println("CK getAndSet           = " + d.getAndSet(7) + "," + d.get());
        AtomicLong e = new AtomicLong(5);
        System.out.println("CK getAndAdd           = " + e.getAndAdd(3) + "," + e.get());
        AtomicLong f = new AtomicLong(5);
        System.out.println("CK incrementAndGet     = " + f.incrementAndGet());
    }
}
