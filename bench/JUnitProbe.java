import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import org.junit.platform.launcher.listeners.*;
import static org.junit.platform.engine.discovery.DiscoverySelectors.*;

public class JUnitProbe {
    public static void main(String[] args) throws Exception {
        String cls = args.length > 0 ? args[0]
            : "org.apache.commons.math4.transform.TransformUtilsTest";
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
            .selectors(selectClass(cls))
            .build();
        Launcher launcher = LauncherFactory.create();
        TestPlan plan = launcher.discover(req);
        long found = plan.countTestIdentifiers(t -> t.isTest());
        System.out.println("DISCOVER_FOUND=" + found);
        for (TestIdentifier root : plan.getRoots()) {
            System.out.println("ROOT: " + root.getDisplayName() + " engine=" + root.getUniqueId());
            dump(plan, root, 1);
        }
        SummaryGeneratingListener l = new SummaryGeneratingListener();
        launcher.execute(req, l);
        var s = l.getSummary();
        System.out.println("EXEC_FOUND=" + s.getTestsFoundCount()
            + " SUCCEEDED=" + s.getTestsSucceededCount()
            + " FAILED=" + s.getTestsFailedCount());
    }
    static void dump(TestPlan plan, TestIdentifier id, int depth) {
        for (TestIdentifier c : plan.getChildren(id)) {
            System.out.println("  ".repeat(depth) + "- " + c.getDisplayName()
                + " test=" + c.isTest() + " container=" + c.isContainer());
            dump(plan, c, depth + 1);
        }
    }
}
