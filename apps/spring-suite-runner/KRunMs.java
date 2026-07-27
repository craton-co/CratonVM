import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import org.junit.platform.launcher.listeners.*;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import java.util.*;

/** Run a SEQUENCE of test methods, each in its own launcher pass, in argv order:
 *  KRunMs <fqcn> <method1> <method2> ... */
public class KRunMs {
    public static void main(String[] a) {
        String cls = a[0];
        for (int i = 1; i < a.length; i++) {
            LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
                .selectors(DiscoverySelectors.selectMethod(cls, a[i])).build();
            SummaryGeneratingListener l = new SummaryGeneratingListener();
            LauncherFactory.create().execute(req, l);
            var s = l.getSummary();
            System.out.println("STEP " + a[i] + " found=" + s.getTestsFoundCount()
                + " succ=" + s.getTestsSucceededCount() + " fail=" + s.getTestsFailedCount());
            s.getFailures().forEach(f -> System.out.println("   FAIL :: " + f.getException()));
        }
    }
}
