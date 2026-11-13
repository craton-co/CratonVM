// j27 wave 6 (lane T1DISP20): DISP20-8 of
// docs/feature-designs/native-dispatch-architecture.md -- the
// reference-ARRAY element arm of the compiled `Unsafe.compareAndSetReference`
// and `getReferenceVolatile` prefixes (`jit/src/unsafe_ref_array_arm.rs`,
// switch CRATONVM_JIT_UNSAFE_REF_ARRAY_INLINE, opt-in). Real users:
// ConcurrentHashMap.casTabAt / tabAt (putVal, get, merge, computeIfAbsent)
// and AtomicReferenceArray (VarHandleReferences$Array -> Unsafe).
//
// The fact rows are the correctness side: every compiled CAS must stay in the
// natives' atomicity domain (no lost update under a 4-thread race, mixed
// with whatever is still interpreted), keep the identity compare, keep the
// natives' bounds answer, and keep the slot values intact across GCs. Each
// race runs after a warm-up that compiles the hot bodies. No fact depends on
// timing, scheduling or tier.
//
//   java -cp <dir> J27T1Disp20ChmRows                           > hs.txt
//   cratonvm --real-jdk   -cp <dir> J27T1Disp20ChmRows | diff - hs.txt
//   cratonvm --compatible -cp <dir> J27T1Disp20ChmRows | diff - hs.txt
//   (each also with CRATONVM_JIT_UNSAFE_REF_ARRAY_INLINE=1, and once --nojit)
// Timing (separate `time` lines, ns/op, median of 5 reps):
//   ... -Dj27.time=true J27T1Disp20ChmRows
//
// Expected HotSpot 25 output (facts only; worked out from the JDK 25 sources:
// AtomicReferenceArray -> VarHandle `Preconditions.checkIndex(.., AIOOBE_FORMATTER)`,
// ConcurrentHashMap.putVal's explicit `new NullPointerException()`):
//   fact ara-answers c1=true c2=false c3=false c4=true gas=true weak=true last=null
//   fact ara-oob java.lang.ArrayIndexOutOfBoundsException Index 8 out of bounds for length 8
//   fact ara-oob-neg java.lang.ArrayIndexOutOfBoundsException Index -1 out of bounds for length 8
//   fact ara-cas-race 81920
//   fact chm-merge-race size=64 total=81920 uniform=true
//   fact chm-put-race size=81920 keysum=3355402240
//   fact chm-put-get sum=149985000 replace1=true replace2=false remove1=true remove2=false pia=99 missing=null size=9999 nullkey=java.lang.NullPointerException:null
//   fact chm-cia-once calls=256 size=256
//   fact ara-gc 40960
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReferenceArray;

public class J27T1Disp20ChmRows {
    static final int THREADS = 4;
    static final int K = 20480; // divisible by 16, 64 and 256

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        warm();
        araAnswers();
        araCasRace();
        chmMergeRace();
        chmPutRace();
        chmPutGet();
        chmCiaOnce();
        araGc();
        if (Boolean.getBoolean("j27.time")) {
            timing();
        }
    }

    // ---- shared hot bodies (the compiled sites) ----

    static void casIncrement(AtomicReferenceArray<Long> arr, int i) {
        for (;;) {
            Long o = arr.get(i);
            if (arr.compareAndSet(i, o, o + 1)) {
                return;
            }
        }
    }

    static long sum(AtomicReferenceArray<Long> arr) {
        long s = 0;
        for (int i = 0; i < arr.length(); i++) {
            s += arr.get(i);
        }
        return s;
    }

    static AtomicReferenceArray<Long> zeros(int n) {
        AtomicReferenceArray<Long> arr = new AtomicReferenceArray<>(n);
        for (int i = 0; i < n; i++) {
            arr.set(i, 0L);
        }
        return arr;
    }

    static void warm() {
        AtomicReferenceArray<Long> arr = zeros(16);
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        long s = 0;
        for (int n = 0; n < 200_000; n++) {
            casIncrement(arr, n & 15);
            m.put(n & 1023, n);
            Integer v = m.get(n & 511);
            s += v == null ? 0 : v;
            m.merge(n & 63, 1, Integer::sum);
        }
        sink = s + sum(arr);
    }

    static void runThreads(int count, java.util.function.IntConsumer body) throws Exception {
        Thread[] ts = new Thread[count];
        for (int t = 0; t < count; t++) {
            final int id = t;
            ts[t] = new Thread(() -> body.accept(id));
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
    }

    // ---- fact rows ----

    static void araAnswers() {
        AtomicReferenceArray<Object> r = new AtomicReferenceArray<>(8);
        Object x = new Object();
        String s1 = "a";
        String s2 = new String("a");
        boolean c1 = r.compareAndSet(3, null, s1);
        boolean c2 = r.compareAndSet(3, null, x);
        boolean c3 = r.compareAndSet(3, s2, x); // equal, not identical
        boolean c4 = r.compareAndSet(3, s1, x);
        boolean gas = r.getAndSet(3, s2) == x;
        while (!r.weakCompareAndSetVolatile(3, s2, null)) {
            // a weak CAS may fail spuriously; retry
        }
        boolean weak = r.get(3) == null;
        System.out.println("fact ara-answers c1=" + c1 + " c2=" + c2 + " c3=" + c3 + " c4=" + c4
                + " gas=" + gas + " weak=" + weak + " last=" + r.get(3));
        System.out.println("fact ara-oob " + oob(r, 8));
        System.out.println("fact ara-oob-neg " + oob(r, -1));
    }

    static String oob(AtomicReferenceArray<Object> r, int i) {
        try {
            r.compareAndSet(i, null, "z");
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName() + " " + t.getMessage();
        }
    }

    static void araCasRace() throws Exception {
        AtomicReferenceArray<Long> arr = zeros(16);
        runThreads(THREADS, t -> {
            for (int n = 0; n < K; n++) {
                casIncrement(arr, (n + t) & 15);
            }
        });
        System.out.println("fact ara-cas-race " + sum(arr));
    }

    static void chmMergeRace() throws Exception {
        ConcurrentHashMap<Integer, Long> m = new ConcurrentHashMap<>();
        runThreads(THREADS, t -> {
            for (int n = 0; n < K; n++) {
                m.merge(n & 63, 1L, Long::sum);
            }
        });
        long total = 0;
        boolean uniform = true;
        for (long v : m.values()) {
            total += v;
            uniform &= v == (long) THREADS * K / 64;
        }
        System.out.println("fact chm-merge-race size=" + m.size() + " total=" + total
                + " uniform=" + uniform);
    }

    static void chmPutRace() throws Exception {
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        runThreads(THREADS, t -> {
            for (int n = 0; n < K; n++) {
                int key = t * K + n;
                m.put(key, key);
            }
        });
        long keysum = 0;
        for (int k : m.keySet()) {
            keysum += k;
        }
        System.out.println("fact chm-put-race size=" + m.size() + " keysum=" + keysum);
    }

    static void chmPutGet() {
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < 10_000; i++) {
            m.put(i, i * 3);
        }
        long s = 0;
        for (int i = 0; i < 10_000; i++) {
            s += m.get(i);
        }
        boolean replace1 = m.replace(5, 15, 99);
        boolean replace2 = m.replace(5, 15, 1);
        boolean remove1 = m.remove(6, 18);
        boolean remove2 = m.remove(7, 0);
        Integer pia = m.putIfAbsent(5, 0);
        Integer missing = m.get(-1);
        String nullkey;
        try {
            m.put(null, 1);
            nullkey = "no-throw";
        } catch (NullPointerException e) {
            nullkey = e.getClass().getName() + ":" + e.getMessage();
        }
        System.out.println("fact chm-put-get sum=" + s + " replace1=" + replace1 + " replace2=" + replace2
                + " remove1=" + remove1 + " remove2=" + remove2 + " pia=" + pia + " missing=" + missing
                + " size=" + m.size() + " nullkey=" + nullkey);
    }

    static void chmCiaOnce() throws Exception {
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        AtomicInteger calls = new AtomicInteger();
        runThreads(THREADS, t -> {
            for (int n = 0; n < K; n++) {
                m.computeIfAbsent((n + t) & 255, k -> {
                    calls.incrementAndGet();
                    return k;
                });
            }
        });
        System.out.println("fact chm-cia-once calls=" + calls.get() + " size=" + m.size());
    }

    static void araGc() throws Exception {
        AtomicReferenceArray<Long> arr = zeros(16);
        Thread churn = new Thread(() -> {
            for (int g = 0; g < 4; g++) {
                for (int i = 0; i < 20_000; i++) {
                    sink = new byte[512];
                }
                System.gc();
            }
        });
        churn.start();
        runThreads(2, t -> {
            for (int n = 0; n < K; n++) {
                casIncrement(arr, n & 15);
                if ((n & 1023) == 0) {
                    sink = new long[64];
                }
            }
        });
        churn.join();
        System.gc();
        System.out.println("fact ara-gc " + sum(arr));
    }

    // ---- timing rows (-Dj27.time=true) ----

    interface Row {
        long run(int n);
    }

    static void timing() {
        ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < 1024; i++) {
            m.put(i, i);
        }
        AtomicReferenceArray<Long> arr = zeros(16);
        time("chm-put", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                m.put(i & 1023, i);
                s += i;
            }
            return s;
        });
        time("chm-get", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += m.get(i & 1023);
            }
            return s;
        });
        time("ara-cas", n -> {
            for (int i = 0; i < n; i++) {
                casIncrement(arr, i & 15);
            }
            return sum(arr);
        });
        time("ara-get", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += arr.get(i & 15);
            }
            return s;
        });
    }

    static void time(String name, Row row) {
        final int n = 1_000_000;
        row.run(n); // warm
        long[] reps = new long[5];
        for (int r = 0; r < reps.length; r++) {
            long t0 = System.nanoTime();
            sink = row.run(n);
            reps[r] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(reps);
        System.out.println("time " + name + " " + (reps[2] / n) + " ns/op");
    }
}
