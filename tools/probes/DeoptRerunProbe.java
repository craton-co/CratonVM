// Does a compiled body that TRAPS run its side effects twice?
//
// A deopt sentinel does not mean "nothing happened": the compiled body ran up
// to the trapping bci and stopped. A sink that answers it by re-entering the
// method from bci 0 therefore runs everything before that bci a SECOND time.
// This probe makes that visible as a number.
//
//   delta == 1   the frame was resumed at the trapping bci
//   delta == 2   the method was re-entered from entry -- the store ran twice
//
// ## The shape, and why each part of it
//
//   * `SIDE_EFFECTS[0]++` is an `iastore` -- `opcode_commits_side_effect`, and
//     one the optimizing IR front end can actually lower. A `putstatic` cannot
//     be used: the front end has no lowering for 0xb3 and refuses the body.
//   * `i / d` is an `idiv`, which the IR tier lowers to a DEOPT GUARD
//     (`emit_deopt_if_zero`) rather than to a throw, so `d == 0` traps a
//     compiled body instead of raising from it.
//   * no `new` anywhere: an allocation-bearing method does not reach the
//     optimizing tier without `CRATONVM_JIT_C2_ALLOC_UPGRADE`.
//   * `hot` is called from `warm`, i.e. through a real `invokestatic`. That is
//     what routes the deopt into `execute_jit_call`'s sink rather than into
//     `execute`'s, which is a different sink with a different history.
//
// ## The flags, all four load-bearing
//
//   CRATONVM_TIER_C1_THRESHOLD=100000     take the Interpreter -> C2 door;
//   CRATONVM_TIER_C2_THRESHOLD=600        at C1 the single-pass backend sets
//   CRATONVM_TIER_C2_MIN_INVOCATIONS=500  `can_deopt_resume` and the sink
//                                         resumes anyway -- no defect visible.
//   CRATONVM_C2_ACCEPT=always             ...and KEEP the optimizing body. The
//                                         acceptance gate otherwise reports
//                                         `REFUSED (evidence: none)` and keeps
//                                         the single-pass one: a method simple
//                                         enough to be a clean witness is too
//                                         simple to earn an optimizing body.
//
// Measured 2026-09-07 (`docs/internal/fixed-bugs/
// jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md`):
// HotSpot 1, `--nojit` 1, C1 1, optimizing body 2, after the fix 1, and 2
// again with `CRATONVM_JIT_DEOPT_SINK_RESUME=0`.
public class DeoptRerunProbe {
    public static final int[] SIDE_EFFECTS = new int[1];

    static int hot(int i, int d) {
        SIDE_EFFECTS[0] = SIDE_EFFECTS[0] + 1;
        return i / d;
    }

    public static int warm(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) acc += hot(i, 7);
        return acc;
    }

    public static void main(String[] args) {
        int outer = Integer.getInteger("probe.outer", 400);
        int inner = Integer.getInteger("probe.inner", 64);
        long acc = 0;
        for (int k = 0; k < outer; k++) acc += warm(inner);
        int before = SIDE_EFFECTS[0];
        try {
            hot(1, 0);
        } catch (ArithmeticException e) {
            System.out.println("caught " + e.getMessage());
        }
        int delta = SIDE_EFFECTS[0] - before;
        System.out.println("acc=" + acc + " delta=" + delta
                + (delta == 1 ? "  (RESUMED)" : delta == 2 ? "  (RE-RAN: side effect twice)" : "  (?)"));
    }
}
