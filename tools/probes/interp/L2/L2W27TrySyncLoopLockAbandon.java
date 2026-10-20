// Lane L2 probe (interpreter round i1 wave 27): a `try { synchronized } catch`
// INSIDE a loop, the shape wave 26's single-pass monitor analysis answered
// nothing for (`interpreter-L2-single-pass-monitor-analysis-answers-nothing-in-a-loop-around-a-try-synchronized-FIXED-20260928`):
// without catch types the enclosing `catch` merged both sides of the
// `monitorenter` at its handler, the loop carried that to every pc, and every
// frame of the loop — the protected call inside the block included — named no
// lock, so a compiled frame abandoned inside the block left the lock held.
// Since wave 27 the backend request carries a catch-all bit per exception-table
// entry and the analysis stops a pc's handler search at the block's catch-all
// (`x64/deopt_stubs.rs` `taken_monitor_slots_per_pc`).
//
// The loop is driven long enough to compile (method entry and OSR), then with
// receivers its call site did not see (a guard or callee trap inside the
// block), with a callee that throws through the block into the loop's own
// `catch`, and with a long call-free stretch inside the block. After each
// phase another thread must be able to take the lock (a hold left behind makes
// a `took` line false: the other thread is a daemon and the join times out); a
// hold released twice throws IllegalMonitorStateException.
//
// The deciding evidence is the jit crate tests
// `x64::tests::w27_single_pass_frames_name_the_lock_in_a_loop_around_a_try_synchronized`
// and `deopt_stubs::w26_taken_monitor_tests::a_try_synchronized_in_a_loop_*`;
// this probe checks the reachable abandon paths end to end.
//
// Run on CratonVM (the first line keeps every method with an exception table,
// i.e. every synchronized block, on the single-pass tier):
//   CRATONVM_JIT_NO_EXC_TABLE_C2=1 cratonvm --java-home <jdk25> -cp <dir> L2W27TrySyncLoopLockAbandon
//   cratonvm --java-home <jdk25> -cp <dir> L2W27TrySyncLoopLockAbandon
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L2W27TrySyncLoopLockAbandon
//   cratonvm --java-home <jdk25> --compatible -cp <dir> L2W27TrySyncLoopLockAbandon
// stdout must equal HotSpot 25's in all four. HotSpot 25 prints:
//   warm=2000000 caught=0
//   warm: other thread took the lock=true
//   mixed=3000000 caught=0
//   mixed: other thread took the lock=true
//   thrown=1100000 caught=100000
//   thrown: other thread took the lock=true
//   spin=62500986500000 caught=0
//   spin: other thread took the lock=true
//   main holds the lock=false
// (`-Xint` prints the same.) Before wave 27: not run in-lane; the abandon
// paths it drives are the ones wave 26's `L2W26SinglePassLockAbandon` passes
// for the loop-free shape.
public class L2W27TrySyncLoopLockAbandon {
    static final Object LOCK = new Object();
    static long counter;
    static int caught;

    static final class Count implements Runnable {
        public void run() {
            counter++;
        }
    }

    static final class Twice implements Runnable {
        public void run() {
            counter += 2;
        }
    }

    static final class Thrower implements Runnable {
        public void run() {
            counter += 2;
            throw new IllegalStateException("from inside the block");
        }
    }

    /** The page's shape: the loop around `try { synchronized } catch`. */
    static void loop(Object lock, Runnable[] rs, int n) {
        for (int i = 0; i < n; i++) {
            try {
                synchronized (lock) {
                    rs[i % rs.length].run();
                }
            } catch (RuntimeException e) {
                caught++;
            }
        }
    }

    /** The same loop with a long call-free stretch inside the block. */
    static long spin(Object lock, int n, int inner) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            try {
                synchronized (lock) {
                    for (int j = 0; j < inner; j++) {
                        s += j;
                    }
                }
            } catch (RuntimeException e) {
                caught++;
            }
        }
        return s;
    }

    static boolean otherTakes(Object lock) throws InterruptedException {
        Thread other = new Thread(() -> {
            synchronized (lock) {
                counter += 0;
            }
        });
        other.setDaemon(true);
        other.start();
        other.join(10_000);
        return !other.isAlive();
    }

    public static void main(String[] args) throws Exception {
        Runnable count = new Count();
        Runnable twice = new Twice();
        Runnable thrower = new Thrower();

        for (int k = 0; k < 20; k++) {
            loop(LOCK, new Runnable[] {count}, 100_000);
        }
        System.out.println("warm=" + counter + " caught=" + caught);
        System.out.println("warm: other thread took the lock=" + otherTakes(LOCK));

        counter = 0;
        loop(LOCK, new Runnable[] {count, twice}, 2_000_000);
        System.out.println("mixed=" + counter + " caught=" + caught);
        System.out.println("mixed: other thread took the lock=" + otherTakes(LOCK));

        counter = 0;
        loop(LOCK, new Runnable[] {count, count, count, count, count, count, count, count, count, thrower},
                1_000_000);
        System.out.println("thrown=" + counter + " caught=" + caught);
        System.out.println("thrown: other thread took the lock=" + otherTakes(LOCK));

        caught = 0;
        long warmSpin = 0;
        for (int k = 0; k < 200; k++) {
            warmSpin += spin(LOCK, 10, 1_000);
        }
        System.out.println("spin=" + (warmSpin + spin(LOCK, 5, 5_000_000)) + " caught=" + caught);
        System.out.println("spin: other thread took the lock=" + otherTakes(LOCK));
        System.out.println("main holds the lock=" + Thread.holdsLock(LOCK));
    }
}
