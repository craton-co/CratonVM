import org.junit.Rule;
import org.junit.rules.TestRule;
import org.junit.runners.model.TestClass;

import java.util.List;

// TOMCAT-DOHEAD-JUNIT-ITERATOR.1 repro: the ban's own note says compiling
// org.junit.runners.model.TestClass.collectAnnotatedMethodValues corrupts
// its enhanced-for iterator local (observed as `i$` going null), causing a
// runner to leak failures until OutOfMemoryError. This method is the one
// BlockJUnit4ClassRunner.getTestRules() calls via
// `getAnnotatedMethodValues(target, Rule.class, TestRule.class)` to
// reflectively invoke every `@Rule`-annotated METHOD on a real target
// instance and collect the returned TestRule values -- an enhanced-for
// loop over the annotated-method list is exactly what it does internally.
// This probe drives that exact call path directly and repeatedly, with
// several @Rule methods per target so each call has real multi-element
// iteration work, first through natural warmup and then with
// CRATONVM_JIT_THRESHOLD=1 for forced eager/aggressive compilation.
public class TomcatDoheadJunitIteratorProbe {

    public static class WithSeveralRuleMethods {
        @Rule
        public TestRule rule1() {
            return (base, description) -> base;
        }

        @Rule
        public TestRule rule2() {
            return (base, description) -> base;
        }

        @Rule
        public TestRule rule3() {
            return (base, description) -> base;
        }

        @Rule
        public TestRule rule4() {
            return (base, description) -> base;
        }

        @Rule
        public TestRule rule5() {
            return (base, description) -> base;
        }
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        TestClass tc = new TestClass(WithSeveralRuleMethods.class);
        long totalRules = 0;
        for (int i = 0; i < iterations; i++) {
            WithSeveralRuleMethods target = new WithSeveralRuleMethods();
            List<TestRule> rules = tc.getAnnotatedMethodValues(target, Rule.class, TestRule.class);
            if (rules.size() != 5) {
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- expected 5 TestRule values, got " + rules.size());
                System.exit(1);
            }
            for (TestRule r : rules) {
                if (r == null) {
                    System.out.println("RESULT: FAIL at iteration " + i
                            + " -- null TestRule in result list (iterator corruption)");
                    System.exit(1);
                }
            }
            totalRules += rules.size();
        }
        System.out.println("RESULT: OK -- " + iterations
                + " getAnnotatedMethodValues(Rule.class) calls, " + totalRules
                + " total rule values collected, all consistent");
    }
}
