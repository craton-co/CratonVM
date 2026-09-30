// SUPERSEDED AS EVIDENCE, 2026-09-22 — kept because five source comments and
// three known-issue pages name it, and it is still a serviceable manual
// reproducer. It is NOT a measurement instrument, and was mistaken for one:
// the four values it prints are computed in `main`, which this VM never
// compiles, so every column of its page's closing table read the INTERPRETER
// while a wrong answer was live in two JIT arms (9,941 of 10,000 calls, NPE
// shape). See
// `docs/internal/retired/r10-self-recursive-catch-of-its-own-call-is-miscompiled-20260921-RETIRED-20260922.md`.
//
// The guard is `regression-suite/src/RJitSelfRecCatch.java`, scheduled in
// `CORE_CLASSES`, which asserts only values produced inside `drive`. Use that
// to decide anything; use this to eyeball a shape.
//
// r10 probe: a self-recursive method that CATCHES what its own recursive call
// throws. This is the exact shape lane `irlow` fixed in
// `emit_self_recursive_call` (jit/src/ir_lower.rs): that route was the only
// raw-CALL route that never gave the callee's own exception table a look before
// treating the i64::MIN deopt sentinel as the caller's own unwind.
//
// Two shapes, both deterministic, both printed:
//   explicit -- the callee throws `new IllegalStateException`
//   implicit -- the callee divides by zero (ArithmeticException raised by the
//               hardware trap, which is the path that actually produces the
//               sentinel)
//
// The work is in `drive`, never in `main`, because a loop in `main` measures
// interpreted code in this VM.
public class R10SelfRecCatch {

    static long recExplicit(int n, int[] a) {
        if (n <= 0) {
            throw new IllegalStateException("bottom");
        }
        try {
            return recExplicit(n - 1, a) + a[n % a.length];
        } catch (IllegalStateException e) {
            return a[n % a.length];
        }
    }

    // The callee's exception is IMPLICIT (a divide-by-zero trap), and it is
    // caught by the caller's own handler one activation up.
    static long recImplicit(int n, int d) {
        if (n <= 0) {
            return 1 / d;
        }
        try {
            return recImplicit(n - 1, d) + n;
        } catch (ArithmeticException e) {
            return -n;
        }
    }

    // A handler several activations above the throw: every level rethrows until
    // the level that catches. Checks that N-deep recursion does not collapse
    // into the outermost frame's throw site.
    static long recDeep(int n, int floor) {
        if (n <= 0) {
            throw new IllegalStateException("bottom");
        }
        if (n <= floor) {
            return recDeep(n - 1, floor);
        }
        try {
            return recDeep(n - 1, floor) + n;
        } catch (IllegalStateException e) {
            return 1000 + n;
        }
    }

    static long drive(int reps, int[] a) {
        long acc = 0;
        for (int i = 0; i < reps; i++) {
            acc += recExplicit(12, a);
            acc += recImplicit(9, 0);
            acc += recImplicit(9, 3);
            acc += recDeep(11, 4);
        }
        return acc;
    }

    public static void main(String[] args) {
        int[] a = new int[7];
        for (int i = 0; i < a.length; i++) {
            a[i] = i * 3 + 1;
        }
        long total = 0;
        for (int w = 0; w < 400; w++) {
            total += drive(500, a);
        }
        System.out.println("explicit=" + recExplicit(12, a));
        System.out.println("implicitThrow=" + recImplicit(9, 0));
        System.out.println("implicitOk=" + recImplicit(9, 3));
        System.out.println("deep=" + recDeep(11, 4));
        System.out.println("total=" + total);
        System.out.println("PASS R10SelfRecCatch");
    }
}
