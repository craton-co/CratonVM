import java.util.concurrent.locks.AbstractOwnableSynchronizer;

/**
 * One question: when compiled code calls
 * `AbstractOwnableSynchronizer.setExclusiveOwnerThread` — a two-line JDK setter
 * that CratonVM also registers a native for, tagged `SyntheticStub` — what
 * actually runs, and what does it cost?
 *
 * `AqsAttributionProbe` prices the rung at ~977 ns per call while
 * `--dump-native-registry` reports 20,899 dispatches for 4,000,000 calls. Those
 * two readings are only compatible two ways: the native runs and the census is
 * blind to the path it runs on, or the bytecode runs and something on the route
 * to it costs 977 ns. This probe separates them by taking the SAME count in
 * both arms — default, and `--nojit CRATONVM_DISABLE_INTRINSICS=1`, the one
 * configuration `NativeMethodRegistry::record_invocation`'s contract calls
 * exact.
 *
 *   N calls, census N  in both arms      -> the native runs, and it is the cost
 *   N calls, census ~0 in the JIT arm    -> the census is blind, ask which path
 *   N calls, census ~0 in BOTH arms      -> the bytecode runs; the cost is the
 *                                           route, not the body
 *
 * `emptyCall` is the scale: the identical two-level call shape with the setter
 * replaced by a plain field store on a class the VM has no registration for.
 * The difference between the two rungs is the whole finding.
 *
 *   javac -d out probes/OwnerCallProbe.java
 *   cratonvm --java-home <jdk> --dump-native-registry owner.json -cp out OwnerCallProbe 2000000
 */
public final class OwnerCallProbe {

    /** Exposes the protected AQS ownership setter, and nothing else. */
    private static final class OwnerOnly extends AbstractOwnableSynchronizer {
        private static final long serialVersionUID = 1L;

        void set(Thread t) {
            setExclusiveOwnerThread(t);
        }
    }

    /** The same two-level shape, on a class with no registered native. */
    private static final class PlainOwner {
        Thread owner;

        void store(Thread t) {
            set0(t);
        }

        private void set0(Thread t) {
            this.owner = t;
        }
    }

    private static long sink;

    private static long rOwner(OwnerOnly s, Thread self, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            s.set(self);
            s.set(null);
        }
        return System.nanoTime() - t0;
    }

    private static long rPlain(PlainOwner p, Thread self, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            p.store(self);
            p.store(null);
        }
        sink += p.owner == null ? 0 : 1;
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        OwnerOnly s = new OwnerOnly();
        PlainOwner p = new PlainOwner();
        Thread self = Thread.currentThread();
        for (int pass = 0; pass < passes; pass++) {
            long plain = rPlain(p, self, n);
            long owner = rOwner(s, self, n);
            System.out.printf("pass %d: plain=%8.2f ns/call   setExclusiveOwnerThread=%8.2f ns/call%n",
                    pass, plain / (double) (2 * n), owner / (double) (2 * n));
        }
        System.out.println("calls per arm = " + (2L * n * passes) + " sink=" + (sink == 0 ? 0 : 1));
    }
}
