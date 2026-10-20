// Lane L2 probe (interpreter round i1 wave 23): a loop whose header sits inside
// a NESTED `synchronized` region on one object. The optimizing tier's
// nested-lock elision deletes the inner enter/exit pair, so every frame state
// inside the loop names the lock twice: the level the compiled code holds
// (relock == false) and the elided level (relock == true), which a resume
// re-takes. Since wave 23 a method-entry body gives that header's back-edge
// poll a mode exit carrying that split
// (`ir_lower::Lowerer::back_edge_mode_exit_state`); before, the elided level
// refused the exit. Without an agent the exit is never taken; the untaken
// branch below is a guard trap inside the same region, which every sink
// resumes through the same `build_deopt_frame_inner` split. A sink that
// REFUSES such a frame now releases the compiled code's level before the
// re-run from entry; page:
// docs/internal/fixed-bugs/interpreter-L2-a-refused-resume-of-a-frame-holding-a-compiled-lock-leaks-the-lock-FIXED-20260926.md
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L2W23NestedLockLoop
//
// What to compare:
// * stdout must equal HotSpot 25's, with and without --nojit, in both modes.
//   HotSpot 25 prints:
//     warm-up checksum=398000000
//     trap call result=9770
//     side effects of the trap call=3
//     other thread took the lock=true
//     side effects of the other thread=100
//     main still holds the lock=false
//     nested depth inside=true,true
//   (the same with -Xint). A level left held (a leaked compiled lock, or an elided level re-taken
//   once too often) makes "other thread took the lock" false (the thread is a
//   daemon and the join times out); a level released twice throws
//   IllegalMonitorStateException; a replay of the trap call prints more than 3
//   side effects.
// * Coverage: CRATONVM_DBG_IR_LINEAR_SCAN=1 prints `[ir-ls] entry poll mode
//   exits:` per method-entry compile; `work`'s loop now counts as given
//   rather than `unlocatable` when the optimizing tier compiled it at entry
//   with the nested-lock elision on (CRATONVM_JIT_IR_NESTED_LOCK_ELIM unset).
// * Time: stderr carries the warm-up time only.
public class L2W23NestedLockLoop {
    static final Object LOCK = new Object();
    static int sideEffects;
    static volatile boolean nestedHeld;

    // The lock is a PARAMETER, so both regions name one IR node, which is
    // what the nested-lock elision requires (two `getstatic`s need not be).
    static long work(Object lock, int n, int mode) {
        long s = 0;
        synchronized (lock) {
            sideEffects++;
            synchronized (lock) {
                sideEffects++;
                for (int i = 0; i < n; i++) {
                    s += i ^ mode;
                    if (mode == 7 && i == n / 2) {
                        // Never taken while warming up.
                        s = -s;
                        nestedHeld = Thread.holdsLock(lock);
                    }
                }
            }
            sideEffects++;
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        long t0 = System.nanoTime();
        long acc = 0;
        for (int r = 0; r < 20_000; r++) {
            acc += work(LOCK, 200, r & 3);
        }
        System.err.printf("warm-up: %.2f ms%n", (System.nanoTime() - t0) / 1e6);
        int before = sideEffects;
        long trapped = work(LOCK, 200, 7);
        int afterTrap = sideEffects;
        Thread other = new Thread(() -> {
            synchronized (LOCK) {
                sideEffects += 100;
            }
        });
        other.setDaemon(true);
        other.start();
        other.join(10_000);
        boolean took = !other.isAlive();
        System.out.println("warm-up checksum=" + acc);
        System.out.println("trap call result=" + trapped);
        System.out.println("side effects of the trap call=" + (afterTrap - before));
        System.out.println("other thread took the lock=" + took);
        synchronized (LOCK) {
            System.out.println("side effects of the other thread=" + (sideEffects - afterTrap));
        }
        System.out.println("main still holds the lock=" + Thread.holdsLock(LOCK));
        System.out.println("nested depth inside=" + nestedHeld + "," + !Thread.holdsLock(LOCK));
    }
}
