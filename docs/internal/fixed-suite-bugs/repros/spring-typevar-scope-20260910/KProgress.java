import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.engine.TestExecutionResult;

public class KProgress {
  static long t0 = System.currentTimeMillis();
  static String ts() { return String.format("[%6.1fs]", (System.currentTimeMillis()-t0)/1000.0); }
  public static void main(String[] a) throws Exception {
    for (String cls : a) {
      System.out.println(ts() + " CLASS " + cls);
      LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
          .selectors(DiscoverySelectors.selectClass(Class.forName(cls))).build();
      Launcher launcher = LauncherFactory.create();
      launcher.registerTestExecutionListeners(new TestExecutionListener() {
        @Override public void executionStarted(TestIdentifier id) {
          if (id.isTest()) System.out.println(ts() + "   START  " + id.getDisplayName());
        }
        @Override public void executionFinished(TestIdentifier id, TestExecutionResult r) {
          if (id.isTest()) System.out.println(ts() + "   FINISH " + id.getDisplayName() + " -> " + r.getStatus()
              + (r.getThrowable().isPresent() ? " :: " + r.getThrowable().get() : ""));
        }
      });
      launcher.execute(req);
      System.out.println(ts() + " END " + cls);
    }
  }
}
