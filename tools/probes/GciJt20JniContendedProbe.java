// gci wave i20, lane jt: T Java threads calling a JNI native in a loop AT THE
// SAME TIME -- the contended price of the in-native bracket
// (docs/known-issues/gc/gci-i19jt-jni-calls-serialise-on-the-barrier-lock-20261014.md).
//
// Every JNI native call goes into native and back (CRATONVM_JNI_NATIVE_TRANSITIONS,
// default ON). The leave took the VM-wide barrier mutex on every call, so T
// threads calling natives serialised on it; HotSpot's transition is two stores
// and a fence, flat in T. This probe reports ns per call per thread for one T;
// the rows compare T = 1, 4, 8 and the arms. Modes:
//   noop  -- the empty native (`noop(I)I`): the bracket alone;
//   peek  -- a fresh int[] per call (young, movable) read by two leaf JNIEnv
//            calls (`arrayPeek([I)I`): the argument must survive a pause that
//            moves it while the thread is in native;
//   str   -- NewStringUTF + GetStringUTFLength + DeleteLocalRef (`newString(I)I`):
//            non-leaf JNIEnv calls, each leaving native and coming back.
// With `churn` a daemon thread allocates continuously, so pauses (young
// collections) run while the callers enter and leave native.
//
//   gcc -shared -fPIC -O1 -I$JDK/include -I$JDK/include/linux -o libgcijt20jni.so tools/probes/jni/GciJt20JniContendedProbe.c
//   java     -XX:+UseSerialGC -Xmx256m -cp <classes> GciJt20JniContendedProbe /abs/libgcijt20jni.so 4 200000 noop
//   cratonvm -Xmx256m -cp <classes> GciJt20JniContendedProbe /abs/libgcijt20jni.so 4 200000 noop churn
//
// Expected stdout (HotSpot 25; deterministic -- every checksum is exact):
//
//   mode noop threads 4 calls 200000: ok
//   PASS
//
// stderr (diagnostics only, never compared):
//   [gci-jt20-contended] mode=<m> threads=<T> churn=<bool> median=<ns>ns/call reps=[...] gcs=<n>
// where a rep's value is the MEDIAN over the T threads of (thread wall time / calls),
// and `median` is the median of the REPS reps (one warm-up rep is not counted).
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;

public final class GciJt20JniContendedProbe {
    static native int noop(int x);

    static native int arrayPeek(int[] a);

    static native int newString(int x);

    static final int REPS = 5;

    static volatile boolean stop;
    static volatile Object sink;

    /** One thread's loop; returns its checksum. */
    static long run(String mode, int calls) {
        long sum = 0;
        switch (mode) {
            case "noop":
                for (int i = 0; i < calls; i++) {
                    sum += noop(i);
                }
                break;
            case "peek":
                for (int i = 0; i < calls; i++) {
                    int[] a = new int[1 + (i & 7)];
                    a[0] = i;
                    sum += arrayPeek(a);
                }
                break;
            case "str":
                for (int i = 0; i < calls; i++) {
                    sum += newString(i);
                }
                break;
            default:
                throw new IllegalArgumentException(mode);
        }
        return sum;
    }

    /** The checksum one thread's loop must produce. */
    static long expected(String mode, int calls) {
        long sum = 0;
        for (int i = 0; i < calls; i++) {
            switch (mode) {
                case "noop":
                    sum += i + 1;
                    break;
                case "peek":
                    sum += 2L * i + 1 + (i & 7);
                    break;
                default:
                    sum += i + 4;
                    break;
            }
        }
        return sum;
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, b.getCollectionCount());
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 4) {
            System.out.println("usage: GciJt20JniContendedProbe <lib> <threads> <calls> noop|peek|str [churn]");
            System.exit(2);
        }
        System.load(args[0]);
        final int threads = Integer.parseInt(args[1]);
        final int calls = Integer.parseInt(args[2]);
        final String mode = args[3];
        boolean churn = args.length > 4 && args[4].equals("churn");
        final long want = expected(mode, calls);

        Thread churner = null;
        if (churn) {
            churner = new Thread(() -> {
                Object[] ring = new Object[256];
                int k = 0;
                while (!stop) {
                    ring[k++ & 255] = new byte[1024];
                }
                sink = ring;
            }, "jt20-churn");
            churner.setDaemon(true);
            churner.start();
        }

        long gc0 = gcCount();
        long[] reps = new long[REPS];
        boolean ok = true;
        for (int r = -1; r < REPS; r++) {
            final long[] ns = new long[threads];
            final long[] sums = new long[threads];
            final CountDownLatch go = new CountDownLatch(1);
            Thread[] workers = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final int me = t;
                workers[t] = new Thread(() -> {
                    try {
                        go.await();
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                    long t0 = System.nanoTime();
                    sums[me] = run(mode, calls);
                    ns[me] = (System.nanoTime() - t0) / calls;
                }, "jt20-caller-" + t);
                workers[t].start();
            }
            go.countDown();
            for (Thread w : workers) {
                w.join();
            }
            for (int t = 0; t < threads; t++) {
                ok &= sums[t] == want;
            }
            if (r >= 0) {
                long[] sorted = ns.clone();
                Arrays.sort(sorted);
                reps[r] = sorted[threads / 2];
            }
        }
        stop = true;
        if (churner != null) {
            churner.join();
        }
        long[] sorted = reps.clone();
        Arrays.sort(sorted);
        System.err.println("[gci-jt20-contended] mode=" + mode + " threads=" + threads + " churn=" + churn
                + " median=" + sorted[REPS / 2] + "ns/call reps=" + Arrays.toString(reps)
                + " gcs=" + (gcCount() - gc0));
        System.out.println("mode " + mode + " threads " + threads + " calls " + calls
                + (ok ? ": ok" : ": FAIL checksum"));
        if (ok) {
            System.out.println("PASS");
        } else {
            System.out.println("FAIL");
            System.exit(1);
        }
    }
}
