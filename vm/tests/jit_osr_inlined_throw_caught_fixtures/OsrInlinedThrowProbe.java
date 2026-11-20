import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;

/**
 * A hot loop whose callee chain (execute -> execute0 -> addTask -> offerTask ->
 * reject) throws once the executor is terminated. The loop is OSR-compiled with
 * those callees spliced in, and the `catch (Throwable)` around the call must
 * still run when the exception comes out of the inlined chain.
 */
public final class OsrInlinedThrowProbe {
    static final class Exec {
        volatile boolean terminated;
        final ConcurrentLinkedQueue<Runnable> q = new ConcurrentLinkedQueue<>();

        void reject() {
            throw new RejectedExecutionException("terminated");
        }

        boolean offerTask(Runnable r) {
            if (terminated) {
                reject();
            }
            q.offer(r);
            if (q.size() > 1000) {
                q.clear();
            }
            return true;
        }

        void addTask(Runnable r) {
            if (!offerTask(r)) {
                reject();
            }
        }

        void execute0(Runnable r) {
            addTask(r);
        }

        void execute(Runnable r) {
            execute0(r);
        }
    }

    public static void main(String[] args) throws Exception {
        final int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 120;
        final AtomicReference<Throwable> error = new AtomicReference<>();
        final AtomicBoolean started = new AtomicBoolean();
        final AtomicBoolean escaped = new AtomicBoolean();
        final Runnable task = () -> { };
        int bad = 0;
        for (int i = 0; i < rounds; i++) {
            final Exec ex = new Exec();
            Thread t = new Thread(() -> {
                try {
                    for (;;) {
                        ex.execute(task);
                        started.set(true);
                    }
                } catch (Throwable cause) {
                    error.set(cause);
                }
            });
            t.setUncaughtExceptionHandler((th, e) -> escaped.set(true));
            t.start();
            // Let the loop run long enough to be OSR-compiled in most rounds.
            do {
                Thread.yield();
            } while (!started.get());
            if (i % 2 == 0) {
                Thread.sleep(5);
            }
            ex.terminated = true;
            t.join();
            if (!(error.get() instanceof RejectedExecutionException) || escaped.get()) {
                bad++;
            }
            error.set(null);
            escaped.set(false);
            started.set(false);
        }
        System.out.println(bad == 0 ? "OSR-INLINED-THROW-OK" : "OSR-INLINED-THROW-BAD " + bad);
        if (bad != 0) {
            System.exit(1);
        }
    }
}
