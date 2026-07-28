import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;

import java.io.PrintWriter;

/** Runs one JUnit 5 test class and prints machine-readable counts. */
public class Junit5One {
    public static void main(String[] args) {
        LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
                .selectors(DiscoverySelectors.selectClass(args[0]))
                .build();
        Launcher launcher = LauncherFactory.create();
        SummaryGeneratingListener listener = new SummaryGeneratingListener();
        launcher.execute(request, listener);
        TestExecutionSummary summary = listener.getSummary();
        PrintWriter out = new PrintWriter(System.out);
        System.out.println("TESTS_FOUND=" + summary.getTestsFoundCount());
        System.out.println("TESTS_SUCCEEDED=" + summary.getTestsSucceededCount());
        System.out.println("TESTS_FAILED=" + summary.getTestsFailedCount());
        System.out.println("TESTS_ABORTED=" + summary.getTestsAbortedCount());
        System.out.println("CONTAINERS_FAILED=" + summary.getContainersFailedCount());
        summary.printFailuresTo(out, 4);
        out.flush();
    }
}
