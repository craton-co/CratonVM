/**
 * Differential check for `CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES`, against
 * HotSpot.
 *
 * The loop kernels this optimization was measured on prove nothing about it:
 * they are trap-free, so no deopt is ever taken and a frame state that
 * reconstructed garbage would never be consulted. What has to be shown is the
 * other half — that a method which CAN deopt keeps its home words, takes the
 * deopt, and reconstructs the right values.
 *
 * So each kernel below is a hot loop with an INTERMEDIATE live across a trap
 * point, and each trap really fires part-way through a run long enough to have
 * been compiled first:
 *
 * <ul>
 *   <li>{@link #divisionTrap} — a zero divisor. `Op::Div`'s guard is a deopt
 *       point, and the frame state at that bci names the partially-computed
 *       sum and the multiply feeding the dividend.</li>
 *   <li>{@link #nullTrap} — a field read through a reference that goes null,
 *       so the implicit null check traps with an intermediate on the stack.</li>
 *   <li>{@link #boundsTrap} — an array index walking off the end.</li>
 *   <li>{@link #trapFree} — the control: pure arithmetic, where the
 *       optimization actually engages. Its answer must not change either.</li>
 * </ul>
 *
 * Every kernel folds into one `long` whose value depends on the intermediates
 * that were live at the trap, so a frame state that reconstructed a stale or
 * wrong word shows up as a different checksum rather than as a crash.
 *
 * Kernels are their own methods for the reason `OsrTierProbe` records: a `main`
 * that reads a system property or concatenates its output disqualifies itself
 * from the optimizing tier, and a loop inlined into it would go with it.
 */
public class DeoptLiveProbe {

    /** The control: nothing here can trap, so the optimization engages. */
    static long trapFree(int n) {
        long sum = 0;
        int acc = 1;
        for (int i = 0; i < n; i++) {
            acc = acc * 31 + (i & 7);
            sum += (acc & 0xFF);
        }
        return sum * 1000003L + acc;
    }

    /**
     * `acc` and `sum` are live across the divide, and `t` is an intermediate
     * the frame state at the division's bci names on the operand stack.
     */
    static long divisionTrap(int n) {
        long sum = 0;
        int acc = 1;
        int traps = 0;
        for (int i = 0; i < n; i++) {
            acc = acc * 31 + (i & 7);
            int d = (i % 1000 == 999) ? 0 : (i & 15) + 1;
            try {
                int t = (acc & 0xFFFF) / d;
                sum += t;
            } catch (ArithmeticException e) {
                // The deopt happened with `acc`, `sum` and `traps` live.
                traps++;
                sum += acc & 0xFF;
            }
        }
        return sum * 31L + acc + traps * 7L;
    }

    static final class Cell {
        final int v;
        Cell(int v) { this.v = v; }
    }

    /** A field read whose receiver goes null part-way through. */
    static long nullTrap(int n) {
        long sum = 0;
        int acc = 1;
        int traps = 0;
        Cell live = new Cell(3);
        for (int i = 0; i < n; i++) {
            acc = acc * 31 + (i & 7);
            Cell c = (i % 997 == 996) ? null : live;
            try {
                sum += (acc & 0xFF) + c.v;
            } catch (NullPointerException e) {
                traps++;
                sum += acc & 0x3F;
            }
        }
        return sum * 131L + acc + traps * 11L;
    }

    /** An index that walks off the end. */
    static long boundsTrap(int n) {
        long sum = 0;
        int acc = 1;
        int traps = 0;
        int[] a = new int[16];
        for (int i = 0; i < 16; i++) {
            a[i] = i * 3;
        }
        for (int i = 0; i < n; i++) {
            acc = acc * 31 + (i & 7);
            int idx = (i % 991 == 990) ? 64 : (acc & 15);
            try {
                sum += a[idx];
            } catch (ArrayIndexOutOfBoundsException e) {
                traps++;
                sum += acc & 0x1F;
            }
        }
        return sum * 17L + acc + traps * 13L;
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 2_000_000);
        long a = trapFree(n);
        long b = divisionTrap(n);
        long c = nullTrap(n);
        long d = boundsTrap(n);
        System.out.print("DLP trapFree=");
        System.out.print(a);
        System.out.print(" division=");
        System.out.print(b);
        System.out.print(" null=");
        System.out.print(c);
        System.out.print(" bounds=");
        System.out.println(d);
    }
}
