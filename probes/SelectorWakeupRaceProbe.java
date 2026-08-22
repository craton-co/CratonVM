import java.nio.channels.Selector;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Does an indefinite Selector.select() always observe a concurrent wakeup()?
 *
 * netty's NioEventLoop blocks in select() with NO timeout whenever it has no
 * scheduled task, and relies solely on wakeup() to be released when another
 * thread submits work (bind/connect/close/write all do). A single lost wakeup
 * is therefore a PERMANENT hang, which is the shape ParameterizedSslHandlerTest
 * stalls in: a channel-operation promise that never completes while the reactor
 * sits in select().
 *
 * The selector thread reports each return by bumping `returns`. The driver
 * issues a wakeup and waits for that counter to move. If it does not move
 * within the grace period, the wakeup was lost.
 *
 * The delay before each wakeup is swept across the select-entry window rather
 * than fixed, because the race is between "select decided to block" and
 * "wakeup published the flag/byte".
 */
public class SelectorWakeupRaceProbe {
    static final AtomicLong returns = new AtomicLong();
    static final AtomicBoolean stop = new AtomicBoolean();

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        long graceMs = args.length > 1 ? Long.parseLong(args[1]) : 4000;
        Selector sel = Selector.open();

        Thread t = new Thread(() -> {
            while (!stop.get()) {
                try { sel.select(); } catch (Exception e) { break; }
                returns.incrementAndGet();
            }
        }, "selector-thread");
        t.setDaemon(true);
        t.start();

        Thread.sleep(200);
        long lost = 0;
        for (int i = 1; i <= iters; i++) {
            long before = returns.get();
            // Sweep the window: 0..~3us of spin, plus an occasional yield.
            int spin = i % 997;
            for (int s = 0; s < spin; s++) { Thread.onSpinWait(); }
            if ((i & 63) == 0) Thread.yield();

            sel.wakeup();

            long deadline = System.currentTimeMillis() + graceMs;
            boolean moved = false;
            while (System.currentTimeMillis() < deadline) {
                if (returns.get() > before) { moved = true; break; }
                Thread.onSpinWait();
            }
            if (!moved) {
                lost++;
                System.out.println("PROBE LOST_WAKEUP at iteration " + i
                        + " (returns stuck at " + before + ", spin=" + spin + ")");
                break;
            }
            if (i % 5000 == 0) System.out.println("PROBE ok " + i + " wakeups, returns=" + returns.get());
        }
        stop.set(true);
        sel.wakeup();
        System.out.println("PROBE DONE iters=" + iters + " lost=" + lost + " returns=" + returns.get());
        System.exit(lost == 0 ? 0 : 3);
    }
}
