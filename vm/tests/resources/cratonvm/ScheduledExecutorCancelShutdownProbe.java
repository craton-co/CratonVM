package cratonvm;

import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;

/**
 * A cancelled delayed task must not keep a real scheduled executor alive after
 * shutdown. JUnit 6 uses this pattern for its same-thread timeout watchdog.
 */
public final class ScheduledExecutorCancelShutdownProbe {
    private ScheduledExecutorCancelShutdownProbe() {
    }

    public static void main(String[] args) throws Exception {
        ScheduledExecutorService executor = Executors.newSingleThreadScheduledExecutor(
                runnable -> new Thread(runnable, "scheduled-cancel-shutdown-probe"));
        try {
            ScheduledFuture<?> timeout = executor.schedule(
                    () -> { throw new AssertionError("cancelled timeout ran"); },
                    120, TimeUnit.SECONDS);
            // Let the real executor create its worker and enter the delayed queue.
            Thread.sleep(100);
            if (!timeout.cancel(false)) {
                throw new AssertionError("delayed timeout was not cancelled");
            }
        } finally {
            executor.shutdown();
        }
        if (!executor.awaitTermination(2, TimeUnit.SECONDS)) {
            throw new AssertionError("cancelled delayed task prevented termination");
        }
        System.out.println("SCHEDULED_CANCEL_SHUTDOWN_OK");
    }
}
