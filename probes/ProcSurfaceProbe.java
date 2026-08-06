import java.io.*;
import java.util.*;

/**
 * Differential probe for the java.lang.Runtime.exec / ProcessBuilder.start
 * surface. Every line is a name=value pair; run it on HotSpot and on CratonVM
 * and diff. Nothing here is CratonVM-specific.
 */
public class ProcSurfaceProbe {

    static File dir;

    static File script(String name, String body) throws IOException {
        File f = new File(dir, name);
        try (FileWriter fw = new FileWriter(f)) {
            fw.write(body);
        }
        f.setExecutable(true);
        return f;
    }

    static String drain(InputStream in) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        byte[] buf = new byte[2048];
        int n;
        while ((n = in.read(buf)) != -1) {
            bos.write(buf, 0, n);
        }
        return bos.toString("UTF-8");
    }

    public static void main(String[] args) throws Exception {
        dir = new File(System.getProperty("java.io.tmpdir"), "procsurface" + System.nanoTime());
        dir.mkdirs();

        File echo = script("echo.sh", "#!/bin/sh\necho \"out:$QUERY_STRING\"\necho \"err:$QUERY_STRING\" 1>&2\n");
        File slow = script("slow.sh", "#!/bin/sh\nsleep 2\necho done\n");
        File cat = script("cat.sh", "#!/bin/sh\ncat\n");
        File fail = script("fail.sh", "#!/bin/sh\nexit 7\n");
        File pwd = script("pwd.sh", "#!/bin/sh\npwd\n");

        // --- 1. exec(String[], String[], File): the CGIServlet route ---
        {
            Process p = Runtime.getRuntime().exec(
                    new String[] { echo.getAbsolutePath() },
                    new String[] { "QUERY_STRING=abc", "PATH=/usr/bin:/bin" },
                    dir);
            String out = drain(p.getInputStream());
            String err = drain(p.getErrorStream());
            System.out.println("T1_OUT=" + out.trim());
            System.out.println("T1_ERR=" + err.trim());
            System.out.println("T1_WAIT=" + p.waitFor());
            System.out.println("T1_PID_POSITIVE=" + (p.pid() > 0));
        }

        // --- 2. exec(String) with whitespace splitting ---
        {
            Process p = Runtime.getRuntime().exec("/bin/echo hello world");
            System.out.println("T2_OUT=" + drain(p.getInputStream()).trim());
            System.out.println("T2_WAIT=" + p.waitFor());
        }

        // --- 3. exec(String[]) plain ---
        {
            Process p = Runtime.getRuntime().exec(new String[] { "/bin/echo", "a b", "c" });
            System.out.println("T3_OUT=" + drain(p.getInputStream()).trim());
        }

        // --- 4. exec must NOT block: the child sleeps 2s, exec returns at once ---
        {
            long t0 = System.nanoTime();
            Process p = Runtime.getRuntime().exec(new String[] { slow.getAbsolutePath() });
            long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;
            System.out.println("T4_EXEC_RETURNED_FAST=" + (elapsedMs < 1000));
            System.out.println("T4_ALIVE_BEFORE_WAIT=" + p.isAlive());
            boolean itse;
            try {
                p.exitValue();
                itse = false;
            } catch (IllegalThreadStateException e) {
                itse = true;
            }
            System.out.println("T4_EXITVALUE_THREW_ITSE=" + itse);
            System.out.println("T4_WAIT=" + p.waitFor());
            System.out.println("T4_ALIVE_AFTER_WAIT=" + p.isAlive());
            System.out.println("T4_OUT=" + drain(p.getInputStream()).trim());
        }

        // --- 5. stdin is a live pipe ---
        {
            Process p = Runtime.getRuntime().exec(new String[] { cat.getAbsolutePath() });
            OutputStream os = p.getOutputStream();
            os.write("piped-in\n".getBytes("UTF-8"));
            os.flush();
            os.close();
            System.out.println("T5_OUT=" + drain(p.getInputStream()).trim());
            System.out.println("T5_WAIT=" + p.waitFor());
        }

        // --- 6. non-zero exit code survives ---
        {
            Process p = Runtime.getRuntime().exec(new String[] { fail.getAbsolutePath() });
            System.out.println("T6_WAIT=" + p.waitFor());
            System.out.println("T6_EXITVALUE=" + p.exitValue());
        }

        // --- 7. working directory is honoured ---
        {
            File sub = new File(dir, "sub");
            sub.mkdirs();
            Process p = Runtime.getRuntime().exec(
                    new String[] { pwd.getAbsolutePath() }, null, sub);
            System.out.println("T7_ENDS_WITH_SUB=" + drain(p.getInputStream()).trim().endsWith("sub"));
            p.waitFor();
        }

        // --- 8. null envp inherits this process's environment ---
        {
            Process p = Runtime.getRuntime().exec(
                    new String[] { "/bin/sh", "-c", "echo $PROBE_INHERIT" }, null, null);
            System.out.println("T8_INHERITED=" + drain(p.getInputStream()).trim());
            p.waitFor();
        }

        // --- 9. a bad program is an IOException, not something else ---
        {
            String kind;
            try {
                Runtime.getRuntime().exec(new String[] { "/definitely/not/here" });
                kind = "NO_THROW";
            } catch (IOException e) {
                kind = "IOException";
            } catch (Throwable t) {
                kind = t.getClass().getName();
            }
            System.out.println("T9_THROWN=" + kind);
        }

        // --- 10. an empty cmdarray ---
        {
            String kind;
            try {
                Runtime.getRuntime().exec(new String[0]);
                kind = "NO_THROW";
            } catch (Throwable t) {
                kind = t.getClass().getName();
            }
            System.out.println("T10_THROWN=" + kind);
        }

        // --- 11. a null element in cmdarray ---
        {
            String kind;
            try {
                Runtime.getRuntime().exec(new String[] { "/bin/echo", null });
                kind = "NO_THROW";
            } catch (Throwable t) {
                kind = t.getClass().getName();
            }
            System.out.println("T11_THROWN=" + kind);
        }

        // --- 12. ProcessBuilder control arm (same child, other entry point) ---
        {
            ProcessBuilder pb = new ProcessBuilder(echo.getAbsolutePath());
            pb.environment().put("QUERY_STRING", "pb");
            pb.directory(dir);
            Process p = pb.start();
            System.out.println("T12_OUT=" + drain(p.getInputStream()).trim());
            System.out.println("T12_ERR=" + drain(p.getErrorStream()).trim());
            System.out.println("T12_WAIT=" + p.waitFor());
        }

        // --- 13. redirectErrorStream merges stderr into stdout ---
        {
            ProcessBuilder pb = new ProcessBuilder(echo.getAbsolutePath());
            pb.environment().put("QUERY_STRING", "merged");
            pb.redirectErrorStream(true);
            Process p = pb.start();
            String all = drain(p.getInputStream());
            System.out.println("T13_HAS_OUT=" + all.contains("out:merged"));
            System.out.println("T13_HAS_ERR=" + all.contains("err:merged"));
            System.out.println("T13_WAIT=" + p.waitFor());
        }

        // --- 14. destroy() on a live child ---
        {
            Process p = Runtime.getRuntime().exec(new String[] { slow.getAbsolutePath() });
            p.destroy();
            int rc = p.waitFor();
            System.out.println("T14_NONZERO_OR_SIGNALLED=" + (rc != 0));
            System.out.println("T14_ALIVE_AFTER_DESTROY=" + p.isAlive());
        }

        // --- 15. waitFor(timeout) on a child that outlives it ---
        {
            Process p = Runtime.getRuntime().exec(new String[] { slow.getAbsolutePath() });
            System.out.println("T15_TIMED_OUT=" + !p.waitFor(200, java.util.concurrent.TimeUnit.MILLISECONDS));
            System.out.println("T15_THEN_WAITS=" + p.waitFor());
        }

        System.out.println("PROBE_DONE");
    }
}
