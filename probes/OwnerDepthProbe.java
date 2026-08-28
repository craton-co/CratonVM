import java.util.concurrent.locks.AbstractOwnableSynchronizer;

/**
 * The same registered native, from two receivers, costs 2.6x apart. Which
 * property of the receiver is it?
 *
 * Measured on this branch, both site-cached (`CRATONVM_DBG=intrinsic-stats`
 * reports 2.00 site-cached dispatches per lock pair, so the per-call-site
 * native cache is serving BOTH):
 *
 *   probes/AqsAttributionProbe `setExclusiveOwnerThread x2`   ~380 ns  (~190/call)
 *     receiver: a direct subclass of AbstractOwnableSynchronizer
 *   probes/PortedLockProbe REAL minus PORT                   ~1100 ns (~500/call)
 *     receiver: ReentrantLock$NonfairSync — three levels down
 *
 * and for scale, two OTHER site-cached natives on the same branch:
 * `AtomicInteger.set(I)V` and `.compareAndSet(II)Z` are ~190 ns each, and
 * `AtomicReferenceArray.lazySet` — which also stores a REFERENCE, through
 * `NativeContext::set_array_element` — is 183 ns. So "a reference store from a
 * native is expensive" does not explain it either.
 *
 * The obvious remaining variable is hierarchy DEPTH: the native memoizes the
 * `exclusiveOwnerThread` field index per receiver class id, and the site cache
 * resolves the owning class by walking supers. Both are supposed to be
 * fill-time costs. This probe asks whether they are.
 *
 * Four receivers, identical in every way except how many classes stand between
 * them and `AbstractOwnableSynchronizer`, each with its OWN loop method so no
 * call site goes bimorphic.
 *
 *   D1 == the AqsAttributionProbe shape
 *   D4 == the ReentrantLock$NonfairSync shape (three intermediates)
 *
 *   D4 ~= D1  ->  depth is not the variable; the difference is a property of
 *                 the AQS call SITE, not of the receiver, and this rung is a
 *                 dead end that should be recorded as one.
 *   D4 >> D1  ->  a per-call cost scales with hierarchy depth, and something
 *                 that was supposed to be memoized at fill time is not.
 *
 *   javac -d out probes/OwnerDepthProbe.java
 *   java      -cp out OwnerDepthProbe    # control
 *   cratonvm --java-home $JDK -cp out OwnerDepthProbe
 */
public final class OwnerDepthProbe {

    private static final int PASSES = 4;
    private static final int ROUNDS = 1_000_000;

    private static long sink;

    static class D1 extends AbstractOwnableSynchronizer {
        private static final long serialVersionUID = 1L;
        final void set(Thread t) { setExclusiveOwnerThread(t); }
    }

    static class D2 extends D1 { private static final long serialVersionUID = 1L; }

    static class D3 extends D2 { private static final long serialVersionUID = 1L; }

    static final class D4 extends D3 { private static final long serialVersionUID = 1L; }

    /**
     * Depth 1 with its own `set` body, so the CALL SITE inside `set` sees only
     * this class — the shape `AqsAttributionProbe` measures.
     */
    static final class Shallow extends AbstractOwnableSynchronizer {
        private static final long serialVersionUID = 1L;
        final void set(Thread t) { setExclusiveOwnerThread(t); }
    }

    private static long rControl(int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { a += i; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rShallow(Shallow s, Thread self, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { s.set(self); s.set(null); }
        return System.nanoTime() - t0;
    }

    private static long rDeep(D4 s, Thread self, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { s.set(self); s.set(null); }
        return System.nanoTime() - t0;
    }

    private interface Rung { long run(int n); }

    private static void pass(String label, Rung r) {
        System.out.printf("%-30s", label);
        for (int p = 0; p < PASSES; p++) {
            System.out.printf("%11.1f", r.run(ROUNDS) / (double) (2 * ROUNDS));
        }
        System.out.println();
    }

    public static void main(String[] args) {
        Shallow shallow = new Shallow();
        D4 deep = new D4();
        Thread self = Thread.currentThread();
        System.out.printf("%-30s%11s%11s%11s%11s   (ns per CALL)%n", "rung", "1", "2", "3", "4");
        pass("control: empty loop", n -> rControl(n) / 2);
        pass("setOwner, depth 1", n -> rShallow(shallow, self, n));
        pass("setOwner, depth 4", n -> rDeep(deep, self, n));
        System.out.println("sink=" + (sink == 0 ? 1 : 0));
    }
}
