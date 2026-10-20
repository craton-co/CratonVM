import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * nio21 wave 4 lane L1: witness for
 * docs/internal/fixed-bugs/r21-interrupt-field-and-flag-can-split-on-a-consume-race-FIXED-20261009.md.
 *
 * The VM keeps two records of a thread's interrupt status: the Java `Thread.interrupted` field
 * (what `isInterrupted()` / `interrupted()` read) and its own flag (what `sleep` / `wait` / `park`
 * poll). Each round races an interrupter thread against a target that consumes interrupts (through
 * `Thread.sleep(0)` and through `Thread.interrupted()`), then stops the interrupter and, with no
 * interrupt in flight, asks the two records the same question: `isInterrupted()` (the field) and
 * whether `Thread.sleep(0)` throws (the flag). On HotSpot they always agree; a disagreement is a
 * split. Counted in each direction.
 *
 * Output (conformance, deterministic on HotSpot):
 *   intr-race rounds=<R> field-only=0 flag-only=0
 *
 * Run:
 *   javac -d out tools/probes/Nio21InterruptRace.java
 *   java -cp out Nio21InterruptRace > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21InterruptRace > jo.txt
 *   cratonvm --compatible -cp out Nio21InterruptRace > co.txt
 * -Dnio21.rounds=N (default 2000), -Dnio21.burst=N interrupts per round (default 50).
 */
public class Nio21InterruptRace {
    public static void main(String[] a) throws Exception {
        int rounds = Integer.getInteger("nio21.rounds", 2000);
        int burst = Integer.getInteger("nio21.burst", 50);
        int fieldOnly = 0, flagOnly = 0;
        Thread me = Thread.currentThread();
        for (int r = 0; r < rounds; r++) {
            AtomicBoolean stop = new AtomicBoolean();
            CountDownLatch go = new CountDownLatch(1);
            Thread interrupter = new Thread(() -> {
                try { go.await(); } catch (InterruptedException e) { return; }
                for (int i = 0; i < burst && !stop.get(); i++) {
                    me.interrupt();
                    Thread.onSpinWait();
                }
            });
            interrupter.start();
            go.countDown();
            for (int i = 0; i < burst; i++) {
                if ((i & 1) == 0) {
                    try { Thread.sleep(0); } catch (InterruptedException e) { /* consumed */ }
                } else {
                    Thread.interrupted();
                }
            }
            stop.set(true);
            // nio21 wave 6 L1: the interrupter's last `me.interrupt()` can land
            // after the consume loop, while (or just before) main waits here,
            // and `join` is interruptible: HotSpot's own run died with
            // `InterruptedException at Thread.join`. Join uninterruptibly; a
            // caught interrupt is consumed in both records at once (the throw
            // clears the status), so the quiescent check below stays valid.
            joinUninterruptibly(interrupter);
            // Quiescent: no interrupt is in flight. Both records must agree.
            boolean field = me.isInterrupted();
            boolean flag;
            try { Thread.sleep(0); flag = false; } catch (InterruptedException e) { flag = true; }
            if (field && !flag) fieldOnly++;
            if (!field && flag) flagOnly++;
            // Leave the round clean whatever happened.
            Thread.interrupted();
        }
        System.out.println("intr-race rounds=" + rounds + " field-only=" + fieldOnly + " flag-only=" + flagOnly);
    }

    static void joinUninterruptibly(Thread t) {
        while (true) {
            try {
                t.join();
                return;
            } catch (InterruptedException e) {
                // An in-flight interrupt from `t` itself; `t` is about to finish.
            }
        }
    }
}
