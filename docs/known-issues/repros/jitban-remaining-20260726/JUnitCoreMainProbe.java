import org.junit.Test;
import org.junit.runner.JUnitCore;

// JUNIT.1 repro: org/junit/runner/JUnitCore.main is normally called once per
// process and always ends by calling System.exit(), so it cannot be looped
// within one process to build up invocation count the way most other
// probes in this sweep do -- matching the ban's own note that "main never
// reaches a compile threshold" under realistic single-shot usage. To
// exercise the exact banned method under JIT anyway, this probe IS
// JUnitCore.main's call site directly (one call per process), and is
// launched many times externally with CRATONVM_JIT_THRESHOLD=1 (forcing
// immediate JIT compilation on the very first invocation, bypassing the
// need for repeated calls) to see whether the JIT'd version of `main`
// still correctly reports pass/fail via its process exit code.
public class JUnitCoreMainProbe {

    public static class TrivialPassingTest {
        @Test
        public void alwaysPasses() {
            int x = 2 + 2;
            if (x != 4) {
                throw new AssertionError("arithmetic broke");
            }
        }
    }

    public static class HeavyAllocTest {
        @Test
        public void allocatesHeavilyThenPasses() {
            java.util.List<Object> hold = new java.util.ArrayList<>();
            for (int i = 0; i < 200000; i++) {
                hold.add(new byte[64]);
                if (i % 5000 == 0) hold.clear();
            }
        }
    }

    public static class TrivialFailingTest {
        @Test
        public void alwaysFails() {
            throw new AssertionError("intentional failure");
        }
    }

    public static void main(String[] args) throws Exception {
        // Delegate straight to the real, unmodified JUnitCore.main -- this
        // call IS the banned method, and it will call System.exit()
        // itself (0 on success, 1 on failure/error).
        JUnitCore.main(args);
    }
}
