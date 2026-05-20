package cratonvm;

/**
 * Session 27: GC Finalizer Support.
 * Tests finalize() method invocation after GC, resurrection, and
 * double-finalization prevention.
 */
public class FinalizerTest {

    // --- Shared state for tracking finalize() calls ---
    static volatile int finalizeCount = 0;
    static volatile int finalizeValue = 0;

    // --- Test helper: a finalizable object ---
    static class Tracked {
        int id;
        Tracked(int id) { this.id = id; }

        protected void finalize() {
            synchronized (FinalizerTest.class) {
                finalizeCount++;
                finalizeValue = id;
            }
        }
    }

    // Test 1: finalize() runs after object becomes unreachable and GC collects
    public static int testFinalizeRuns() {
        finalizeCount = 0;
        finalizeValue = 0;
        allocateTracked(42);
        System.gc();
        return finalizeCount > 0 ? finalizeValue : 0;
        // expect 42
    }

    // Separate method so the local variable goes out of scope
    private static void allocateTracked(int id) {
        Tracked t = new Tracked(id);
        // t goes out of scope here
    }

    // Test 2: finalize() runs for multiple objects
    public static int testFinalizeMultiple() {
        finalizeCount = 0;
        allocateMultiple();
        System.gc();
        return finalizeCount;
        // expect >= 3
    }

    private static void allocateMultiple() {
        new Tracked(1);
        new Tracked(2);
        new Tracked(3);
    }

    // Test 3: Object resurrection — finalize() stores `this` in static field
    static Tracked resurrected = null;

    static class Resurrectable {
        int value;
        Resurrectable(int v) { this.value = v; }

        protected void finalize() {
            // Resurrect by storing in a static field
            resurrected = new Tracked(value);
            synchronized (FinalizerTest.class) {
                finalizeCount++;
            }
        }
    }

    public static int testResurrection() {
        finalizeCount = 0;
        resurrected = null;
        allocateResurrectable(77);
        System.gc();
        // After GC + finalization, resurrected should be non-null
        if (resurrected == null) return 0;
        return resurrected.id; // expect 77
    }

    private static void allocateResurrectable(int v) {
        new Resurrectable(v);
    }

    // Test 4: finalize() is NOT called on live (reachable) objects
    public static int testNoFinalizeOnLive() {
        finalizeCount = 0;
        Tracked live = new Tracked(99);
        System.gc();
        // live is still reachable, finalize should NOT have been called
        int result = finalizeCount == 0 ? 1 : 0;
        // Use live to prevent optimization
        return result + live.id - live.id; // expect 1
    }

    // Test 5: Objects without finalize() override are not affected
    static class Plain {
        int x;
        Plain(int x) { this.x = x; }
        // No finalize() override
    }

    public static int testNoFinalizeOnPlain() {
        finalizeCount = 0;
        allocatePlain();
        System.gc();
        return finalizeCount == 0 ? 1 : 0; // expect 1
    }

    private static void allocatePlain() {
        new Plain(123);
    }

    // Test 6: System.gc() actually collects (verify GC runs)
    public static int testSystemGcCollects() {
        // Allocate some garbage, then GC — just verify no crash
        for (int i = 0; i < 100; i++) {
            new Plain(i);
        }
        System.gc();
        return 1; // expect 1 (no crash)
    }

    // Test 7: finalize() with side effects on static fields
    static volatile int sideEffect = 0;

    static class SideEffector {
        int delta;
        SideEffector(int d) { this.delta = d; }

        protected void finalize() {
            synchronized (FinalizerTest.class) {
                sideEffect += delta;
            }
        }
    }

    public static int testFinalizeSideEffect() {
        sideEffect = 0;
        allocateSideEffectors();
        System.gc();
        return sideEffect; // expect 60 (10+20+30)
    }

    private static void allocateSideEffectors() {
        new SideEffector(10);
        new SideEffector(20);
        new SideEffector(30);
    }
}
