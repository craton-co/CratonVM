// j27 wave 6, lane T1TIERMGR: rows for the tier-manager book
// (docs/feature-designs/wire-tiered-manager.md).
//
// TIERMGR21-1 (one owner per owed C2 retry, CRATONVM_JIT_ONE_OWED_RETRY=1, opt-in):
//   late-bind-*     a hot caller whose static callee's class is first initialised after
//                   the caller compiled (the late-bind door; EAGER19-3's older publication
//                   with CRATONVM_JIT_LATE_BIND_PENDING_STAMP=1).
//   deferred-new-*  a hot method whose `new` names a class loaded only after it compiled
//                   (the deferred-new door).
// TIERMGR21-4 (accept timing hook, CRATONVM_DBG_ACCEPT_CENSUS=1):
//   accept-leaf     a tiny leaf the evidence gate refuses (no evidence) -- an
//                   `[accept-census] ... gate=refuse-no-evidence` line, and once wave 7
//                   installs the timer, an `[accept-census] timing ...` line.
//
// Census (one DBG run per arm, both modes):
//   A  CRATONVM_DBG_JITC=1 CRATONVM_JIT_LATE_BIND_PENDING_STAMP=1 <bin> J27T1TiermgrRows
//   B  A + CRATONVM_JIT_ONE_OWED_RETRY=1
//   grep -c 'late-bind owed DOUBLE'           (A: debts both owners hold; B: 0)
//   grep -c 'late-bind owed STOOD DOWN'       (B: = A's DOUBLE count)
//   grep -c 'late-bind-owed retry .*answered by the optimizing compile this completion queued'
//   and the optimizing compile count of the two callers:
//   CRATONVM_DBG_JIT_COMPILED=1 ... | grep -cE 'put J27T1TiermgrRows\.(caller|make)'
//   Flip rule: B's facts identical to A's and HotSpot's, B compiles <= A, and the
//   late-bind-post / deferred-new-post timing rows (-Dj27.time=true) are not slower.
//
// Expected output on HotSpot 25 (and every arm, both modes):
//   fact late-bind-pre 105000
//   fact late-bind-post 1349985000
//   fact deferred-new-pre 45000
//   fact deferred-new-post 450045000
//   fact accept-leaf 7499900000
// Timing lines (`time <row> <ns/op>`) only with -Dj27.time=true.
public class J27T1TiermgrRows {
    static final int N = 30_000;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    static final class Late {
        static int f(int i) {
            return i * 3 + 1;
        }
    }

    static final class LateObj {
        final int v;

        LateObj(int v) {
            this.v = v + 2;
        }
    }

    static int caller(int i, boolean late) {
        return late ? Late.f(i) : (i & 7);
    }

    static int make(int i, boolean late) {
        if (late) {
            LateObj o = new LateObj(i);
            return o.v ^ 5;
        }
        return i & 3;
    }

    static int add(int a, int b) {
        return a + b;
    }

    static long callerRow(boolean late) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += caller(i, late);
        }
        return sum;
    }

    static long makeRow(boolean late) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += make(i, late);
        }
        return sum;
    }

    static long leafRow() {
        long sum = 0;
        for (int i = 0; i < 100_000; i++) {
            sum += add(i, i >> 1);
        }
        return sum;
    }

    static void time(String row, java.util.function.LongSupplier body, int ops) {
        if (!TIME) {
            return;
        }
        long best = Long.MAX_VALUE;
        long sink = 0;
        for (int rep = 0; rep < 15; rep++) {
            long t0 = System.nanoTime();
            sink += body.getAsLong();
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("time " + row + " " + (best / ops) + " (sink " + (sink & 1) + ")");
    }

    public static void main(String[] args) throws Exception {
        // Warm the callers on the arm that touches no late class, so they compile first.
        long pre = 0;
        long preNew = 0;
        for (int round = 0; round < 10; round++) {
            pre = callerRow(false);
            preNew = makeRow(false);
        }
        System.out.println("fact late-bind-pre " + pre);
        System.out.println("fact deferred-new-pre " + preNew);
        // Give a background compile time to publish before the late classes load.
        Thread.sleep(200);
        long post = 0;
        long postNew = 0;
        for (int round = 0; round < 10; round++) {
            post = callerRow(true);
            postNew = makeRow(true);
        }
        System.out.println("fact late-bind-post " + post);
        System.out.println("fact deferred-new-post " + postNew);
        long leaf = 0;
        for (int round = 0; round < 10; round++) {
            leaf = leafRow();
        }
        System.out.println("fact accept-leaf " + leaf);
        time("late-bind-post", () -> callerRow(true), N);
        time("deferred-new-post", () -> makeRow(true), N);
        time("accept-leaf", J27T1TiermgrRows::leafRow, 100_000);
    }
}
