import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.nio.charset.Charset;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

/**
 * Regression probe for {@code java.lang.Process}'s JDK 17+ reader/writer
 * caches on a CratonVM-spawned process.
 *
 * <p>CratonVM hands {@code ProcessBuilder.start()} a synthetic Process whose
 * class chain does not reach {@code java.lang.Process}; the link is recorded
 * as a supertype only, for {@code checkcast}/{@code instanceof}. The final
 * concrete methods {@code inputReader()} / {@code errorReader()} /
 * {@code outputWriter()} nevertheless run their REAL bytecode against that
 * receiver, and that bytecode reads the six instance fields
 * {@code java.lang.Process} declares for itself (outputWriter, outputCharset,
 * inputReader, inputCharset, errorReader, errorCharset) at absolute slots
 * 0..5. The synthetic Process used to keep its own state (exit code, pipe
 * fds, pid, handle) at exactly those slots, so:
 *
 * <pre>
 * java.lang.NullPointerException: Cannot invoke "java.nio.charset.Charset.equals(Object)"
 *         because "this.inputCharset" is null
 *         at java.lang.Process.inputReader(Process.java:338)
 * </pre>
 *
 * — the stdout pipe fd read back as a non-null {@code inputReader}, taking the
 * "reader already created" branch, whose {@code inputCharset} was still null.
 * That single defect failed all 9 {@code org.apache.tomcat.integration.httpd.*}
 * classes (TesterHttpd.start pumps httpd's output through
 * {@code p.inputReader()}), on a fixture where HotSpot passes all 9.
 *
 * <p>Prints one line per check and exits non-zero if any fails, so it works
 * as a plain pass/fail gate:
 *
 * <pre>
 * cratonvm.exe -cp &lt;dir&gt; ProcessReaderProbe
 * java         -cp &lt;dir&gt; ProcessReaderProbe   # HotSpot control
 * </pre>
 */
public class ProcessReaderProbe {

    private static final List<String> failures = new ArrayList<>();

    /**
     * Never block forever on a child. On a VM where {@code outputWriter()}
     * throws, the {@code sort} child below never sees EOF on stdin, so a plain
     * {@code waitFor()} hangs the probe rather than reporting the failure it
     * just found.
     */
    private static int reap(Process p) throws InterruptedException {
        if (p.waitFor(20, java.util.concurrent.TimeUnit.SECONDS)) {
            return p.exitValue();
        }
        p.destroyForcibly();
        p.waitFor(10, java.util.concurrent.TimeUnit.SECONDS);
        return -1;
    }

    private static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "PASS " : "FAIL ") + what + (detail.isEmpty() ? "" : " -- " + detail));
        if (!ok) {
            failures.add(what);
        }
    }

    /** A command that writes a known line to stdout and another to stderr. */
    private static ProcessBuilder echoBoth() {
        boolean windows = System.getProperty("os.name", "").toLowerCase().contains("win");
        if (windows) {
            return new ProcessBuilder("cmd", "/c", "echo OUT& echo ERR 1>&2");
        }
        return new ProcessBuilder("sh", "-c", "echo OUT; echo ERR 1>&2");
    }

    public static void main(String[] args) throws Exception {
        // 1. inputReader() must not throw, and must read the child's stdout.
        Process p = echoBoth().start();
        String out = null;
        try {
            BufferedReader r = p.inputReader();
            check("inputReader() returns non-null", r != null, "");
            out = r.readLine();
        } catch (Throwable t) {
            check("inputReader() does not throw", false, t.toString());
        }
        check("inputReader() reads the child's stdout", out != null && out.trim().equals("OUT"),
                "got " + out);

        // 2. Repeated calls must return the SAME reader (the whole point of the
        //    cache fields), not a second one over an already-drained pipe.
        try {
            check("inputReader() is idempotent", p.inputReader() == p.inputReader(), "");
        } catch (Throwable t) {
            check("inputReader() is idempotent", false, t.toString());
        }

        // 3. Asking for a different charset after the reader exists is a
        //    documented IllegalStateException - reachable only when
        //    inputCharset was actually stored.
        boolean ise = false;
        try {
            Charset other = StandardCharsets.UTF_16;
            if (other.equals(Charset.forName(System.getProperty("native.encoding", "UTF-8")))) {
                other = StandardCharsets.ISO_8859_1;
            }
            p.inputReader(other);
        } catch (IllegalStateException expected) {
            ise = true;
        } catch (Throwable t) {
            check("inputReader(other charset) throws IllegalStateException", false, t.toString());
        }
        check("inputReader(other charset) throws IllegalStateException", ise, "");
        reap(p);

        // 4. errorReader() over the child's stderr.
        Process p2 = echoBoth().start();
        String err = null;
        try {
            err = p2.errorReader().readLine();
        } catch (Throwable t) {
            check("errorReader() does not throw", false, t.toString());
        }
        check("errorReader() reads the child's stderr", err != null && err.trim().equals("ERR"),
                "got " + err);
        reap(p2);

        // 5. outputWriter() over the child's stdin. `sort` echoes its input
        //    back once stdin closes, which proves the write landed.
        boolean windows = System.getProperty("os.name", "").toLowerCase().contains("win");
        ProcessBuilder pb = windows ? new ProcessBuilder("cmd", "/c", "sort")
                                    : new ProcessBuilder("sort");
        Process p3 = pb.start();
        String echoed = null;
        try {
            BufferedWriter w = p3.outputWriter();
            check("outputWriter() returns non-null", w != null, "");
            w.write("ZZTOP");
            w.newLine();
            w.flush();
            w.close();
            echoed = p3.inputReader().readLine();
        } catch (Throwable t) {
            check("outputWriter() does not throw", false, t.toString());
        }
        check("outputWriter() reaches the child's stdin",
                echoed != null && echoed.trim().equals("ZZTOP"), "got " + echoed);
        reap(p3);

        // 6. The synthetic Process's own state must still be intact - the fix
        //    moved those slots, so a stale reader would show up here.
        Process p4 = echoBoth().start();
        int rc = reap(p4);
        check("waitFor() still returns the real exit code", rc == 0, "got " + rc);
        check("pid() still returns a plausible pid", p4.pid() > 0, "got " + p4.pid());
        check("isAlive() is false after waitFor()", !p4.isAlive(), "");

        if (failures.isEmpty()) {
            System.out.println("ALL CHECKS PASSED");
        } else {
            System.out.println("FAILED CHECKS: " + failures);
            System.exit(1);
        }
    }
}
