/*
 * Interpreter round i1, wave 4, lane L5: `ReferenceQueue.remove()` blocks until
 * a reference arrives or the caller is interrupted, and never returns null.
 *
 * Root cause of the intermittent `Exception in thread "Finalizer"
 * java.lang.NullPointerException: Cannot invoke
 * "java.lang.ref.Finalizer.runFinalizer(...)"`: CratonVM's native `remove()`
 * gave up after 60 seconds and returned null, and the JDK's Finalizer thread
 * dereferences what `remove()` returns. Any run longer than a minute lost its
 * Finalizer thread. The same native also ignored `Thread.interrupt()`.
 *
 * Default mode (fast, deterministic). HotSpot 25 prints exactly:
 *
 *   parkedAlive=true
 *   interruptedRemove=InterruptedException
 *   timedEmpty=null
 *   deliveredWhileInterrupted=true
 *   interruptStatusKept=true
 *
 * CratonVM before the fix printed `interruptedRemove=still-parked` (the
 * interrupt was ignored; the thread would have returned null 60 s later).
 *
 * MODE: run it with `--jdk-only`. The merged fix (dev's W2-F-1, which the
 * interpreter round adopted) applies there only; `--compatible` keeps the
 * historic loop byte-for-byte per AGENTS.md and still prints `still-parked`.
 *
 * `long` mode (pass the argument `long`; takes ~65 s, not for the default
 * runner) additionally prints, on HotSpot:
 *
 *   afterMinute.finalizerAlive=true
 *   afterMinute.removerParked=true
 *
 * CratonVM before the fix printed `false` / `false` there, plus the Finalizer
 * NPE on stderr (and the remover thread's null on stdout as
 * `remover.got=null`).
 */
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

public class RefQueueRemoveBlocks {
    static volatile String removerOutcome = "none";

    public static void main(String[] args) throws Exception {
        boolean longMode = args.length > 0 && args[0].equals("long");

        // 1. An untimed remove() parks, and an interrupt ends it with
        //    InterruptedException.
        final ReferenceQueue<Object> q1 = new ReferenceQueue<>();
        final String[] outcome = {"still-parked"};
        Thread t = new Thread(() -> {
            try {
                Reference<?> r = q1.remove();
                outcome[0] = "returned:" + r;
            } catch (InterruptedException e) {
                outcome[0] = "InterruptedException";
            }
        });
        t.setDaemon(true);
        t.start();
        Thread.sleep(200);
        System.out.println("parkedAlive=" + t.isAlive());
        t.interrupt();
        t.join(5000);
        System.out.println("interruptedRemove=" + outcome[0]);

        // 2. A timed remove() on an empty queue times out with null.
        ReferenceQueue<Object> q2 = new ReferenceQueue<>();
        System.out.println("timedEmpty=" + q2.remove(50));

        // 3. remove() polls before it waits: an interrupted caller still gets
        //    a reference that is already queued, and keeps its interrupt.
        ReferenceQueue<Object> q3 = new ReferenceQueue<>();
        Object referent = new Object();
        WeakReference<Object> w = new WeakReference<>(referent, q3);
        w.enqueue();
        Thread.currentThread().interrupt();
        Reference<?> got = q3.remove();
        System.out.println("deliveredWhileInterrupted=" + (got == w));
        System.out.println("interruptStatusKept=" + Thread.interrupted());
        if (referent.hashCode() == 42) {
            System.err.println("(keep referent reachable)");
        }

        if (!longMode) {
            return;
        }

        // 4. Past the old 60-second cap: the Finalizer thread is still alive,
        //    and a thread parked in remove() since the start is still parked.
        final ReferenceQueue<Object> q4 = new ReferenceQueue<>();
        Thread remover = new Thread(() -> {
            try {
                Reference<?> r = q4.remove();
                removerOutcome = r == null ? "null" : "ref";
                System.out.println("remover.got=" + removerOutcome);
            } catch (InterruptedException e) {
                removerOutcome = "interrupted";
            }
        });
        remover.setDaemon(true);
        remover.start();
        Thread.sleep(65_000);
        boolean finalizerAlive = false;
        for (Thread th : Thread.getAllStackTraces().keySet()) {
            if (th.getName().equals("Finalizer") && th.isAlive()) {
                finalizerAlive = true;
            }
        }
        System.out.println("afterMinute.finalizerAlive=" + finalizerAlive);
        System.out.println("afterMinute.removerParked=" + (remover.isAlive() && removerOutcome.equals("none")));
    }
}
