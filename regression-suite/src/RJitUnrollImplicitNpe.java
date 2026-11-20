/**
 * Regression: a null receiver dereferenced in an UNROLLED copy of a loop body
 * must throw NullPointerException, not kill the process.
 *
 * The single-pass backend elides the receiver null check at a `getfield` it
 * cannot prove non-null and lets the dereference FAULT instead, translating the
 * SIGSEGV into an NPE by looking the faulting PC up in a table
 * (`jit::implicit_null`). Its native unroller duplicates the loop body's machine
 * code, so each copy holds that dereference at a different PC — and every other
 * patch vector the duplicator shifts (forward patches, bounds-check stubs,
 * exception-check stubs, null-check-store stubs, self-call patches, deopt stubs,
 * jump-table patches, oop maps, inline-cache slots) was shifted while the
 * implicit-null table was not, because it post-dates that sweep.
 *
 * The shape, all three parts of which are needed:
 *
 *   * a counted loop with a CONSTANT trip count, small enough that the unroller
 *     admits it (a 20-byte body gets four copies);
 *   * a receiver REASSIGNED in the body (`o = o.next`), so no dataflow can prove
 *     it non-null on the next iteration and the check is left implicit;
 *   * a list SHORTER than the trip count, so the receiver goes null in a copy
 *     that is not copy 0.
 *
 * Measured before the fix: correct under `CRATONVM_DISABLE_UNROLL=1` and an
 * `EXCEPTION_ACCESS_VIOLATION` reading address 0x0F without it, from
 * byte-identical loop bodies — only the table differed. A crash is not a wrong
 * checksum, so the runner's HotSpot diff cannot be the whole test: `traps` is,
 * and a process that dies reports nothing at all.
 *
 * The long list is the control. If the loop stopped being unrolled, or the check
 * stopped being implicit, `walk5` still has to produce 15 and `traps` still has
 * to be `N`, so a green run on a changed backend still means what it says.
 */
public class RJitUnrollImplicitNpe {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    private static final int N = 200_000;

    static final class Node {
        final int v;
        final Node next;
        Node(int v, Node next) { this.v = v; this.next = next; }
    }

    /**
     * Five iterations over a pointer walk. The trip count is constant and the
     * body is 20 bytecodes, which is what puts it in the unroller's 4x band.
     */
    private static int walk(Node o) {
        int a = 0;
        for (int i = 0; i < 5; i++) {
            a += o.v;
            o = o.next;
        }
        return a;
    }

    private static Node list(int len) {
        Node head = null;
        for (int i = len; i > 0; i--) {
            head = new Node(i, head);
        }
        return head;
    }

    public static void main(String[] args) {
        Node five = list(5);
        Node three = list(3);

        long sum = 0;
        int traps = 0;
        int spurious = 0;
        for (int i = 0; i < N; i++) {
            // Control: long enough, never null, must always return 1+2+3+4+5.
            sum += walk(five);
            // The vector: goes null in the fourth iteration, i.e. inside a copy
            // the duplicator made rather than the one the emitter wrote.
            try {
                sum += walk(three);
                spurious++;
            } catch (NullPointerException e) {
                traps++;
            }
        }

        check(spurious == 0, "a three-element list survived five dereferences x" + spurious);
        check(traps == N, "expected " + N + " NullPointerExceptions, got " + traps);
        check(sum == 15L * N, "sum " + sum);

        System.out.println("CK sum=" + sum + " traps=" + traps + " spurious=" + spurious
                + " checksum=" + (sum + traps * 31L));
        System.out.println("PASS RJitUnrollImplicitNpe (" + checks + " checks)");
    }
}
