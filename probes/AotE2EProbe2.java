import java.io.PrintWriter;
import java.util.ArrayList;
import java.util.List;

import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherConfig;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;
import org.junit.platform.launcher.listeners.TestExecutionSummary;

import org.springframework.aot.AotDetector;
import org.springframework.aot.generate.InMemoryGeneratedFiles;
import org.springframework.aot.hint.RuntimeHints;
import org.springframework.aot.test.generate.CompilerFiles;
import org.springframework.context.aot.AbstractAotProcessor;
import org.springframework.core.test.tools.TestCompiler;
import org.springframework.test.context.aot.TestContextAotGenerator;

import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClasses;

/**
 * AotIntegrationTests#runEndToEndTests, cut down to the classes named on the
 * command line: AOT process -> compile the generated sources -> re-run the same
 * classes with spring.aot.enabled=true. Seconds instead of the ~15 minutes the
 * full 175-class endToEndTestsForBeanOverrides needs.
 */
public class AotE2EProbe2 {
    public static void main(String[] args) throws Exception {
        List<Class<?>> testClasses = new ArrayList<>();
        for (String name : args) {
            testClasses.add(Class.forName(name));
        }
        InMemoryGeneratedFiles generatedFiles = new InMemoryGeneratedFiles();
        TestContextAotGenerator generator =
                new TestContextAotGenerator(generatedFiles, new RuntimeHints(), true);
        try {
            System.setProperty(AbstractAotProcessor.AOT_PROCESSING, "true");
            generator.processAheadOfTime(testClasses.stream());
            System.out.println("PROBE aot-processing OK");
        } finally {
            System.clearProperty(AbstractAotProcessor.AOT_PROCESSING);
        }
        TestCompiler.forSystem().with(CompilerFiles.from(generatedFiles)).compile(compiled -> {
            try {
                System.setProperty(AotDetector.AOT_ENABLED, "true");
                LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
                        .selectors(selectClasses(testClasses)).build();
                SummaryGeneratingListener listener = new SummaryGeneratingListener();
                // Only the Jupiter engine, explicitly: ServiceLoader-based engine
                // discovery runs under the FORKED TCCL and ends up splitting some
                // engines package-private helpers across the two loaders
                // (IllegalAccessError in SuiteTestEngine / the TestNG engine).
                LauncherConfig config = LauncherConfig.builder()
                        .enableTestEngineAutoRegistration(false)
                        .addTestEngines(new org.junit.jupiter.engine.JupiterTestEngine())
                        .enablePostDiscoveryFilterAutoRegistration(false)
                        .enableLauncherSessionListenerAutoRegistration(false)
                        .enableLauncherDiscoveryListenerAutoRegistration(false)
                        .build();
                LauncherFactory.create(config).execute(request, listener);
                TestExecutionSummary s = listener.getSummary();
                System.out.println("PROBE RESULT found=" + s.getTestsFoundCount()
                        + " succ=" + s.getTestsSucceededCount()
                        + " fail=" + s.getTotalFailureCount());
                for (TestExecutionSummary.Failure f : s.getFailures()) {
                    Throwable t = f.getException();
                    System.out.println("PROBE FAIL " + f.getTestIdentifier().getDisplayName()
                            + " :: " + (t == null ? "?" : t.getClass().getName() + ": "
                            + String.valueOf(t.getMessage()).replace('\n', ' ')));
                    if (t != null && System.getenv("PROBE_STACK") != null) {
                        t.printStackTrace(System.out);
                    }
                }
                if (System.getenv("PROBE_SUMMARY") != null) {
                    s.printTo(new PrintWriter(System.out));
                }
            } finally {
                System.clearProperty(AotDetector.AOT_ENABLED);
            }
        });
        System.out.println("PROBE DONE");
    }
}
