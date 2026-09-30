// Lane L2 probe (interpreter round i1 wave 19): a method with a `synchronized`
// block before a loop is now admitted to optimizing method-entry poll exits
// (`ir_lower::ir_entry_mode_exits_admitted`), and a body that got an exit
// vouches for its frames (`can_deopt_resume`), so ANY trap of that body
// (a mode exit, or a guard such as a branch the warm-up never took) is resumed
// at its bci instead of re-running the method from entry.
//
// What to compare:
// * stdout must equal HotSpot 25's, with and without --nojit, under
//   --compatible. HotSpot prints, among the lines, exactly
//     side effects during warm-up=40000
//     side effects of the trap call=2
//   A whole-method re-run of the trap call (a replay) would print 3 or 4 on
//   the second line.
// * Coverage (no extra setup): CRATONVM_DBG_JITC=1 prints the exit line
//   `[c2-supersede] ir entry poll mode exits: given=N | refused: ...`; `work`
//   contributes one given back edge when the optimizing tier compiled it at
//   entry. CRATONVM_DBG_DEOPT=1 shows how the trap call was serviced.
// * Time: stderr carries the warm-up time only.
public class L2W19MonitorBlockLoop {
    static final Object LOCK = new Object();
    static int sideEffects;

    static long work(int n, int mode) {
        synchronized (LOCK) {
            sideEffects++;
        }
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i ^ mode;
        }
        sideEffects++;
        if (mode == 7) {
            // Never taken while warming up.
            s = -s;
        }
        return s;
    }

    public static void main(String[] args) {
        long t0 = System.nanoTime();
        long acc = 0;
        for (int r = 0; r < 20_000; r++) {
            acc += work(200, r & 3);
        }
        System.err.printf("warm-up: %.2f ms%n", (System.nanoTime() - t0) / 1e6);
        int before = sideEffects;
        long trapped = work(200, 7);
        System.out.println("warm-up checksum=" + acc);
        System.out.println("side effects during warm-up=" + before);
        System.out.println("trap call result=" + trapped);
        System.out.println("side effects of the trap call=" + (sideEffects - before));
    }
}
