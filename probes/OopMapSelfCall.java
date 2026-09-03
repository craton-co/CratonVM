/**
 * Does a SELF-RECURSIVE call with a reference argument cost this method its
 * precise oop map?
 *
 * The 0xb8 self-recursive arm of the single-pass backend raises
 * `pending_staged_args_unmapped` whenever any argument of the call is a
 * reference, which fails the NEXT safepoint closed
 * (`map_incomplete_cause::STAGED_ARG_UNMAPPABLE`) and, through
 * `relocation_coverage_complete`, withdraws that safepoint's relocation
 * claim. The two direct-call arms stopped doing that when
 * `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS` gave them a nameable service range;
 * this arm was never given one.
 *
 * <p>Two shapes, because the arm has two forms and they fail differently:
 *
 * <ul>
 *   <li>{@link #walk} is NON-tail (the result is added to), so it emits the
 *       stack-guard safepoint and then a real CALL. The guard safepoint is
 *       the one that consumes the flag -- and at that safepoint the argument
 *       oops are still sitting in ordinary frame slots that a map CAN name.
 *   <li>{@link #sum} is a TAIL self-call, which loads the arguments into the
 *       parameter locals and JMPs. It emits NO safepoint at all, so the flag
 *       it raises is consumed by some later, unrelated safepoint.
 * </ul>
 *
 * <h2>Running it</h2>
 *
 * <pre>
 *   javac -d $OUT probes/OopMapSelfCall.java
 *   CRATONVM_DBG=oopcov,jit-rootscan cratonvm --java-home $JDK25 --Xmx 256m \
 *       --verbose:gc -cp $OUT OopMapSelfCall
 * </pre>
 *
 * <p>Read `causes(... staged_unmappable=N ...)` on the `[oopcov]
 * frameslot-detail` line for `OopMapSelfCall.walk`/`sum`, and
 * `relocation_on_proven_jit` on `[GC] zgc-features`.
 */
public class OopMapSelfCall {
    static final int ROUNDS = 40000;
    static Object[] sink;
    static long guard;

    /** Non-tail self-recursion with a reference argument. */
    static int walk(Object[] node, int depth) {
        if (depth == 0) {
            // Allocate with `node` live across it: a real safepoint holding a
            // reference the map has to name.
            sink = new Object[] { node, new int[32] };
            return node.length;
        }
        Object[] next = new Object[] { node, new int[48] };
        // NON-tail: the result is used, so the call cannot be a JMP.
        return walk(next, depth - 1) + node.length;
    }

    /** Tail self-recursion with a reference argument. */
    static int sum(Object[] node, int acc) {
        if (node == null || acc > 24) {
            return acc;
        }
        Object[] next = (Object[]) node[0];
        // Tail position: invokestatic immediately followed by ireturn.
        return sum(next, acc + 1);
    }

    static Object[] chain(int n) {
        Object[] head = null;
        for (int i = 0; i < n; i++) {
            head = new Object[] { head, new int[16] };
        }
        return head;
    }

    public static void main(String[] args) {
        long total = 0;
        for (int r = 0; r < ROUNDS; r++) {
            total += walk(new Object[] { null, new int[8] }, 6);
            total += sum(chain(20), 0);
        }
        guard = total;
        System.out.println("PASS OopMapSelfCall total=" + total);
    }
}
