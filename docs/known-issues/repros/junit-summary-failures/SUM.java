import java.io.PrintWriter;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;

import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;
import static org.junit.platform.launcher.EngineFilter.includeEngines;

/**
 * `SummaryGeneratingListener` must not report a failure COUNT that its failure
 * LIST cannot supply.
 *
 * Spring's CompileWithForkedClassLoaderExtension.runTest ends with
 *
 *     if (summary.getTotalFailureCount() > 0) {
 *         throw summary.getFailures().get(0).getException();   // line 140
 *     }
 *
 * On CratonVM that line throws ArrayIndexOutOfBoundsException, which is what
 * `AotIntegrationTests.endToEndTests()` and `endToEndTestsForBeanOverrides()`
 * fail with (HotSpot: 4 found / 2 passed / 0 failed / 2 skipped; CratonVM:
 * 0 passed / 2 failed). An AIOOBE from `.get(0)` means the list is EMPTY while
 * the count is non-zero -- so the real failure of the nested run is destroyed
 * on its way out and never reported.
 *
 * This probe runs a nested JUnit Platform over a class with one passing and two
 * failing tests and compares the two accessors directly. On HotSpot the count
 * and the list agree.
 */
public class SUM {

    public static class Nested {
        @Test
        void passes() {
        }

        @Test
        void failsWithAssertion() {
            throw new AssertionError("deliberate assertion failure");
        }

        @Test
        void failsWithException() {
            throw new IllegalStateException("deliberate exception failure");
        }
    }

    public static void main(String[] args) {
        LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
                .selectors(selectClass(Nested.class))
                .filters(includeEngines("junit-jupiter"))
                .build();
        SummaryGeneratingListener listener = new SummaryGeneratingListener();
        Launcher launcher = LauncherFactory.create();
        launcher.execute(request, listener);
        TestExecutionSummary summary = listener.getSummary();

        long count = summary.getTotalFailureCount();
        List<TestExecutionSummary.Failure> failures = summary.getFailures();
        System.out.println("started=" + summary.getTestsStartedCount()
                + " succeeded=" + summary.getTestsSucceededCount()
                + " failed=" + summary.getTestsFailedCount());
        System.out.println("getTotalFailureCount()=" + count);
        System.out.println("getFailures().size()  =" + failures.size());

        if (count != failures.size()) {
            System.out.println("MISMATCH count=" + count + " list=" + failures.size());
        } else {
            System.out.println("AGREE");
        }

        // The exact expression Spring evaluates.
        if (count > 0) {
            try {
                Throwable t = failures.get(0).getException();
                System.out.println("get(0) OK -> " + t.getClass().getName() + ": " + t.getMessage());
            } catch (Throwable e) {
                System.out.println("get(0) THREW " + e.getClass().getName() + ": " + e.getMessage());
            }
        }

        for (TestExecutionSummary.Failure f : failures) {
            System.out.println("  failure: " + f.getTestIdentifier().getDisplayName()
                    + " -> " + f.getException());
        }
        summary.printFailuresTo(new PrintWriter(System.out), 2);
        System.out.println("SUMDONE");
    }
}
