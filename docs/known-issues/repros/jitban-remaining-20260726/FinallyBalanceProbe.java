// Standalone witness for the defect behind SPRING-TESTCOMPILER.3 symptom (a).
//
// The [PUTFIELD-WATCH] ledger of `com.sun.tools.javac.comp.Annotate.blockCount`
// shows that once `ClassFinder.complete` is JIT-compiled it runs
// `annotate.blockAnnotations()` (counter++) without the matching
// `annotate.unblockAnnotationsNoFlush()` (counter--) that lives in its
// `finally`, leaving the counter permanently > 0 -- which makes
// `Annotate.flush()` a no-op for the rest of that compilation.
//
// `complete`'s shape, reduced: a counter increment INSIDE a try region whose
// only handler is a catch-all `any` (a javac `finally`), a callee that throws
// through it, and a caller that catches. javac drives that path constantly
// while resolving cross-compilation-unit references, because a missing class
// is signalled with `CompletionFailure`.
public class FinallyBalanceProbe {

    static int depth = 0;
    static long sink = 0;

    static class Boom extends RuntimeException {
        Boom() { super(null, null, false, false); }   // cheap, no stack trace
    }

    static void work(int i) {
        sink += i;
        if ((i % 7) == 3) {
            throw new Boom();
        }
    }

    // Shape of ClassFinder.complete: the increment is inside the protected
    // range, the decrement is the finally.
    static void guarded(int i) {
        try {
            depth++;
            work(i);
        } finally {
            depth--;
        }
    }

    // Same, one frame deeper -- the real `complete` reaches its throwing
    // callees through completeOwners/completeEnclosing/fillIn.
    static void guardedNested(int i) {
        try {
            depth++;
            guardedInner(i);
        } finally {
            depth--;
        }
    }

    static void guardedInner(int i) {
        work(i);
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 400000;
        int firstLeakFlat = -1, firstLeakNested = -1;

        for (int i = 0; i < iterations; i++) {
            try {
                guarded(i);
            } catch (Boom e) {
                // expected
            }
            if (depth != 0 && firstLeakFlat < 0) {
                firstLeakFlat = i;
                System.out.println("FLAT   leak at iteration " + i + " depth=" + depth);
            }
        }
        System.out.println("FLAT   final depth=" + depth + " (expect 0) firstLeak=" + firstLeakFlat);

        depth = 0;
        for (int i = 0; i < iterations; i++) {
            try {
                guardedNested(i);
            } catch (Boom e) {
                // expected
            }
            if (depth != 0 && firstLeakNested < 0) {
                firstLeakNested = i;
                System.out.println("NESTED leak at iteration " + i + " depth=" + depth);
            }
        }
        System.out.println("NESTED final depth=" + depth + " (expect 0) firstLeak=" + firstLeakNested);

        System.out.println("RESULT: " + ((firstLeakFlat < 0 && firstLeakNested < 0) ? "OK" : "FAIL")
                + " sink=" + sink);
    }
}
