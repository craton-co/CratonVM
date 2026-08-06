// Residual of the cov-07 athrow lane (see
// fixed-suite-bugs/hibernate/offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md,
// "Why COV-07 is the leading suspect" — the paragraph flagging the shared
// exception stub's missing `jit_set_throw_bci` stamp as a pre-existing gap the
// cov-07 lane did not own).
//
// The IR tier's shared exceptional-exit stub (`ir_lower::emit_call_exc_stub`)
// did not stamp `jit_set_throw_bci`, so when a dispatched callee threw, the
// compiled caller returned the `i64::MIN` sentinel with
// `JitSignals::athrow_bci` still at -1 (the general
// `set_jit_pending_exception` resets it, and only `jit_throw_exception` sets a
// real bci).
//
// The interpreter then routes with `throw_pc == usize::MAX`, and
// `find_jit_exception_handler`'s unknown-pc rule honours a catch-all ONLY when
// its protected region spans the whole method. A javac `finally` region never
// does — so the `finally` is silently skipped and the balance leaks one count
// per throw. The single-pass backend has stamped the bci since RBC.6
// (`x64/deopt_stubs.rs`, `emit_exception_check_stub`); the IR tier did not.
//
// `body` is written to be C2-admissible: no `putstatic` (0xb3 has no IR
// lowering), and the `finally` handler reads only a PARAMETER local, so RBC.6's
// `local_handler_reads_unsafe_local` does not force the method back to the
// single-pass backend.
//
// HotSpot / --nojit control: leaked=0.
public class IrFinallyBciProbe {
    // Not inlined into `body`'s compiled form: it is the DISPATCHED callee
    // whose throw exits `body` through the shared stub.
    static void thrower(int i) {
        throw new RuntimeException("x" + i);
    }

    // `try { n[0]+=1; thrower(i); } finally { n[0]-=1; }` — the protected
    // region starts at 0 but ends well before the method does, which is exactly
    // the shape the unknown-pc rule refuses.
    //
    // Written long-hand, not `n[0]++`: the compound form compiles to `dup2`
    // (0x5c), which the IR builder has no lowering for, and the whole method
    // would fall back to the single-pass backend — where this defect does not
    // exist, so the probe would pass vacuously.
    static void body(int i, int[] n) {
        try {
            n[0] = n[0] + 1;
            thrower(i);
        } finally {
            n[0] = n[0] - 1;
        }
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int[] n = new int[1];
        int caught = 0;
        for (int i = 0; i < iters; i++) {
            try {
                body(i, n);
            } catch (RuntimeException e) {
                caught++;
            }
        }
        System.out.println("caught=" + caught);
        System.out.println("leaked=" + n[0]);
        System.out.println("OK");
    }
}
