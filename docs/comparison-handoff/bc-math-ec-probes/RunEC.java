// Driver that runs the EC suite, flushes, and surfaces any throwable to stderr
// (junit.textui.TestRunner swallows output to System.out which is lost on an
// abnormal CratonVM exit).
public class RunEC {
    public static void main(String[] a) {
        try {
            junit.framework.TestResult r =
                junit.textui.TestRunner.run(org.bouncycastle.math.ec.test.AllTests.suite());
            System.out.flush();
            System.err.println("DONE tests=" + r.runCount()
                + " failures=" + r.failureCount() + " errors=" + r.errorCount());
            java.util.Enumeration e = r.errors();
            while (e.hasMoreElements()) {
                Object te = e.nextElement();
                System.err.println("ERROR: " + te);
            }
            java.util.Enumeration f = r.failures();
            while (f.hasMoreElements()) {
                System.err.println("FAIL: " + f.nextElement());
            }
            System.err.flush();
        } catch (Throwable t) {
            System.err.println("THROWN in main: " + t);
            t.printStackTrace();
            System.err.flush();
        }
    }
}
