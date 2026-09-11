import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import org.junit.platform.launcher.listeners.*;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.launcher.TestIdentifier;

public class KStack {
  public static void main(String[] a) throws Exception {
    for (String cls : a) {
      System.out.println("BEGIN " + cls);
      try {
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
            .selectors(DiscoverySelectors.selectClass(Class.forName(cls))).build();
        Launcher launcher = LauncherFactory.create();
        launcher.registerTestExecutionListeners(new TestExecutionListener() {
          @Override public void executionFinished(TestIdentifier id, TestExecutionResult r) {
            if (r.getStatus() != TestExecutionResult.Status.SUCCESSFUL) {
              System.out.println("### FAILED: " + id.getDisplayName() + " [" + id.getUniqueId() + "]");
              r.getThrowable().ifPresent(t -> {
                Throwable c = t; int d = 0;
                while (c != null && d < 12) {
                  System.out.println((d==0?"THROWN: ":"CAUSED BY: ") + c.getClass().getName() + ": " + c.getMessage());
                  for (StackTraceElement e : c.getStackTrace()) System.out.println("    at " + e);
                  c = c.getCause(); d++;
                }
              });
            }
          }
        });
        launcher.execute(req);
      } catch (Throwable t) { System.out.println("LOADERR " + cls + " :: " + t); t.printStackTrace(System.out); }
      System.out.println("END " + cls);
    }
  }
}
