// Interpreter round i1, lane L7 — a `static synchronized` callee reached from
// hot (compiled / OSR'd) callers on several threads must keep its lock.
//
// What it exercises: `compile_optimizing_artifact`'s direct-bind closure used to
// return a JIT-cache hit BEFORE its synchronized check, so once the background
// tier had published `bump`'s wrapped-entry body, a caller compiled through the
// upgrade/OSR doors could bake a raw CALL to it — no monitor — and increments
// were lost (the `RSyncMethodJit` shape).
//
// Expected output (HotSpot 25, any flags), exactly:
//   counter=4000000
//   perThread=1000000,1000000,1000000,1000000
//   ok
// A lower `counter` is the defect. Run CratonVM with default flags (JIT on);
// `CRATONVM_DBG_JITC=1` shows which doors compiled `work`.
public class L7SyncCalleeFromCompiledCaller {
    static long counter;

    static synchronized void bump() {
        counter++;
    }

    static final class Holder {
        // A second, unsynchronized static callee in the same loop: it must
        // still be direct-bound (the fix must not refuse ordinary statics).
        static long seed = computeSeed();

        static long computeSeed() {
            return 1L;
        }

        static long next(long x) {
            return x + seed;
        }
    }

    static long work(int n) {
        long local = 0;
        for (int i = 0; i < n; i++) {
            bump();
            local = Holder.next(local);
        }
        return local;
    }

    public static void main(String[] args) throws Exception {
        final int threads = 4;
        final int perThread = 1_000_000;
        // Warm the callers on the main thread so they are compiled before the
        // contended phase (and so `bump` gets a published body).
        for (int r = 0; r < 20; r++) {
            work(20_000);
        }
        counter = 0;
        final long[] locals = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> locals[id] = work(perThread));
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("counter=" + counter);
        StringBuilder sb = new StringBuilder("perThread=");
        for (int t = 0; t < threads; t++) {
            if (t > 0) {
                sb.append(',');
            }
            sb.append(locals[t]);
        }
        System.out.println(sb);
        System.out.println(counter == (long) threads * perThread ? "ok" : "LOST-UPDATES");
    }
}
