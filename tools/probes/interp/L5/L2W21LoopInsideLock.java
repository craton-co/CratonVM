// Lane L2 probe (interpreter round i1 wave 21): a loop whose header sits
// INSIDE a `synchronized` block. An optimizing method-entry body of `work` now
// gives that header's back-edge poll a mode exit whose frame names the held
// lock (`ir_lower::Lowerer::back_edge_mode_exit_state`, relock == false), and
// the stash sinks rebuild the frame keeping the lock, so the interpreter's own
// `monitorexit` releases it exactly once. Without an agent the exit is never
// taken; the untaken branch below is a guard trap inside the same block,
// which is resumed the same way (the body vouches for its frames).
//
// What to compare:
// * stdout must equal HotSpot 25's, with and without --nojit, under
//   --compatible. HotSpot prints, among the lines,
//     side effects of the trap call=2
//     other thread took the lock=true
//     side effects of the other thread=100
//     main still holds the lock=false
//   A lock left held by a resume makes the second line false (the thread is
//   a daemon and the join times out); one released twice throws
//   IllegalMonitorStateException; a replay of the trap call prints 3 or 4 on
//   the first line.
// * Coverage (no extra setup): CRATONVM_DBG_JITC=1 prints
//   `[c2-supersede] ir entry poll mode exits: given=N | refused: ...`; `work`
//   now contributes a given exit instead of a `shape` refusal when the
//   optimizing tier compiled it at entry.
// * Time: stderr carries the warm-up time only.
public class L2W21LoopInsideLock {
    static final Object LOCK = new Object();
    static int sideEffects;

    static long work(int n, int mode) {
        long s = 0;
        synchronized (LOCK) {
            sideEffects++;
            for (int i = 0; i < n; i++) {
                s += i ^ mode;
                if (mode == 7 && i == n / 2) {
                    // Never taken while warming up.
                    s = -s;
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
            acc += work(200, r & 3);
        }
        System.err.printf("warm-up: %.2f ms%n", (System.nanoTime() - t0) / 1e6);
        int before = sideEffects;
        long trapped = work(200, 7);
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
    }
}
