// Lane L2 probe (interpreter round i1 wave 25): a long-running loop whose
// header sits INSIDE a `synchronized` block, entered once, so it reaches
// compiled code through the OSR door. An optimizing OSR body now gives that
// header's back-edge poll a mode exit whose frame names the held lock
// (`ir_lower::Lowerer::back_edge_mode_exit_state`, `OsrDoor` mode;
// `osr_exit::poll_exit_point_resumable`), and the planless in-place transfer
// keeps the hold with the live frame and rewrites its `held_monitors` record
// from the exit frame (`CompiledLocksOfAStash::replace_live_record_and_unpin`),
// so the interpreter's own `monitorexit` releases it exactly once. Without an
// agent the exit is never taken; the OSR body's normal completion, the
// record's handling and the lock's release are what stdout checks.
//
// Run on CratonVM:
//   cratonvm --java-home <jdk25> -cp <dir> L2W25OsrLoopInsideLock
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L2W25OsrLoopInsideLock
//   cratonvm --java-home <jdk25> --compatible -cp <dir> L2W25OsrLoopInsideLock
// stdout must equal HotSpot 25's in all three. HotSpot 25 prints:
//   sum=1249999975000000
//   side effects=2
//   other thread took the lock=true
//   side effects of the other thread=100
//   main still holds the lock=false
//   nested sum=499999500000
//   other thread took the nested lock=true
// A hold left by the OSR exit makes a `took` line false (the thread is a
// daemon and the join times out); one released twice throws
// IllegalMonitorStateException.
//
// Coverage: CRATONVM_DBG_JITC=1 prints `[c2-supersede] ir osr poll mode exits:
// given=N | refused: ...` at exit; `spin`'s header contributes a given exit
// instead of a `shape` refusal when the optimizing OSR tier compiled it
// (wave 24 and earlier: `shape`). `nested`'s header holds an ELIDED level
// when the nested-lock elision ran, which the in-place transfer cannot re-take,
// so it stays `unlocatable` there.
public class L2W25OsrLoopInsideLock {
    static final Object LOCK = new Object();
    static final Object NESTED = new Object();
    static int sideEffects;

    static long spin(int n) {
        long s = 0;
        synchronized (LOCK) {
            sideEffects++;
            for (int i = 0; i < n; i++) {
                s += i;
            }
            sideEffects++;
        }
        return s;
    }

    static long nested(int n) {
        long s = 0;
        synchronized (NESTED) {
            synchronized (NESTED) {
                for (int i = 0; i < n; i++) {
                    s += i;
                }
            }
        }
        return s;
    }

    static boolean otherTakes(Object lock, Runnable body) throws InterruptedException {
        Thread other = new Thread(() -> {
            synchronized (lock) {
                body.run();
            }
        });
        other.setDaemon(true);
        other.start();
        other.join(10_000);
        return !other.isAlive();
    }

    public static void main(String[] args) throws Exception {
        long t0 = System.nanoTime();
        long sum = spin(50_000_000);
        int after = sideEffects;
        System.err.printf("spin: %.2f ms%n", (System.nanoTime() - t0) / 1e6);
        System.out.println("sum=" + sum);
        System.out.println("side effects=" + after);
        boolean took = otherTakes(LOCK, () -> sideEffects += 100);
        System.out.println("other thread took the lock=" + took);
        synchronized (LOCK) {
            System.out.println("side effects of the other thread=" + (sideEffects - after));
        }
        System.out.println("main still holds the lock=" + Thread.holdsLock(LOCK));
        long nestedSum = nested(1_000_000);
        System.out.println("nested sum=" + nestedSum);
        System.out.println("other thread took the nested lock=" + otherTakes(NESTED, () -> {}));
    }
}
