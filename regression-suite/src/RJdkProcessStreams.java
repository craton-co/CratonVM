import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;

/**
 * {@code Runtime.exec}/{@code ProcessBuilder}: the child {@code Process}'s
 * stdin/stdout/stderr streams must be real, live pipes to the OS process --
 * not stub objects that are merely non-null.
 *
 * CratonVM shipped a {@code Runtime.exec} whose returned {@code Process} had
 * streams that were present but not actually wired to anything: writes to
 * stdin never reached the child, and reads from stdout/stderr returned EOF
 * immediately regardless of what the child printed. See
 * {@code fixed-bugs/runtime-exec-returned-a-process-with-no-streams-FIXED-20260806.md}
 * -- traced to Apache Tomcat's own
 * {@code org.apache.tomcat.security.TestSecurity2019#testCVE_2019_0232}, a
 * real CVE regression test, not a synthetic probe. {@link RJdkProcess}, the
 * suite's other process-handling vector, deliberately never touches this
 * surface: every child it spawns has {@code redirectOutput}/{@code
 * redirectError} set to {@code DISCARD} (it is testing {@code
 * ProcessHandle}, not stream I/O), so the bug this vector guards had no
 * regression coverage until now.
 *
 * Every check here round-trips actual bytes through a pipe rather than just
 * asserting non-null: a stub stream that is non-null but reads/writes
 * nothing would pass a null-check and fail everything below it. Content is
 * compared trimmed, not byte-for-byte -- Windows console tools are free to
 * normalize line endings, and the property under test is "did the bytes
 * actually get there", not "was the newline convention preserved".
 */
public class RJdkProcessStreams {
    static int checks;
    static final int EXPECTED_CHECKS = 22;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static boolean windows() {
        return File.separatorChar == '\\';
    }

    /** A command that copies stdin to stdout, one line at a time, until EOF. */
    static List<String> catCmd() {
        return windows()
                ? Arrays.asList("cmd.exe", "/c", "sort")
                : Arrays.asList("/bin/cat");
    }

    /** Writes one fixed line to stdout, a different fixed line to stderr, then exits 0. */
    static List<String> stdoutAndStderrCmd() {
        return windows()
                ? Arrays.asList("cmd.exe", "/c", "echo OUT-LINE&echo ERR-LINE 1>&2")
                : Arrays.asList("/bin/sh", "-c", "echo OUT-LINE; echo ERR-LINE 1>&2");
    }

    static List<String> exitWithCode(int code) {
        return windows()
                ? Arrays.asList("cmd.exe", "/c", "exit " + code)
                : Arrays.asList("/bin/sh", "-c", "exit " + code);
    }

    static byte[] readAll(InputStream in) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[4096];
        int n;
        while ((n = in.read(buf)) != -1) {
            out.write(buf, 0, n);
        }
        return out.toByteArray();
    }

    static String trimmed(byte[] b) {
        return new String(b, StandardCharsets.UTF_8).trim();
    }

    static void stdinToStdoutRoundTrip() throws Exception {
        Process p = new ProcessBuilder(catCmd()).start();
        OutputStream stdin = p.getOutputStream();
        InputStream stdout = p.getInputStream();
        check(stdin != null, "stdin stream must not be null");
        check(stdout != null, "stdout stream must not be null");

        String payload = "hello-from-regression-suite";
        stdin.write((payload + System.lineSeparator()).getBytes(StandardCharsets.UTF_8));
        stdin.close(); // send EOF so the child terminates
        String echoed = trimmed(readAll(stdout));
        check(p.waitFor(30, TimeUnit.SECONDS), "cat child did not exit");
        check(p.exitValue() == 0, "cat child exit code");
        check(payload.equals(echoed),
                "stdin bytes must round-trip through stdout unchanged; sent [" + payload
                        + "], got back [" + echoed + "]");
    }

    static void stdoutAndStderrAreSeparateByDefault() throws Exception {
        ProcessBuilder pb = new ProcessBuilder(stdoutAndStderrCmd());
        pb.redirectErrorStream(false); // default, but explicit
        Process p = pb.start();
        p.getOutputStream().close();
        String out = trimmed(readAll(p.getInputStream()));
        String err = trimmed(readAll(p.getErrorStream()));
        check(p.waitFor(30, TimeUnit.SECONDS), "stdout/stderr child did not exit");
        check(p.exitValue() == 0, "stdout/stderr child exit code");
        check(out.contains("OUT-LINE"), "stdout must contain the child's stdout write, got: " + out);
        check(!out.contains("ERR-LINE"), "stdout must NOT contain the child's stderr write, got: " + out);
        check(err.contains("ERR-LINE"), "stderr must contain the child's stderr write, got: " + err);
        check(!err.contains("OUT-LINE"), "stderr must NOT contain the child's stdout write, got: " + err);
    }

    static void redirectErrorStreamMergesIntoStdout() throws Exception {
        ProcessBuilder pb = new ProcessBuilder(stdoutAndStderrCmd());
        pb.redirectErrorStream(true);
        Process p = pb.start();
        p.getOutputStream().close();
        String out = trimmed(readAll(p.getInputStream()));
        check(p.waitFor(30, TimeUnit.SECONDS), "merged child did not exit");
        check(p.exitValue() == 0, "merged child exit code");
        check(out.contains("OUT-LINE"), "merged stream must contain the stdout write, got: " + out);
        check(out.contains("ERR-LINE"), "merged stream must contain the stderr write once merged, got: " + out);
    }

    static void runtimeExecOverload() throws Exception {
        // Runtime.exec(String[]) is a different call path from
        // ProcessBuilder.start() -- both must return live streams, not just
        // whichever one the fix happened to cover.
        Process p = Runtime.getRuntime().exec(catCmd().toArray(new String[0]));
        OutputStream stdin = p.getOutputStream();
        InputStream stdout = p.getInputStream();
        check(stdin != null, "Runtime.exec: stdin stream must not be null");
        check(stdout != null, "Runtime.exec: stdout stream must not be null");

        String payload = "runtime-exec-round-trip";
        stdin.write((payload + System.lineSeparator()).getBytes(StandardCharsets.UTF_8));
        stdin.close();
        String echoed = trimmed(readAll(stdout));
        check(p.waitFor(30, TimeUnit.SECONDS), "Runtime.exec child did not exit");
        check(p.exitValue() == 0, "Runtime.exec child exit code");
        check(payload.equals(echoed), "Runtime.exec: stdin must round-trip through stdout unchanged");
    }

    static void exitCodeReflectsChildBehavior() throws Exception {
        Process p = new ProcessBuilder(exitWithCode(7)).start();
        p.getOutputStream().close();
        readAll(p.getInputStream());
        readAll(p.getErrorStream());
        check(p.waitFor(30, TimeUnit.SECONDS), "exit-code child did not exit");
        check(p.exitValue() == 7, "exit code must be the child's real exit code, not a stub 0");
    }

    public static void main(String[] args) throws Exception {
        stdinToStdoutRoundTrip();
        stdoutAndStderrAreSeparateByDefault();
        redirectErrorStreamMergesIntoStdout();
        runtimeExecOverload();
        exitCodeReflectsChildBehavior();
        if (checks != EXPECTED_CHECKS) {
            throw new AssertionError("check count moved: expected " + EXPECTED_CHECKS + ", ran " + checks);
        }
        System.out.println("CK RJdkProcessStreams checks=" + checks);
        System.out.println("PASS RJdkProcessStreams (" + checks + " checks)");
    }
}
