package org.springframework.test.context.junit.jupiter.parallel;

import org.junit.jupiter.api.Constants;
import org.junit.platform.testkit.engine.EngineExecutionResults;
import org.junit.platform.testkit.engine.EngineTestKit;

import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;

/** args: parallel(on|off) [reps] -- runs the SAME nested TestCase the suite class runs. */
public class ParProbe {
  public static void main(String[] a) throws Exception {
    boolean parallel = a.length > 0 && a[0].equals("on");
    int reps = a.length > 1 ? Integer.parseInt(a[1]) : 1;
    for (int i = 1; i <= reps; i++) {
      long t0 = System.currentTimeMillis();
      EngineTestKit.Builder b = EngineTestKit.engine("junit-jupiter")
          .configurationParameter("junit.platform.discovery.issue.severity.critical", "INFO")
          .configurationParameter(Constants.DEACTIVATE_CONDITIONS_PATTERN_PROPERTY_NAME, "*DisabledCondition")
          .configurationParameter(Constants.PARALLEL_EXECUTION_ENABLED_PROPERTY_NAME, parallel ? "true" : "false");
      if (parallel) {
        b = b.configurationParameter(Constants.PARALLEL_CONFIG_DYNAMIC_FACTOR_PROPERTY_NAME, "10")
             .configurationParameter(Constants.PARALLEL_CONFIG_EXECUTOR_SERVICE_PROPERTY_NAME, "WORKER_THREAD_POOL");
      }
      EngineExecutionResults r = b.selectors(selectClass(ParallelExecutionSpringExtensionTests.TestCase.class)).execute();
      long ms = System.currentTimeMillis() - t0;
      long started = r.testEvents().started().count();
      long ok = r.testEvents().succeeded().count();
      System.out.printf("PARPROBE parallel=%s rep=%d started=%d succeeded=%d ms=%d%n", a.length>0?a[0]:"off", i, started, ok, ms);
    }
  }
}
