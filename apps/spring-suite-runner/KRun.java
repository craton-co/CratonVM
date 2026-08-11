import java.io.PrintStream;
import java.util.List;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.core.LauncherConfig;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;

/**
 * Minimal programmatic JUnit Platform launcher: run ONE test class per JVM and
 * print a single flushed RESULT line. One JVM per class (driven by the runner)
 * so a CratonVM crash/hang in one class cannot abort the others. The JUnit
 * ConsoleLauncher path is unreliable on CratonVM (0-byte reports, lost stdout on
 * System.exit), hence this hand-rolled launcher.
 *
 * Engine-agnostic: with junit-vintage-engine on the classpath this also runs
 * legacy JUnit4 (@RunWith) WildFly test classes; with junit-jupiter-engine it
 * runs Jupiter tests. Auto-registration of post-discovery filters / session /
 * discovery listeners is DISABLED so a framework's ServiceLoader extension
 * (Arquillian, WildFly) that fails to instantiate standalone cannot break every
 * class. The test engines themselves stay auto-registered.
 *
 * Usage: KRun <fully.qualified.TestClass> [<more classes...>]
 */
public class KRun {
    public static void main(String[] args) {
        PrintStream out = System.out;
        if (args.length < 1) { out.println("RESULT <none> status=NOARG"); out.flush(); return; }
        for (String cls : args) {
            runOne(out, cls);
        }
    }

    static void runOne(PrintStream out, String cls) {
        out.println("BEGIN " + cls); out.flush();
        long t0 = System.currentTimeMillis();
        try {
            Class<?> testClass = Class.forName(cls);
            LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
                    .selectors(DiscoverySelectors.selectClass(testClass))
                    .build();
            LauncherConfig config = LauncherConfig.builder()
                    .enablePostDiscoveryFilterAutoRegistration(false)
                    .enableLauncherSessionListenerAutoRegistration(false)
                    .enableLauncherDiscoveryListenerAutoRegistration(false)
                    .build();
            Launcher launcher = LauncherFactory.create(config);
            SummaryGeneratingListener listener = new SummaryGeneratingListener();
            launcher.registerTestExecutionListeners(listener);
            launcher.execute(request);
            TestExecutionSummary s = listener.getSummary();
            long found = s.getTestsFoundCount();
            long succ = s.getTestsSucceededCount();
            long skip = s.getTestsSkippedCount();
            long abort = s.getTestsAbortedCount();
            long fail = s.getTotalFailureCount();
            String status = (found == 0) ? "EMPTY"
                          : (fail == 0) ? "OK" : "FAIL";
            out.println("RESULT " + cls + " found=" + found + " succ=" + succ
                    + " fail=" + fail + " skip=" + skip + " abort=" + abort
                    + " ms=" + (System.currentTimeMillis() - t0)
                    + " status=" + status);
            out.flush();
            List<TestExecutionSummary.Failure> fails = s.getFailures();
            int shown = 0;
            for (TestExecutionSummary.Failure f : fails) {
                if (shown++ >= 5) break;
                Throwable t = f.getException();
                String msg = (t == null) ? "?" : (t.getClass().getName() + ": " + t.getMessage());
                out.println("FAILCAUSE " + cls + " :: " + f.getTestIdentifier().getDisplayName()
                        + " :: " + msg);
                if (t != null && System.getenv("KRUN_STACK") != null) {
                    t.printStackTrace(out);
                    out.flush();
                }
            }
            out.flush();
        } catch (Throwable t) {
            out.println("RESULT " + cls + " found=0 succ=0 fail=0 skip=0 abort=0"
                    + " ms=" + (System.currentTimeMillis() - t0) + " status=LOADERR");
            out.println("LOADERR " + cls + " :: " + t.getClass().getName() + ": " + t.getMessage());
            out.flush();
        }
    }
}
