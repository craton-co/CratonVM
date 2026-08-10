import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.TestIdentifier;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;
import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;

import java.io.PrintWriter;
import java.util.ArrayList;
import java.util.List;

/**
 * Universal single-class runner for the Spring Boot suite, mirroring
 * apps/keycloak/kc-runner/KcRunner.java. Uses the JUnit Platform launcher's
 * default ServiceLoader-based engine auto-registration so it runs BOTH
 * JUnit 5 (jupiter) and JUnit 4 (vintage, if present on the classpath) test
 * classes uniformly. Prints a machine-parseable result line for the harness.
 */
public class SbRunner {

    /**
     * `SummaryGeneratingListener` counts aborted and skipped tests but keeps no
     * reason for either -- `getSummary().getFailures()` covers failures only.
     * So a class whose only non-pass was an `Assumptions.abort(...)` -- the
     * JUnit-sanctioned way for a test to declare the host cannot run it --
     * reached the suite log as a bare `aborted=1` with no explanatory text
     * anywhere in `.out.log` or `.err.log`, and could only be triaged by
     * reading the upstream test source. `ApplicationTempTests` cost a
     * hand-written known-issues doc for exactly that reason: its abort message
     * already said "Symlink creation not supported", and the harness dropped
     * it on the floor.
     *
     * Skips get the same treatment: a `@DisabledOnOs(OS.WINDOWS)` carries its
     * reason in the `executionSkipped` callback, and printing it means a
     * `skipped=1` no longer has to be reconciled against the test source by
     * hand either.
     */
    static final class OutcomeReasonCollector implements TestExecutionListener {
        final List<String> aborted = new ArrayList<>();
        final List<String> skipped = new ArrayList<>();

        @Override
        public void executionFinished(TestIdentifier id, TestExecutionResult result) {
            if (!id.isTest() || result.getStatus() != TestExecutionResult.Status.ABORTED) { return; }
            String reason = result.getThrowable()
                    .map(t -> t.getClass().getName() + ": " + t.getMessage())
                    .orElse("(aborted with no throwable reported)");
            aborted.add(id.getDisplayName() + " : " + reason);
        }

        @Override
        public void executionSkipped(TestIdentifier id, String reason) {
            if (!id.isTest()) { return; }
            skipped.add(id.getDisplayName() + " : " + reason);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) { System.out.println("SBRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=0 NOARG"); System.exit(2); }
        String fqcn = args[0];
        Class<?> cls = null;
        try {
            cls = Class.forName(fqcn, false, Thread.currentThread().getContextClassLoader());
        } catch (Throwable t) {
            System.out.println("SBRUNNER_LOAD_FAIL " + fqcn + " : " + t);
            t.printStackTrace(System.out);
            System.out.println("SBRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=1 LOADFAIL");
            System.exit(3);
        }
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
                .selectors(selectClass(cls)).build();
        Launcher launcher = LauncherFactory.create();
        SummaryGeneratingListener sl = new SummaryGeneratingListener();
        OutcomeReasonCollector reasons = new OutcomeReasonCollector();
        launcher.registerTestExecutionListeners(sl, reasons);
        launcher.execute(req);
        TestExecutionSummary s = sl.getSummary();
        PrintWriter pw = new PrintWriter(System.out);
        s.printTo(pw);
        s.printFailuresTo(pw, 200);
        // SummaryGeneratingListener prints the top-level
        // MultipleFailuresError but omits AssertJ's individual assertion
        // failures. Emit the original throwable too, including its suppressed
        // children, so a per-class suite log remains diagnostic.
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
