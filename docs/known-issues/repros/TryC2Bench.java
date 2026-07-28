/**
 * Isolates what a `try`/`catch` costs on the optimizing tier, on ONE binary,
 * A/B'd against its own pre-change behaviour via
 * `CRATONVM_JIT_NO_EXC_TABLE_C2=1` (which restores the old blanket exclusion of
 * every exception-table-bearing method from the IR pipeline).
 *
 * Both methods are pure `int` arithmetic with no calls, no allocation and no
 * field access, so they are unambiguously `ir_compatible` — nothing but the
 * exception table can decide which tier they land on. That is the whole point:
 * the earlier `LazyIsolate` d8/d9 pair went through `ConcurrentHashMap` and
 * `Charset.forName`, so it could be held off the IR path by an unrelated gate
 * and could not, on its own, prove anything about this change.
 *
 *   withTry    — identical body, wrapped in try/catch. The `idiv` is a genuine
 *                throwing site inside the protected range, so the handler is
 *                real; `(b|1)` is never zero, so it never actually fires and
 *                the measured cost is entirely static.
 *   withoutTry — the same arithmetic, no exception table. The control.
 *
 * A/B expectation: `withoutTry` should be unaffected by the env var (it has no
 * table); `withTry` should match `withoutTry` with the fix on, and be several
 * times slower with the fix off.
 */
public final class TryC2Bench {

    static int withTry(int a, int b) {
        int r = a;
        try {
            r = r * 31 + b;
            r = r ^ (r >>> 7);
            r = r + (a / (b | 1));
            r = r * 5 - b;
            r = r ^ (r << 3);
        } catch (ArithmeticException e) {
            return -1;                  // reads nothing — RBC.6 must not fire
        }
        return r;
    }

    static int withoutTry(int a, int b) {
        int r = a;
        r = r * 31 + b;
        r = r ^ (r >>> 7);
        r = r + (a / (b | 1));
        r = r * 5 - b;
        r = r ^ (r << 3);
        return r;
    }

    static long benchTry(int iters) {
        int sink = 0;
        for (int i = 0; i < iters / 4; i++) { sink += withTry(i, i + 1); }
        long t = System.nanoTime();
        for (int i = 0; i < iters; i++) { sink += withTry(i, i + 1); }
        long ns = System.nanoTime() - t;
        report("withTry", ns, iters, sink);
        return ns;
    }

    static long benchNoTry(int iters) {
        int sink = 0;
        for (int i = 0; i < iters / 4; i++) { sink += withoutTry(i, i + 1); }
        long t = System.nanoTime();
        for (int i = 0; i < iters; i++) { sink += withoutTry(i, i + 1); }
        long ns = System.nanoTime() - t;
        report("withoutTry", ns, iters, sink);
        return ns;
    }

    static void report(String label, long ns, int iters, int sink) {
        System.out.println(String.format("%-12s %8dms %8.2fns/op sink=%d",
                label, ns / 1_000_000L, (double) ns / iters, sink));
    }

    /** Correctness: the two bodies must agree for every input. */
    static void verify() {
        int bad = 0;
        for (int i = -1000; i < 1000; i++) {
            if (withTry(i, i + 1) != withoutTry(i, i + 1)) { bad++; }
            if (withTry(i * 7, i) != withoutTry(i * 7, i)) { bad++; }
        }
        // b == 0 exercises the (b|1) guard; nothing may throw.
        for (int i = 0; i < 100; i++) {
            if (withTry(i, 0) != withoutTry(i, 0)) { bad++; }
        }
        System.out.println(bad == 0 ? "bodies agree" : ("BODIES DIVERGE: " + bad));
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 3_000_000;
        String only = args.length > 1 ? args[1] : "all";
        if (only.equals("all") || only.equals("verify")) { verify(); }
        if (only.equals("all") || only.equals("try")) { benchTry(iters); }
        if (only.equals("all") || only.equals("notry")) { benchNoTry(iters); }
    }
}
