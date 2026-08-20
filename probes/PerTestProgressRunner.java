import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;

import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.TestIdentifier;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;

import java.util.concurrent.ConcurrentHashMap;
import java.util.Map;

/**
 * A JUnit Platform launcher that prints a line when each individual test
 * STARTS, not only when the class finishes.
 *
 * The suite harness's `CratonRunner` prints `@@RESULT` once per class and a
 * `@@TESTFAIL` line per failure. Neither survives a run killed at the wall
 * cap, so a class that exceeds its budget produces no evidence at all about
 * WHICH of its tests was running — which is what
 * `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md` had
 * to work around by hand.
 *
 * `@@BEGIN` is flushed before the test body runs, so the last `@@BEGIN` with
 * no matching `@@END` names the test that was in flight when the process was
 * killed. Both go to stdout so a single redirect captures the sequence.
 *
 * Usage: `PerTestProgressRunner <fully.qualified.TestClass>...`
 */
public final class PerTestProgressRunner {
    private PerTestProgressRunner() {}

    public static void main(String[] args) {
        if (args.length == 0) {
            System.err.println("usage: PerTestProgressRunner <fully.qualified.TestClass>...");
            System.exit(2);
        }
        for (String className : args) {
            LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
                    .selectors(selectClass(className))
                    .build();
            Launcher launcher = LauncherFactory.create();
            launcher.registerTestExecutionListeners(new Progress());
            long startedAt = System.nanoTime();
            launcher.execute(request);
            System.out.printf("@@CLASSEND %s ms=%d%n",
                    className, (System.nanoTime() - startedAt) / 1_000_000L);
            System.out.flush();
        }
    }

    private static final class Progress implements TestExecutionListener {
        private final Map<String, Long> startedAt = new ConcurrentHashMap<>();

        @Override
        public void executionStarted(TestIdentifier id) {
            if (!id.isTest()) {
                return;
            }
            startedAt.put(id.getUniqueId(), System.nanoTime());
            System.out.println("@@BEGIN  " + id.getUniqueId());
            System.out.flush();
        }

        @Override
        public void executionFinished(TestIdentifier id, TestExecutionResult status) {
            if (!id.isTest()) {
                return;
            }
            Long began = startedAt.remove(id.getUniqueId());
            long ms = began == null ? -1 : (System.nanoTime() - began) / 1_000_000L;
            System.out.println("@@END    " + status.getStatus() + " " + ms + "ms " + id.getUniqueId());
            status.getThrowable().ifPresent(t ->
                    System.out.println("         " + t.getClass().getName() + ": " + t.getMessage()));
            System.out.flush();
        }
    }
}
