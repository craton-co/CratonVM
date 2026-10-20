import java.util.concurrent.atomic.AtomicInteger;

/**
 * The clone of a finalizable object is finalized too, exactly once, as on
 * HotSpot (`JVM_Clone` registers it). gc-common round 2026-09-24,
 * `common-d-finalizable-objects-from-native-allocation-paths-are-never-registered`.
 * Prints `PROBE-OK` when N originals + N clones are each finalized once.
 */
public class CloneFinalizeProbe {
    static final int N = 50;
    static final AtomicInteger FINALIZED = new AtomicInteger();

    static final class Fin implements Cloneable {
        final int id;
        Fin(int id) { this.id = id; }
        @Override public Fin clone() {
            try { return (Fin) super.clone(); } catch (CloneNotSupportedException e) { throw new AssertionError(e); }
        }
        @SuppressWarnings("removal")
        @Override protected void finalize() { FINALIZED.incrementAndGet(); }
    }

    static void make() {
        for (int i = 0; i < N; i++) { new Fin(i).clone(); }
    }

    @SuppressWarnings("removal")
    public static void main(String[] a) throws Exception {
        make();
        for (int round = 0; round < 10 && FINALIZED.get() < 2 * N; round++) {
            System.gc();
            System.runFinalization();
            Thread.sleep(20);
        }
        int got = FINALIZED.get();
        System.out.println("finalized=" + got + " expected=" + (2 * N) + (got == 2 * N ? " PROBE-OK" : " PROBE-FAIL"));
    }
}
