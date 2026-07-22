import java.util.*;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.launcher.TestIdentifier;

/**
 * Per-test progress probe: prints @@START/@@END (flushed) for every test as it
 * runs, so a hang/infinite-loop can be attributed to the exact method.
 * Usage: HangProbe <fqcn>
 */
public class HangProbe {
    public static void main(String[] args) throws Exception {
        String fqcn = args[0];
        Class<?> c = Class.forName(fqcn, false, HangProbe.class.getClassLoader());
        Launcher launcher = LauncherFactory.create();
        launcher.registerTestExecutionListeners(new TestExecutionListener() {
            public void executionStarted(TestIdentifier id) {
                if (id.isTest()) { System.out.println("@@START " + id.getDisplayName() + " [" + id.getUniqueId() + "]"); System.out.flush(); }
            }
            public void executionFinished(TestIdentifier id, TestExecutionResult r) {
                if (id.isTest()) { System.out.println("@@END   " + id.getDisplayName() + " -> " + r.getStatus()); System.out.flush(); }
            }
        });
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
                .selectors(DiscoverySelectors.selectClass(c)).build();
        launcher.execute(req);
        System.out.println("@@ALLDONE");
        System.out.flush();
        System.exit(0);
    }
}
