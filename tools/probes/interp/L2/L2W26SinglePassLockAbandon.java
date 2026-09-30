// Lane L2 probe (interpreter round i1 wave 26): single-pass bodies holding a
// `synchronized` block's lock when they leave compiled code other than through
// the block's own `monitorexit` — a callee that throws through the block, a
// receiver the call site's speculation did not expect (a guard or callee trap
// inside the block), and a long loop inside the block (the OSR door's exits).
// Since wave 26 the single-pass backend's frame states name the lock the code
// took (`x64/deopt_stubs.rs` `taken_monitor_slots_per_pc`,
// `build_frame_state_at`), so every VM path that abandons such a frame
// releases it and every one that rebuilds it hands it over
// (`interpreter-L2-single-pass-frames-describe-no-lock-their-code-took-FIXED-20260928`). A hold left
// behind makes a `took` line false (the other thread is a daemon and the join
// times out); a hold released twice throws IllegalMonitorStateException.
//
// The page's exact abandon path (a compiled callee whose trap its call site
// declines, so the caller's reason-9 pad exits with no exception pending)
// cannot be forced from Java; this probe drives the reachable neighbours of it
// through the same frames. The deciding evidence is the jit crate test
// `x64::tests::w26_single_pass_frames_name_*`.
//
// Run on CratonVM (the first line keeps every method with an exception table,
// i.e. every synchronized block, on the single-pass tier):
//   CRATONVM_JIT_NO_EXC_TABLE_C2=1 cratonvm --java-home <jdk25> -cp <dir> L2W26SinglePassLockAbandon
//   cratonvm --java-home <jdk25> -cp <dir> L2W26SinglePassLockAbandon
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L2W26SinglePassLockAbandon
//   cratonvm --java-home <jdk25> --compatible -cp <dir> L2W26SinglePassLockAbandon
// stdout must equal HotSpot 25's in all four. HotSpot 25 prints:
//   warm=200000
//   warm: other thread took the lock=true
//   thrown=1000 caught=1000
//   throws: other thread took the lock=true
//   mixed=150000
//   mixed: other thread took the lock=true
//   spin=1249999975000000
//   spin: other thread took the lock=true
//   nested=150000
//   nested: other thread took the outer lock=true
//   nested: other thread took the inner lock=true
//   main holds a lock=false
public class L2W26SinglePassLockAbandon {
    static final Object LOCK = new Object();
    static final Object OUTER = new Object();
    static final Object INNER = new Object();
    static int counter;
    static int caught;

    static final class Count implements Runnable {
        public void run() {
            counter++;
        }
    }

    static final class Thrower implements Runnable {
        public void run() {
            counter++;
            throw new IllegalStateException("from inside the block");
        }
    }

    static final class Twice implements Runnable {
        public void run() {
            counter += 2;
        }
    }

    static void guarded(Object lock, Runnable r) {
        try {
            synchronized (lock) {
                r.run();
            }
        } catch (RuntimeException e) {
            caught++;
        }
    }

    static void nested(Object a, Object b, Runnable r) {
        synchronized (a) {
            synchronized (b) {
                r.run();
            }
        }
    }

    static long spin(Object lock, int n) {
        long s = 0;
        synchronized (lock) {
            for (int i = 0; i < n; i++) {
                s += i;
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
        for (int i = 0; i < 200_000; i++) {
            guarded(LOCK, count);
        }
        System.out.println("warm=" + counter);
        System.out.println("warm: other thread took the lock=" + otherTakes(LOCK));

        counter = 0;
        Runnable thrower = new Thrower();
        for (int i = 0; i < 1_000; i++) {
            guarded(LOCK, thrower);
        }
        System.out.println("thrown=" + counter + " caught=" + caught);
        System.out.println("throws: other thread took the lock=" + otherTakes(LOCK));

        counter = 0;
        Runnable twice = new Twice();
        for (int i = 0; i < 100_000; i++) {
            guarded(LOCK, (i & 1) == 0 ? count : twice);
        }
        System.out.println("mixed=" + counter);
        System.out.println("mixed: other thread took the lock=" + otherTakes(LOCK));

        System.out.println("spin=" + spin(LOCK, 50_000_000));
        System.out.println("spin: other thread took the lock=" + otherTakes(LOCK));

        counter = 0;
        for (int i = 0; i < 100_000; i++) {
            nested(OUTER, INNER, (i & 1) == 0 ? count : twice);
        }
        System.out.println("nested=" + counter);
        System.out.println("nested: other thread took the outer lock=" + otherTakes(OUTER));
        System.out.println("nested: other thread took the inner lock=" + otherTakes(INNER));
        System.out.println("main holds a lock="
                + (Thread.holdsLock(LOCK) || Thread.holdsLock(OUTER) || Thread.holdsLock(INNER)));
    }
}
