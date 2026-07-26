package jitprobe;

import org.springframework.beans.factory.support.DefaultListableBeanFactory;
import org.springframework.context.annotation.ClassPathBeanDefinitionScanner;

// SPB.9c (Session 114) repro: the ban's own note describes the real
// insurance-backend component-scan critical path --
// SpringApplication.run -> AbstractApplicationContext.refresh ->
// ConfigurationClassPostProcessor.processConfigBeanDefinitions ->
// ConfigurationClassParser.parse -> ClassPathBeanDefinitionScanner.doScan
// -> ClassPathScanningCandidateComponentProvider.scanCandidateComponents
// -> PathMatchingResourcePatternResolver.getResources -- each step doing
// putfield-heavy allocations (SourceClass.<init>, Resource[] via
// aastore). This probe drives the exact same real machinery directly:
// ClassPathBeanDefinitionScanner.scan(String... basePackages), which
// internally performs the full doScan -> scanCandidateComponents ->
// PathMatchingResourcePatternResolver.getResources chain against a real
// classpath package containing several real @Component-annotated
// classes, repeated many times to exercise the exact JIT-eligible code
// path under real, repeated use.
public class ComponentScanProbe {

    // Package with several real @Component classes to scan repeatedly --
    // this class's own package (componentscanprobe.components) is used
    // as the real scan target, giving the resolver real multi-file
    // classpath resources to enumerate each call.
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 500;
        String basePackage = "jitprobe.components";
        int expectedFound = -1;
        int expectedTotal = -1;
        for (int i = 0; i < iterations; i++) {
            DefaultListableBeanFactory factory = new DefaultListableBeanFactory();
            ClassPathBeanDefinitionScanner scanner = new ClassPathBeanDefinitionScanner(factory, true);
            int found = scanner.scan(basePackage);
            int total = factory.getBeanDefinitionCount();
            if (found < 5) {
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- expected at least 5 scanned components (the 5 real @Component/@Service/@Repository/@Configuration classes), got "
                        + found);
                System.exit(1);
            }
            if (expectedFound == -1) {
                expectedFound = found;
                expectedTotal = total;
            } else if (found != expectedFound || total != expectedTotal) {
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- inconsistent scan results: found=" + found + " (expected " + expectedFound
                        + "), total=" + total + " (expected " + expectedTotal + ")");
                System.exit(1);
            }
        }
        System.out.println("RESULT: OK -- " + iterations
                + " real ClassPathBeanDefinitionScanner.scan() cycles, all consistent");
    }
}
