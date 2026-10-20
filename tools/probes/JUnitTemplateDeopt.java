/**
 * Runs one JUnit Platform method selector and prints every container/test result. Repro for
 * docs/internal/fixed-bugs/junit-test-template-container-fails-with-precise-deoptimization-unavailable-FIXED-20261011.md:
 *   java ... JUnitTemplateDeopt io.netty.handler.ssl.JdkSslEngineTest testBeginHandshakeCloseOutbound 'io.netty.handler.ssl.SSLEngineTest$SSLEngineTestParam'
 * (classpath: the netty-suite-runner common args). HotSpot: 12 tests SUCCESSFUL; CratonVM: the template container FAILED.
 */
import org.junit.platform.launcher.*; import org.junit.platform.launcher.core.*; import org.junit.platform.engine.*; import org.junit.platform.engine.discovery.DiscoverySelectors; import java.util.*;
public class JUnitTemplateDeopt { public static void main(String[] a) throws Exception {
  LauncherDiscoveryRequest r=LauncherDiscoveryRequestBuilder.request().selectors(DiscoverySelectors.selectMethod(a[0], a[1], a[2])).build();
  TestPlan plan=LauncherFactory.create().discover(r); System.out.println("planTests="+plan.countTestIdentifiers(TestIdentifier::isTest)+" containers="+plan.countTestIdentifiers(TestIdentifier::isContainer));
  TestExecutionListener l=new TestExecutionListener(){ public void executionFinished(TestIdentifier t, TestExecutionResult res){ System.out.println((t.isTest()?"T ":"C ")+res.getStatus()+" "+t.getDisplayName()+(res.getThrowable().isPresent()?" "+res.getThrowable().get():"")); } public void executionSkipped(TestIdentifier t,String reason){ System.out.println("SKIP "+t.getDisplayName()+" "+reason);} };
  LauncherFactory.create().execute(plan,l); System.exit(0); } }
