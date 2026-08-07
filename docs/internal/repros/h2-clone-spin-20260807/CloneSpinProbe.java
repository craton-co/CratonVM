import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReference;

/**
 * The H2 `TransactionStore.registerTransaction` shape, with the database
 * removed: an `AtomicReference` holding an immutable `long[]`-backed bit set
 * that every registration replaces with a CAS'd copy.
 *
 * The copy goes through `BitSetHelper.flip` -> `Arrays.copyOf(long[], int)`,
 * and `Arrays.copyOf` returns `original.clone()` whenever the new length
 * equals the old one -- an `invokevirtual "[J".clone:()Ljava/lang/Object;`
 * with a `long[]` receiver read out of a HEAP FIELD. That single bytecode is
 * the one every recorded occurrence of
 * `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame` runs
 * through, so this drives it directly instead of driving 100 000 H2
 * create/drop cycles to reach it a few thousand times.
 *
 * Usage: CloneSpinProbe <threads> <seconds> [garbageWordsPerIter]
 */
public class CloneSpinProbe {

    /** The `VersionedBitSet` shape: a final `long[]` and a version. */
    static final class Holder {
        final long[] bits;
        final long version;

        Holder() {
            bits = new long[0];
            version = 0;
        }

        Holder(Holder other, int bitToFlip) {
            bits = flip(other.bits, bitToFlip);
            version = other.version + 1;
        }
    }

    /** `org.h2.mvstore.tx.BitSetHelper.flip`, verbatim in shape. */
    static long[] flip(long[] bits, int bitIndex) {
        int wordIndex = bitIndex >> 6;
        int length = bits.length;
        while (--length > wordIndex && bits[length] == 0L) { /**/ }
        bits = Arrays.copyOf(bits, Math.max(length, wordIndex) + 1);
        bits[wordIndex] ^= 1L << bitIndex;
        return bits;
    }

    static final AtomicReference<Holder> REF = new AtomicReference<Holder>(new Holder());
    static volatile boolean stop = false;
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        final int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 120;
        final int garbage = args.length > 2 ? Integer.parseInt(args[2]) : 64;

        final long[] iters = new long[threads];
        final Throwable[] errors = new Throwable[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int idx = t;
            ts[t] = new Thread(new Runnable() {
                public void run() {
                    long n = 0;
                    try {
                        while (!stop) {
                            // The registerTransaction CAS loop: read the
                            // current set, produce the next one, install it.
                            Holder cur = REF.get();
                            int bit = (int) (n % 64);
                            Holder next = new Holder(cur, bit);
                            REF.compareAndSet(cur, next);
                            // Keep the allocator busy so the young generation
                            // turns over while those `long[]`s are live in a
                            // heap field.
                            sink = new Object[garbage];
                            n++;
                        }
                    } catch (Throwable e) {
                        errors[idx] = e;
                    }
                    iters[idx] = n;
                }
            }, "spin-" + t);
        }
        for (Thread th : ts) {
            th.start();
        }
        Thread.sleep(seconds * 1000L);
        stop = true;
        for (Thread th : ts) {
            th.join();
        }
        long total = 0;
        int failed = 0;
        for (int t = 0; t < threads; t++) {
            total += iters[t];
            if (errors[t] != null) {
                failed++;
                System.out.println("THREAD " + t + " FAILED: " + errors[t]);
                errors[t].printStackTrace(System.out);
            }
        }
        System.out.println("threads=" + threads + " seconds=" + seconds
                + " iterations=" + total + " failed=" + failed);
    }
}
