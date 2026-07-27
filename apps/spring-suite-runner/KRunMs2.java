import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import org.junit.platform.launcher.listeners.*;
import org.junit.platform.engine.discovery.DiscoverySelectors;

public class KRunMs2 {
    static void show(String n) {
        try {
            Class<?> c = Class.forName(n);
            StringBuilder sb = new StringBuilder();
            for (var f : c.getDeclaredFields()) sb.append(f.getName()).append(' ');
            System.out.println("  [probe] " + n + " super=" + c.getSuperclass().getName() + " fields=[" + sb + "]");
        } catch (Throwable t) { System.out.println("  [probe] " + n + " -> " + t); }
    }
    public static void main(String[] a) {
        String cls = a[0], watch = a[1];
        show(watch);
        for (int i = 2; i < a.length; i++) {
            LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
                .selectors(DiscoverySelectors.selectMethod(cls, a[i])).build();
            SummaryGeneratingListener l = new SummaryGeneratingListener();
            LauncherFactory.create().execute(req, l);
            var s = l.getSummary();
            System.out.println("STEP " + a[i] + " succ=" + s.getTestsSucceededCount() + " fail=" + s.getTestsFailedCount());
            s.getFailures().forEach(f -> System.out.println("   FAIL :: " + f.getException()));
            show(watch);
        }
    }
}
