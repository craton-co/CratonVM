// Lane L2 timing probe (interpreter round i1 wave 25): the per-call cost of
// the interpreter->compiled method-entry door (`jit_bridge::run_jit_body_raw`)
// after wave 25 added the exceptional-stash floor its no-exception sentinel
// arm needs (`release_locks_of_own_pad_exits`; page
// `interpreter-L2-a-dropped-own-reason-9-frame-leaves-its-compiled-locks-held-for-the-rerun`).
// The floor is one thread-local read per door call for a body that owns a
// point a pad could bake (`body_may_publish_a_pad_frame`), and none for a
// body with no such point.
//
//   leaf       - a trap-free static leaf (no deopt point: no read)
//   guarded    - a static method with an array access (bounds / null guards:
//                the read)
//   syncBlock  - a static method whose call sits inside `synchronized` inside
//                a `try` (a reason-9 pad: the read)
//
// How to run (each row's caller loop must stay interpreted so every call goes
// through the door; the callees compile from their invocation counters):
//
//   CRATONVM_JIT_OSR=0 cratonvm --java-home <jdk25> -cp <dir> L2W25DoorSentinelBench
//
// A/B: interleave against the wave-24 build (`f5cc0e32d`), medians of 3+,
// ns/call on stderr. Expected: `leaf` equal; `guarded` and `syncBlock` equal
// within noise (+1-2 ns at most). A larger step on `leaf` means the predicate
// is not what gates the read.
//
// Stdout is a deterministic checksum per row and must equal HotSpot 25's
// (25.0.3, default and -Xint):
//
//   leaf 200000010000000
//   guarded 20000000
//   syncBlock 200000010000000
public class L2W25DoorSentinelBench {
    static final int N = 20_000_000;
    static final Object LOCK = new Object();
    static final int[] TABLE = {1, 2, 3, 4, 5, 6, 7, 8};
    static int sideEffects;

    static long leaf(long x) {
        return x + 1;
    }

    static int guarded(int i) {
        return TABLE[i & 7] - TABLE[(i + 3) & 7];
    }

    static void bump() {
        sideEffects++;
    }

    static long syncBlock(long x) {
        try {
            synchronized (LOCK) {
                bump();
            }
        } catch (RuntimeException e) {
            return -1;
        }
        return x + 1;
    }

    static long rowLeaf() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += leaf(i);
        }
        return s;
    }

    static long rowGuarded() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += guarded(i) + 1;
        }
        return s;
    }

    static long rowSyncBlock() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += syncBlock(i);
        }
        return s;
    }

    static void time(String name, java.util.function.LongSupplier row) {
        long t0 = System.nanoTime();
        long r = row.getAsLong();
        long t1 = System.nanoTime();
        System.out.println(name + " " + r);
        System.err.printf("%-10s %6.1f ns/call%n", name, (t1 - t0) / (double) N);
    }

    public static void main(String[] args) {
        time("leaf", L2W25DoorSentinelBench::rowLeaf);
        time("guarded", L2W25DoorSentinelBench::rowGuarded);
        time("syncBlock", L2W25DoorSentinelBench::rowSyncBlock);
    }
}
