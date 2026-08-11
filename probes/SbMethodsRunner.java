import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;
import org.junit.platform.engine.DiscoverySelector;
import static org.junit.platform.engine.discovery.DiscoverySelectors.selectMethod;

import java.io.PrintWriter;
import java.util.ArrayList;
import java.util.List;

/**
 * Run an ORDERED subset of one test class's methods in one JVM.
 *
 * `SbRunnerMethod` takes exactly one method and `SbRunner` takes the whole
 * class, so neither can answer "does method A poison method B?" — the question
 * a test that passes alone and fails in-class always raises. This runner takes
 * the class and then any number of method names, and selects them in the order
 * given.
 *
 * Usage: {@code SbMethodsRunner <fqcn> <method>...}
 */
public class SbMethodsRunner {
    public static void main(String[] args) throws Exception {
        String fqcn = args[0];
        List<DiscoverySelector> selectors = new ArrayList<>();
        for (int i = 1; i < args.length; i++) {
            selectors.add(selectMethod(fqcn, args[i]));
        }
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
                .selectors(selectors).build();
        Launcher launcher = LauncherFactory.create();
        SummaryGeneratingListener sl = new SummaryGeneratingListener();
        launcher.registerTestExecutionListeners(sl);
        launcher.execute(req);
        TestExecutionSummary s = sl.getSummary();
        PrintWriter pw = new PrintWriter(System.out);
        for (TestExecutionSummary.Failure failure : s.getFailures()) {
            pw.println("SBRUNNER_FAILURE_DETAIL " + failure.getTestIdentifier().getDisplayName());
            failure.getException().printStackTrace(pw);
        }
        pw.flush();
        System.out.println("SBRUNNER_RESULT"
                + " tests=" + (s.getTestsStartedCount())
                + " failed=" + s.getTestsFailedCount()
                + " aborted=" + s.getTestsAbortedCount()
                + " skipped=" + s.getTestsSkippedCount()
                + " containersFailed=" + s.getContainersFailedCount());
        long bad = s.getTestsFailedCount() + s.getContainersFailedCount();
        System.exit(bad > 0 ? 1 : 0);
    }
}
