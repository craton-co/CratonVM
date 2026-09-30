/**
 * The positive control `IrEscapeProbe` deliberately is not.
 *
 * `IrEscapeProbe` passes its allocation to a call precisely so it ESCAPES and
 * the `Op::New` survives to codegen. That makes it useless for asking whether
 * scalar replacement ever fires. This one is its mirror: every allocation is
 * consumed entirely within the method that made it, so escape analysis has a
 * NoEscape population to replace if it is going to replace anything.
 *
 * Constraints copied from IrEscapeProbe so the optimizing tier admits it: no
 * arrays, no statics, no athrow, no invokedynamic, no checkcast/instanceof,
 * no try/catch in the hot methods.
 */
public class ScalarReplProbe {
    static final class Cell {
        int a;
        int b;
        Cell(int a, int b) { this.a = a; this.b = b; }
        int sum() { return a + b; }
    }

    // The allocation never leaves: constructed, read, discarded.
    static int hot(int x, int y) {
        Cell c = new Cell(x, y);
        int s = c.sum();
        Cell d = new Cell(s, x ^ y);
        return d.a - d.b + c.a;
    }

    static int loop(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += hot(i, acc);
        }
        return acc;
    }

    public static void main(String[] args) {
        int acc = 0;
        for (int r = 0; r < 200; r++) {
            acc ^= loop(500);
        }
        System.out.println("ScalarReplProbe " + acc);
    }
}
