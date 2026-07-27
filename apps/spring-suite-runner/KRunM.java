import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import org.junit.platform.launcher.listeners.*;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.engine.TestExecutionResult;

/** Run ONE test method: KRunM <fqcn> <method> */
public class KRunM {
    public static void main(String[] a) {
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
            .selectors(DiscoverySelectors.selectMethod(a[0], a[1])).build();
        Launcher launcher = LauncherFactory.create();
        SummaryGeneratingListener l = new SummaryGeneratingListener();
        launcher.execute(req, l);
        var s = l.getSummary();
        System.out.println("RESULT " + a[0] + "#" + a[1] + " found=" + s.getTestsFoundCount()
            + " succ=" + s.getTestsSucceededCount() + " fail=" + s.getTestsFailedCount());
        s.getFailures().forEach(f -> { System.out.println("FAIL " + f.getTestIdentifier().getDisplayName() + " :: " + f.getException()); f.getException().printStackTrace(System.out); });
    }
}
