// Multi-threaded arm of probes/BoxUnboxReloc.java.
//
// The single-threaded probe rules the inline sequence out: 8 runs per arm,
// clean, with relocation (34,629 objects) and the bail edge (19,200 deopts)
// both engaged. The remaining difference from the H2 workload that DOES crash
// is thread count.
//
// Relocation here is stop-the-world, so a thread inside the intrinsic (which
// contains no safepoint poll) cannot be parked mid-sequence. What more threads
// DO change is how many frames the collector has to heal at each relocating
// safepoint -- and the open question on this defect is why a receiver slot
// comes back unhealed.
//
// Read `objects_relocated` and `nullBails` (CRATONVM_GC_STATS=1) before
// believing a clean run.
public class BoxUnboxRelocMT {

    static final int TABLE = 8192;
    static volatile Long[] longs = new Long[TABLE];
    static volatile Integer[] ints = new Integer[TABLE];
    static Object[] sparse = new Object[4096];
    static volatile boolean stop = false;
    static final java.util.concurrent.atomic.AtomicLong bad =
            new java.util.concurrent.atomic.AtomicLong();
    static final java.util.concurrent.atomic.AtomicLong nulls =
            new java.util.concurrent.atomic.AtomicLong();

    static long unboxLong(Long v) {
        return v.longValue();
    }

    static int unboxInt(Integer v) {
        return v.intValue();
    }

    static void refill(int i) {
        sparse[(i * 3) % sparse.length] = new byte[96];
        longs[i] = Long.valueOf(1000000L + i);
        sparse[(i * 5 + 1) % sparse.length] = new byte[96];
        ints[i] = Integer.valueOf(100000 + i);
        sparse[(i * 7 + 2) % sparse.length] = new byte[96];
    }

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 30;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 4;

        long expected = 0;
        for (int i = 0; i < TABLE; i++) {
            refill(i);
            expected += 1000000L + i;
            expected += 100000 + i;
        }
        final long want = expected;

        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            ts[t] = new Thread(() -> {
                while (!stop) {
                    long sum = 0;
                    Long[] ls = longs;
                    Integer[] is = ints;
                    for (int i = 0; i < TABLE; i++) {
                        sum += unboxLong(ls[i]);
                        sum += unboxInt(is[i]);
                    }
                    if (sum != want) {
                        bad.incrementAndGet();
                    }
                    for (int n = 0; n < 32; n++) {
                        try {
                            unboxLong(null);
                        } catch (NullPointerException e1) {
                            nulls.incrementAndGet();
                        }
                        try {
                            unboxInt(null);
                        } catch (NullPointerException e2) {
                            nulls.incrementAndGet();
                        }
                    }
                }
            });
            ts[t].setDaemon(true);
            ts[t].start();
        }

        long deadline = System.currentTimeMillis() + seconds * 1000L;
        int round = 0;
        while (System.currentTimeMillis() < deadline) {
            for (int j = 0; j < sparse.length; j += 3) {
                sparse[j] = null;
            }
            for (int j = 0; j < 2048; j++) {
                sparse[(round * 11 + j) % sparse.length] = new byte[256];
            }
            int base = (round * 512) % TABLE;
            for (int k = 0; k < 512; k++) {
                refill((base + k) % TABLE);
            }
            round++;
        }
        stop = true;
        for (Thread t : ts) {
            t.join(5000);
        }
        System.out.println("BoxUnboxRelocMT threads=" + threads
                + " rounds=" + round + " mismatches=" + bad.get()
                + " nullBails=" + nulls.get());
    }
}
