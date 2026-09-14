import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * CratonBenchC2 — a CANDIDATE workload for the perf gate, not a gate phase.
 *
 * MEAS-02 measured what the gate actually exercises: CratonBench issues seven
 * compile requests to the optimizing tier across all seven phases and gets two
 * bodies. Every phase is one enormous loop inside one method, so the tier
 * manager — which promotes on INVOCATION COUNT, `c2_threshold` = 20,000 — has
 * almost nothing to promote. The gate measures the single-pass backend.
 *
 * This file is the counter-sample MEAS-02 asks for: same benchmark discipline
 * (deterministic, checksummed, isolated phase per process, seconds not
 * minutes) with a framework-shaped NODE MIX instead of a kernel-shaped one.
 * Framework-shaped means what the ir-coverage survey found real Spring code to
 * be made of, in its own ranking:
 *
 *   getstatic / ldc          static tables and string constants, read hot
 *   invokeinterface          registry -> handler dispatch, several impls
 *   checkcast / instanceof   every typed read out of an untyped container
 *   getfield / putfield      of REFERENCE type, not just int
 *   aaload / arraylength     iterating an array of objects
 *   anewarray                building those arrays per request
 *   athrow                   a validation failure, thrown and caught
 *
 * and — the part the kernels cannot supply at all — MANY SMALL METHODS, each
 * invoked tens of thousands of times, which is the only thing that makes the
 * tier manager ask for C2 in the first place.
 *
 * Usage:
 *   CratonBenchC2            run all three phases in-process
 *   CratonBenchC2 <phase>    run ONE phase (isolated-process methodology)
 *                            phase = dispatch | bind | pipeline
 *
 * KNOWN, and the reason this is not anchored: on the Azure bench host the
 * `dispatch` phase is BIMODAL run to run — the same binary on the same
 * classes measured 0.42 s and 0.65 s, then 8.8 s, then 2.1 s, then 19 s and
 * 30 s, with IDENTICAL compile counts (c1=6 c2=6 osr=1) in the fast and slow
 * modes. It is not driven by the iteration count: 2,000,000 reps measured
 * 2.1 s and 19 s on different runs. Pinning to two CPUs instead of one made
 * some runs fast and left others slow, so the single-core contention between
 * the mutator and the background compiler is at most part of it. The host was
 * at a 1-min load of 13-20 throughout, which is a confound that could not be
 * removed, so this is recorded rather than explained.
 *
 * The REACH and NODE-MIX numbers in the survey are counts and are unaffected
 * by any of that; they reproduced exactly across runs. The timing is what is
 * uncharacterised. See
 * meas-02-bench-suite-c2-reach-RETIRED-20260803.md.
 *
 * Output shape is CratonBench's, exactly: "<n>. <name> : <ms> ms  [<sum>]",
 * so `run-cratonbench-gate.sh` can run this file unchanged if it is ever
 * anchored. It is NOT anchored: see docs/known-issues/c2/ for the policy on
 * what a gate phase costs.
 *
 * Methodology notes, inherited from CratonBench and for the same reasons:
 *   - Every kernel lives in a static method, NOT main: main carries the
 *     invokedynamic string concat, which is an `ir_compatible` whole-method
 *     refusal, and a main-resident loop would be refused with it.
 *   - String concatenation is written through StringBuilder in the hot paths
 *     for the same reason. `+` on a String compiles to invokedynamic on
 *     JDK 9+, and `indy` has no lowering in the optimizing tier — a hot path
 *     built out of it measures the refusal, not the workload.
 *   - The checksum is order- and count-sensitive on purpose: it multiplies by
 *     a prime per contribution, so a handler silently skipped, run twice, or
 *     run in the wrong order changes it. A checksum that only sums is one a
 *     dropped iteration can survive.
 */
public class CratonBenchC2 {

    // ---- the framework ------------------------------------------------
    // getstatic + ldc, read on every request: the survey's largest opcode
    // bucket (189 of 273 events, 69%).
    static final String KEY_ID = "id";
    static final String KEY_NAME = "name";
    static final String KEY_SIZE = "size";
    static final String KEY_FLAG = "flag";
    /** Spelled out rather than built as `KEY_ID + "-absent"`. javac would
     *  fold that (both operands are compile-time constants) and produce the
     *  same `ldc`, but the reader cannot tell folded concatenation from the
     *  invokedynamic kind by looking, and the whole point of this file is a
     *  node mix somebody can verify without `javap`. */
    static final String KEY_ABSENT = "id-absent";
    static final int MIX = 31;
    static final long PRIME = 1_000_003L;

    // Phase sizes. Two constraints, and the second is the one that is easy to
    // break by accident:
    //
    //   1. Seconds, not minutes — the gate's own requirement. Measured on the
    //      Azure bench host: 0.4-0.7 s, 3.3-3.5 s, 2.0-2.6 s. (CratonVM is
    //      70-300x HotSpot on this workload, so the HotSpot times are tens of
    //      milliseconds; size against the slow one.)
    //   2. Every method whose compilation is the POINT of this file must stay
    //      an order of magnitude past `c2_threshold` = 20,000 INVOCATIONS.
    //      That is what makes a workload reach the optimizing tier at all —
    //      not its opcodes — and it is why CratonBench, which is seven
    //      enormous loops inside seven methods, does not.
    //
    // `pipeline` is 40,000 x 16 rather than the more natural 1,000 x 256 for
    // constraint 2 alone: the same 640,000 handler calls either way, but at
    // 1,000 batches `runBatch` itself is invoked a thousand times and never
    // becomes a C2 candidate. Batch COUNT is what makes the per-batch frame
    // hot; batch SIZE only makes each call do more.
    static final int DISPATCH_REQS = 400_000;
    static final int BIND_REQS = 2_000_000;
    static final int PIPELINE_BATCHES = 40_000;
    static final int PIPELINE_BATCH_SIZE = 16;

    /** Dispatch target. Several impls, so the call site is polymorphic. */
    interface Handler {
        long handle(Request r);
        String id();
    }

    /** Reference fields, read AND written — `putfield` of a reference is the
     *  survey's second asymmetry (37 refusals, every reference field store). */
    static final class Request {
        String name;
        Object payload;
        Request next;
        int size;
        boolean flag;

        Request(String name, Object payload, int size, boolean flag) {
            this.name = name;
            this.payload = payload;
            this.size = size;
            this.flag = flag;
        }

        void link(Request other) { this.next = other; }
    }

    static final class ValidationFailure extends RuntimeException {
        final int code;
        ValidationFailure(String message, int code) {
            super(message);
            this.code = code;
        }
        // A framework's own exception type overrides this to skip the stack
        // walk. Keeping it means the throw cost is the walk, not the throw.
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    }

    static final class CountHandler implements Handler {
        public long handle(Request r) { return r.size + (r.flag ? 1 : 0); }
        public String id() { return "count"; }
    }

    static final class NameHandler implements Handler {
        public long handle(Request r) {
            String n = r.name;
            long h = 0;
            for (int i = 0; i < n.length(); i++) {
                h = h * MIX + n.charAt(i);
            }
            return h & 0xffffL;
        }
        public String id() { return "name"; }
    }

    /** instanceof / checkcast: 306 whole-method refusals in the survey, more
     *  than every opcode gap combined, and invisible in the opcode histogram
     *  because the method never reaches the builder. */
    static final class PayloadHandler implements Handler {
        public long handle(Request r) {
            Object p = r.payload;
            if (p instanceof Integer) {
                return ((Integer) p).intValue();
            } else if (p instanceof String) {
                return ((String) p).length() * 7L;
            } else if (p instanceof long[]) {
                long[] a = (long[]) p;
                long s = 0;
                for (int i = 0; i < a.length; i++) s += a[i];
                return s;
            } else if (p instanceof Request) {
                return ((Request) p).size * 3L;
            }
            return 0;
        }
        public String id() { return "payload"; }
    }

    static final class ChainHandler implements Handler {
        public long handle(Request r) {
            long s = 0;
            Request cur = r.next;
            int guard = 0;
            while (cur != null && guard++ < 8) {
                s += cur.size;
                cur = cur.next;
            }
            return s;
        }
        public String id() { return "chain"; }
    }

    /** The container every framework has: untyped in, typed out — so every
     *  read is a checkcast the JIT has to prove or guard. */
    static final class Registry {
        private final Map<String, Object> slots = new HashMap<>();

        void put(String key, Object value) { slots.put(key, value); }

        Handler handler(String key) { return (Handler) slots.get(key); }

        int intValue(String key) {
            Object v = slots.get(key);
            if (v instanceof Integer) return ((Integer) v).intValue();
            throw new ValidationFailure(key, 2);
        }

        String text(String key) {
            Object v = slots.get(key);
            if (v == null) throw new ValidationFailure(key, 1);
            return (String) v;
        }
    }

    // ---- 1. dispatch --------------------------------------------------
    // Interface dispatch over an array of handlers, one request per
    // iteration. Every handler is invoked `reps` times, so at 200,000 reps
    // each is an order of magnitude past `c2_threshold`.

    static Handler[] buildHandlers() {
        Handler[] hs = new Handler[4];          // anewarray
        hs[0] = new CountHandler();
        hs[1] = new NameHandler();
        hs[2] = new PayloadHandler();
        hs[3] = new ChainHandler();
        return hs;
    }

    static long dispatchOne(Handler[] hs, Request r) {
        long acc = 0;
        for (int i = 0; i < hs.length; i++) {   // arraylength + aaload
            acc = acc * MIX + hs[i].handle(r);
        }
        return acc;
    }

    static long benchDispatch(int reps) {
        Handler[] hs = buildHandlers();
        Request tail = new Request("tail", Integer.valueOf(9), 3, false);
        Request mid = new Request("middle", "payload-text", 5, true);
        mid.link(tail);
        long[] wide = new long[] { 11, 22, 33, 44 };
        long check = 0;
        for (int rep = 0; rep < reps; rep++) {
            Object payload;
            int m = rep & 3;
            if (m == 0) payload = Integer.valueOf(rep & 0xff);
            else if (m == 1) payload = "req";
            else if (m == 2) payload = wide;
            else payload = tail;
            Request r = new Request("request", payload, rep & 63, (rep & 1) == 0);
            r.link(mid);                        // putfield of a reference
            check = check * PRIME + dispatchOne(hs, r);
        }
        return check;
    }

    // ---- 2. bind ------------------------------------------------------
    // Property binding: untyped container in, typed values out, with the
    // validation failure a real binder has — thrown and caught per request,
    // not once at the end.

    static Registry buildRegistry() {
        Registry reg = new Registry();
        reg.put(KEY_ID, Integer.valueOf(4242));
        reg.put(KEY_NAME, "craton");
        reg.put(KEY_SIZE, Integer.valueOf(17));
        reg.put(KEY_FLAG, "true");
        return reg;
    }

    static long readOne(Registry reg, int rep) {
        long acc = reg.intValue(KEY_ID) + reg.intValue(KEY_SIZE);
        acc = acc * MIX + reg.text(KEY_NAME).length();
        acc = acc * MIX + (parseBool(reg.text(KEY_FLAG)) ? 1 : 0);
        return acc + rep;
    }

    static boolean parseBool(String s) {
        return s.length() == 4
            && s.charAt(0) == 't' && s.charAt(1) == 'r'
            && s.charAt(2) == 'u' && s.charAt(3) == 'e';
    }

    static long missOne(Registry reg, String key) {
        try {
            return reg.text(key).length();     // never reached for a missing key
        } catch (ValidationFailure e) {
            return e.code;
        }
    }

    static long benchBind(int reps) {
        Registry reg = buildRegistry();
        long check = 0;
        for (int rep = 0; rep < reps; rep++) {
            check = check * PRIME + readOne(reg, rep & 1023);
            // One in eight requests asks for something that is not there.
            // A binder that never fails is not a binder.
            if ((rep & 7) == 0) {
                check = check * MIX + missOne(reg, KEY_ABSENT);
            }
        }
        return check;
    }

    // ---- 3. pipeline --------------------------------------------------
    // A handler chain over a list of small objects — the shape of every
    // filter/interceptor stack — with the per-request StringBuilder work a
    // framework does before it can dispatch.

    static List<Request> buildBatch(int n) {
        List<Request> batch = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            batch.add(new Request(routeName(i), Integer.valueOf(i), i & 31, (i & 3) == 0));
        }
        return batch;
    }

    static String routeName(int i) {
        StringBuilder sb = new StringBuilder(16);
        sb.append("route/").append(i & 15);
        return sb.toString();
    }

    static int routeSlot(String route) {
        int slash = route.indexOf('/');
        if (slash < 0 || slash + 1 >= route.length()) return 0;
        int v = 0;
        for (int i = slash + 1; i < route.length(); i++) {
            v = v * 10 + (route.charAt(i) - '0');
        }
        return v & 3;
    }

    static long runBatch(Handler[] hs, List<Request> batch) {
        long acc = 0;
        for (int i = 0; i < batch.size(); i++) {
            Request r = batch.get(i);           // checkcast out of the List
            Handler h = hs[routeSlot(r.name)];
            acc = acc * MIX + h.handle(r);
        }
        return acc;
    }

    static long benchPipeline(int batches, int batchSize) {
        Handler[] hs = buildHandlers();
        List<Request> batch = buildBatch(batchSize);
        long check = 0;
        for (int b = 0; b < batches; b++) {
            check = check * PRIME + runBatch(hs, batch);
        }
        return check;
    }

    // ---- harness ------------------------------------------------------

    static long report(String label, long t0, long checksum) {
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println(label + ": " + elapsed + " ms  [" + checksum + "]");
        return elapsed;
    }

    static boolean wants(String filter, String phase) {
        return filter == null || filter.equals(phase);
    }

    public static void main(String[] args) {
        String filter = args.length > 0 ? args[0] : null;
        boolean all = filter == null;
        System.out.println("=== CratonBenchC2 (candidate; NOT a gate phase) ===");
        long total = 0;

        if (wants(filter, "dispatch")) {
            long t0 = System.currentTimeMillis();
            long r = benchDispatch(DISPATCH_REQS);
            total += report("1. dispatch    (400K reqs)", t0, r);
            if (all) System.gc();
        }
        if (wants(filter, "bind")) {
            long t0 = System.currentTimeMillis();
            long r = benchBind(BIND_REQS);
            total += report("2. bind        (2M reqs)  ", t0, r);
            if (all) System.gc();
        }
        if (wants(filter, "pipeline")) {
            long t0 = System.currentTimeMillis();
            long r = benchPipeline(PIPELINE_BATCHES, PIPELINE_BATCH_SIZE);
            total += report("3. pipeline    (40Kx16)   ", t0, r);
            if (all) System.gc();
        }
        if (all) {
            System.out.println("TOTAL                     : " + total + " ms");
        }
    }
}
