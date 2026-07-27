import junit.framework.Test;
import junit.framework.TestCase;
import junit.framework.TestSuite;

/**
 * Regression witness for the ONE class under the blanket `junit/` ban that has
 * its own documented crash history: `junit/textui/TestRunner.main`.
 *
 * The 2026-05-28 "bc-math-ec JUnit-3 AllTests SEGV" entry in
 * `vm/src/jit/skip_list.rs`'s `is_known_miscompile` list says JIT-compiling
 * `TestRunner.main`'s `new TestRunner; dup; invokespecial <init>; astore_1;
 * aload_1; invokevirtual start(args)` sequence left the next minor GC walker
 * reading object headers off byte[] data. That targeted entry is gated behind
 * `callee_saved_gpr_local_homes_enabled()` (default-OFF), so once the blanket
 * `junit/` package ban is removed, `TestRunner.main` becomes JIT-eligible by
 * default for the first time — this probe is what tests that directly.
 *
 * `main` runs once per process and ends in System.exit, so the probe drives it
 * one call per process and is launched repeatedly by the harness with
 * CRATONVM_JIT_THRESHOLD=1 (compile on the first invocation) and the `junit/`
 * package explicitly allowed. Allocation pressure inside the tests forces
 * several minor GCs while the JIT'd `main` frame is live — that is the exact
 * window the original SEGV opened in.
 */
public class JUnit3TextUiRunnerProbe extends TestCase {

    public JUnit3TextUiRunnerProbe(String name) {
        super(name);
    }

    public void testAllocatesUnderJitdMain() {
        java.util.List<Object> hold = new java.util.ArrayList<Object>();
        for (int i = 0; i < 300000; i++) {
            hold.add(new byte[64]);
            if (i % 4000 == 0) {
                hold.clear();
            }
        }
        assertTrue(hold.size() >= 0);
    }

    public void testStringChurnUnderJitdMain() {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 20000; i++) {
            sb.append(i % 10);
            if (sb.length() > 4096) {
                sb.setLength(0);
            }
        }
        assertEquals(4, 2 + 2);
    }

    public void testArithmeticStillCorrect() {
        long acc = 0;
        for (int i = 1; i <= 100000; i++) {
            acc += i;
        }
        assertEquals(5000050000L, acc);
    }

    public static Test suite() {
        TestSuite s = new TestSuite();
        s.addTest(new JUnit3TextUiRunnerProbe("testAllocatesUnderJitdMain"));
        s.addTest(new JUnit3TextUiRunnerProbe("testStringChurnUnderJitdMain"));
        s.addTest(new JUnit3TextUiRunnerProbe("testArithmeticStillCorrect"));
        return s;
    }
}
