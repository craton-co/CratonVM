/**
 * Does `CRATONVM_SCALAR_DEOPT=1` miscompile the shape the IR-vs-single-pass
 * differential caught, in a REAL program under the real VM?
 *
 * `jit/tests/ir_vs_singlepass.rs::ir_elidable_trivial_init_on_fresh_new_is_still_elided`
 * fails with that flag set and passes without it, returning what looks like a
 * heap address where an `int` belongs. That test drives the compiler directly
 * with stub helpers, so on its own it cannot tell a real miscompile from a
 * harness artifact. This is the same bytecode shape reached the ordinary way:
 *
 *     static int f(int n) { Box b = new Box(); b.v = n; return b.v; }
 *
 *   new Box; dup; invokespecial <init>()V; astore_1;
 *   aload_1; iload_0; putfield v; aload_1; getfield v; ireturn
 *
 * — a fresh allocation with a trivial `<init>`, one field written and read
 * back, and nothing else. Escape analysis should scalar-replace the object and
 * the method should compile to `return n`.
 *
 * The answer is checked, not printed: `f(i)` must be `i` for every i, so a
 * wrong value is a MISMATCH line and a non-zero exit rather than a number a
 * reader has to evaluate. Two more shapes sit beside it to bound the defect —
 * a two-field object, and one whose field is read before it is written (the
 * zero-default path).
 */
public class ScalarDeoptProbe {

    static final class Box {
        int v;
    }

    static final class Pair {
        int a;
        int b;
    }

    /** The exact shape the differential test drives. */
    static int writeThenRead(int n) {
        Box b = new Box();
        b.v = n;
        return b.v;
    }

    /** Two fields, so a field-index mix-up shows up as a swap. */
    static int twoFields(int n) {
        Pair p = new Pair();
        p.a = n;
        p.b = n + 1;
        return p.b - p.a;          // must be 1
    }

    /** Read before write: the zero-default path. */
    static int readBeforeWrite(int n) {
        Box b = new Box();
        int before = b.v;          // must be 0
        b.v = n;
        return before + b.v;       // must be n
    }

    public static void main(String[] args) {
        int iters = Integer.getInteger("sd.iters", 400_000);
        long bad1 = 0, bad2 = 0, bad3 = 0;
        long firstBadIn = -1, firstBadOut = 0;

        for (int i = 0; i < iters; i++) {
            int r1 = writeThenRead(i);
            if (r1 != i) {
                if (bad1 == 0) { firstBadIn = i; firstBadOut = r1; }
                bad1++;
            }
            if (twoFields(i) != 1) bad2++;
            if (readBeforeWrite(i) != i) bad3++;
        }

        System.out.println("iters=" + iters
                + " writeThenRead_bad=" + bad1
                + " twoFields_bad=" + bad2
                + " readBeforeWrite_bad=" + bad3);
        if (bad1 != 0) {
            System.out.println("*** MISMATCH writeThenRead(" + firstBadIn + ") = " + firstBadOut
                    + "  (0x" + Long.toHexString(firstBadOut) + ")");
        }
        if (bad1 == 0 && bad2 == 0 && bad3 == 0) {
            System.out.println("PASS ScalarDeoptProbe");
        } else {
            System.out.println("FAIL ScalarDeoptProbe");
            System.exit(1);
        }
    }
}
