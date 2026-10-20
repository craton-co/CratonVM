// The WHOLE deopt-guard family, one arm per guard emitter.
//
// `probes/DeoptRerunProbe.java` is the single-shape witness for the sink defect
// itself (`idiv` guard + `iastore` side effect,
// `jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md`) and it is
// the one to read first: it explains the mechanism, the four load-bearing
// flags, and the `delta == 1` / `delta == 2` verdict.
//
// This one widens that to every opcode the optimizing tier lowers to a RUNTIME
// deopt guard rather than to a throw — array load, array store, `arraylength`,
// `getfield`, `putfield`, `idiv`, `ldiv`, `irem` — because those are four
// separate emitters in `jit/src/ir_lower.rs`
// (`emit_array_null_bounds_guards_for`, the inline field null check,
// `emit_deopt_if_zero`) and a regression in one of them is invisible to a probe
// that exercises another. None of these arms involves an `invokedynamic`, so
// no compile-side trap refusal can screen them.
//
// Each arm's method has NO exception table and does `h.n++` BEFORE the guarded
// opcode, so a replay from entry is visible as a count: **ten arms must add
// exactly 10 to `Holder.n`.** The exceptions themselves are asserted too, but
// they were never the part that broke — every binary gets those right.
//
//   cratonvm --java-home <jdk> -c <dir> DeoptTrapProbe [warm-iterations]
//
// `CRATONVM_C2_ACCEPT=always` is required to reach the sinks at all, for the
// reason `DeoptRerunProbe` documents: the acceptance gate reports
// `REFUSED (evidence: none)` for methods this simple and keeps the single-pass
// body, which throws directly and stashes no frame. Without it every arm passes
// on every binary and the probe asserts nothing about these sinks.
//
// HotSpot prints `PROBE PASS`; so must CratonVM, on every tier.
public class DeoptTrapProbe {
    static final class Holder { int n; }
    static final class Node { int v = 5; }

    static int guardedArrayLoad(Holder h, int[] a, int i) { h.n++; return a[i]; }
    static void guardedArrayStore(Holder h, int[] a, int i) { h.n++; a[i] = 3; }
    static int guardedArrayLength(Holder h, int[] a) { h.n++; return a.length; }
    static int guardedGetField(Holder h, Node n) { h.n++; return n.v; }
    static void guardedPutField(Holder h, Node n) { h.n++; n.v = 7; }
    static int guardedDiv(Holder h, int x, int y) { h.n++; return x / y; }
    static long guardedLdiv(Holder h, long x, long y) { h.n++; return x / y; }
    static int guardedRem(Holder h, int x, int y) { h.n++; return x % y; }

    static int fails;

    static void arm(String name, String want, Runnable r) {
        try {
            r.run();
            System.out.println(name + ": FAIL - no exception (wanted " + want + ")");
            fails++;
        } catch (Throwable t) {
            String got = t.getClass().getName();
            if (got.equals(want)) {
                System.out.println(name + ": ok " + got);
            } else {
                System.out.println(name + ": FAIL - got " + got + " (wanted " + want + ")");
                if (t.getMessage() != null) System.out.println("    " + t.getMessage());
                fails++;
            }
        }
    }

    public static void main(String[] args) {
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 400000;
        final int[] a = new int[8];
        final Node node = new Node();
        final Holder h = new Holder();
        for (int i = 0; i < warm; i++) {
            guardedArrayLoad(h, a, i & 7);
            guardedArrayStore(h, a, i & 7);
            guardedArrayLength(h, a);
            guardedGetField(h, node);
            guardedPutField(h, node);
            guardedDiv(h, i, 3);
            guardedLdiv(h, i, 3);
            guardedRem(h, i, 3);
        }
        // Every arm below must add EXACTLY ONE to `h.n`: the guarded opcode
        // trapped, so the store before it ran once and must not be replayed.
        int before = h.n;
        arm("aload/oob",        "java.lang.ArrayIndexOutOfBoundsException", () -> guardedArrayLoad(h, a, 99));
        arm("aload/null",       "java.lang.NullPointerException",           () -> guardedArrayLoad(h, null, 0));
        arm("astore/oob",       "java.lang.ArrayIndexOutOfBoundsException", () -> guardedArrayStore(h, a, 99));
        arm("astore/null",      "java.lang.NullPointerException",           () -> guardedArrayStore(h, null, 0));
        arm("arraylength/null", "java.lang.NullPointerException",           () -> guardedArrayLength(h, null));
        arm("getfield/null",    "java.lang.NullPointerException",           () -> guardedGetField(h, null));
        arm("putfield/null",    "java.lang.NullPointerException",           () -> guardedPutField(h, null));
        arm("idiv/zero",        "java.lang.ArithmeticException",            () -> guardedDiv(h, 7, 0));
        arm("ldiv/zero",        "java.lang.ArithmeticException",            () -> guardedLdiv(h, 7L, 0L));
        arm("irem/zero",        "java.lang.ArithmeticException",            () -> guardedRem(h, 7, 0));
        int added = h.n - before;
        if (added != 10) {
            System.out.println("side-effect count: FAIL - 10 arms added " + added
                    + " to Holder.n; a replay from entry duplicated the store");
            fails++;
        } else {
            System.out.println("side-effect count: ok 10");
        }
        System.out.println(fails == 0 ? "PROBE PASS" : "PROBE FAIL fails=" + fails);
        // Exit status, not just the printed line: a harness that classifies on
        // `rc` would otherwise read every failure as a pass, which is the
        // failure mode this probe exists to catch in the VM.
        System.exit(fails == 0 ? 0 : 1);
    }
}
