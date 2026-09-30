// GcStress — Part F fixture for GPU/GC coordination.
//
// Real Java, real allocation. NO GPU calls. Its job is to allocate
// aggressively from multiple threads so that a separate native worker
// (a future vm/tests integration test) can hold a SafepointToken via
// Heap::enter_gpu_critical while this code thrashes the heap. While the
// token is alive the GC must spin-yield instead of collecting; once the
// token is released the GC must catch up without losing references.
//
// Consumption belongs to a future integration test under vm/tests/ —
// Part F's scope ends at producing the .class file. The Rust side of
// Part F is verified by gc/src/heap.rs's gpu_offload_tests.
//
// Design constraints:
//   - No java.lang.Thread direct subclass is needed; main() spawns
//     worker threads via a Runnable lambda would require invokedynamic,
//     which the analyzer does not (and need not) support. We use the
//     classical Thread + Runnable form so the bytecode is plain
//     invokespecial / invokevirtual that any JVM front-end handles.
//   - Each worker allocates a fresh int[] every iteration; the array
//     escapes briefly into a shared sink so the JIT cannot elide it,
//     then is released so the next GC cycle has something to do.
//   - The driver loop runs for a bounded iteration count rather than a
//     wall-clock deadline so the fixture is deterministic when invoked
//     from a Rust integration test that wants to assert specific
//     counters (e.g. gpu_blocked_gc_count) at the end.
public class GcStress {

    // Volatile sink so the optimiser cannot prove the allocations are dead.
    // A single slot is enough — the previous reference is overwritten on
    // every store, which makes the unreferenced array eligible for GC.
    private static volatile int[] sink;

    // Per-worker iteration count and array length. Tuned small enough to
    // run in well under a second on the dev box but large enough that
    // many GC cycles will fire if the GC is allowed to run. Multiplying
    // workers * iters * arrayLen * 4 gives the total bytes churned.
    public static final int WORKERS = 4;
    public static final int ITERS = 50_000;
    public static final int ARRAY_LEN = 256;

    public static void main(String[] args) throws InterruptedException {
        Thread[] workers = new Thread[WORKERS];
        for (int w = 0; w < WORKERS; w++) {
            workers[w] = new Thread(new Allocator(w));
            workers[w].start();
        }
        for (int w = 0; w < WORKERS; w++) {
            workers[w].join();
        }
        // Print a single machine-parseable line so a Rust integration
        // test can assert the fixture ran to completion without inspecting
        // the JVM internals.
        int last = (sink == null) ? -1 : sink[sink.length - 1];
        System.out.println(
            "GcStress workers=" + WORKERS
            + " iters=" + ITERS
            + " arrayLen=" + ARRAY_LEN
            + " lastTail=" + last
        );
    }

    // Plain Runnable — no lambda, no invokedynamic, no synthetic accessors.
    // The bytecode is intentionally boring so the fixture's behaviour is
    // identical across JDK versions.
    static final class Allocator implements Runnable {
        private final int workerId;

        Allocator(int workerId) {
            this.workerId = workerId;
        }

        @Override
        public void run() {
            for (int i = 0; i < ITERS; i++) {
                int[] arr = new int[ARRAY_LEN];
                // Touch enough slots to defeat any zero-elision pass and to
                // keep the array live until the assignment below.
                arr[0] = workerId;
                arr[ARRAY_LEN - 1] = i;
                // Publish via the volatile sink. Replacing the previous
                // reference is what makes the prior array collectable.
                sink = arr;
            }
        }
    }
}
