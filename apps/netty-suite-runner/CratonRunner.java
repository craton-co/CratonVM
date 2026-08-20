import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;

import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;

/**
 * Small fixture-local launcher for CratonVM validation.  The source fixture is
 * intentionally ignored by the VM repository, so this runner is not a product
 * artifact and must never be staged with a VM fix.
 */
public final class CratonRunner {
    private CratonRunner() {}

    public static void main(String[] args) {
        if (args.length == 0) {
            System.err.println("usage: CratonRunner <fully.qualified.TestClass>...");
            System.exit(2);
        }
        int failures = 0;
        for (String className : args) {
            SummaryGeneratingListener summary = new SummaryGeneratingListener();
            LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
                    .selectors(selectClass(className))
                    .build();
            Launcher launcher = LauncherFactory.create();
            // Report each failure AS IT HAPPENS, not only through
            // printFailuresTo below. That call runs after the whole class
            // finishes, so a run killed at the harness's wall cap loses every
            // failure it had already collected — which is exactly what happened
            // to the two witnesses of
            // docs/known-issues/hibernate/gettypename-wrong-receiver-in-sessionfactory-rebuild-cascade-20260801.md:
            // they died mid-class and the logs never recorded what killed the
            // test that started the cascade. Goes to stderr, so `run-hib.sh`'s
            // stdout parsing of @@RESULT is unaffected.
            TestExecutionListener eager = new TestExecutionListener() {
                @Override
                public void executionFinished(org.junit.platform.launcher.TestIdentifier id,
                        org.junit.platform.engine.TestExecutionResult status) {
                    if (!id.isTest()
                            || status.getStatus() == org.junit.platform.engine.TestExecutionResult.Status.SUCCESSFUL) {
                        return;
                    }
                    System.err.println("@@TESTFAIL " + className + " " + id.getDisplayName()
                            + " " + status.getStatus());
                    status.getThrowable().ifPresent(t -> t.printStackTrace(System.err));
                    System.err.flush();
                }
            };
            launcher.registerTestExecutionListeners(new TestExecutionListener[] { summary, eager });
            long startedAt = System.nanoTime();
            launcher.execute(request);
            long elapsedMs = (System.nanoTime() - startedAt) / 1_000_000L;
            TestExecutionSummary result = summary.getSummary();
            long found = result.getTestsFoundCount();
            long started = result.getTestsStartedCount();
            long ok = result.getTestsSucceededCount();
            long failed = result.getTestsFailedCount();
            long aborted = result.getTestsAbortedCount();
            long skipped = result.getTestsSkippedCount();
            System.out.printf(
                    "@@RESULT %s found=%d started=%d ok=%d failed=%d aborted=%d skipped=%d ms=%d%n",
                    className, found, started, ok, failed, aborted, skipped, elapsedMs);
            if (failed != 0 || started != ok + failed + aborted) {
                failures++;
                result.printFailuresTo(new java.io.PrintWriter(System.err, true));
            }
        }
        System.out.println("@@BATCHEND failed_classes=" + failures);
        if (failures != 0) System.exit(1);
    }
}
