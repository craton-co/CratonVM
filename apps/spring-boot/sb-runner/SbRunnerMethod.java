import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;
import static org.junit.platform.engine.discovery.DiscoverySelectors.selectMethod;

import java.io.PrintWriter;

public class SbRunnerMethod {
    public static void main(String[] args) throws Exception {
        String fqcn = args[0];
        String method = args[1];
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
                .selectors(selectMethod(fqcn, method)).build();
        Launcher launcher = LauncherFactory.create();
        SummaryGeneratingListener sl = new SummaryGeneratingListener();
        // Same reason-capture as SbRunner: a single-method rerun of an aborting
        // test is exactly when the abort reason matters most, and the summary
        // listener does not carry one. See SbRunner.OutcomeReasonCollector.
        SbRunner.OutcomeReasonCollector reasons = new SbRunner.OutcomeReasonCollector();
        launcher.registerTestExecutionListeners(sl, reasons);
        launcher.execute(req);
        TestExecutionSummary s = sl.getSummary();
        PrintWriter pw = new PrintWriter(System.out);
        s.printTo(pw);
        s.printFailuresTo(pw, 200);
        for (TestExecutionSummary.Failure failure : s.getFailures()) {
            pw.println("SBRUNNER_FAILURE_DETAIL " + failure.getTestIdentifier().getDisplayName());
            failure.getException().printStackTrace(pw);
        }
        for (String line : reasons.aborted) { pw.println("SBRUNNER_ABORTED_DETAIL " + line); }
        for (String line : reasons.skipped) { pw.println("SBRUNNER_SKIPPED_DETAIL " + line); }
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
