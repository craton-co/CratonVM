import java.io.File;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.file.Files;

/**
 * The whole `java.lang.Process` surface a real `ProcessImpl` has to deliver,
 * printed line by line so two VMs can be diffed byte for byte.
 *
 * `SubprocessKindProbe` answers "what class came back and does echo work".
 * This one answers "does every route through that object behave", which is the
 * question that matters once `ProcessBuilder.start()` stops being shadowed and
 * the real JDK's `ProcessImpl` starts running: the pid, the three streams in
 * both directions, `redirectErrorStream`, a file redirect, `onExit`'s async
 * completion, and `destroy`. Each of those reaches a different native —
 * `forkAndExec`'s `int[] fds` write-back, `waitForProcessExit0`'s pid lookup,
 * `destroy0`'s pid lookup — and each fails in its own quiet way.
 *
 * Every line is a `key=value` a diff can compare. Anything that throws is
 * printed as `key=threw <type>` rather than aborting the run, so one broken
 * route does not hide the state of the others.
 *
 *   $JDK/bin/java                        -cp out RealProcessSurfaceProbe
 *   cratonvm --real-jdk --java-home $JDK -cp out RealProcessSurfaceProbe
 *   cratonvm --jdk-only --java-home $JDK -cp out RealProcessSurfaceProbe
 *
 * ORDERING REQUIREMENT: `onExit()` runs `waitFor` on a pool thread. Its rung
 * blocks on the future, so it is self-ordering — but do not add a rung that
 * merely *starts* async work and then prints something it might race with. See
 * `UserProcessInterceptProbe` for what that costs.
 */
public class RealProcessSurfaceProbe {

    interface Step {
        String run() throws Exception;
    }

    static void say(String key, Step step) {
        String value;
        try {
            value = step.run();
        } catch (Throwable t) {
            value = "threw " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(key + "=" + value);
    }

    static String readAll(InputStream in) throws Exception {
        return new String(in.readAllBytes()).trim();
    }

    public static void main(String[] args) throws Exception {
        // --- Identity ------------------------------------------------------
        Process p = new ProcessBuilder("/bin/sh", "-c",
                "echo out-line; echo err-line 1>&2; exit 7").start();
        say("class", () -> p.getClass().getName());
        say("super", () -> p.getClass().getSuperclass().getName());
        say("isProcess", () -> String.valueOf(p instanceof Process));
        say("pidPositive", () -> String.valueOf(p.pid() > 0));
        say("handlePidMatches", () -> String.valueOf(p.toHandle().pid() == p.pid()));

        // --- Both output pipes, kept separate ------------------------------
        say("stdout", () -> readAll(p.getInputStream()));
        say("stderr", () -> readAll(p.getErrorStream()));
        say("exitCode", () -> String.valueOf(p.waitFor()));
        say("aliveAfterExit", () -> String.valueOf(p.isAlive()));
        say("exitValue", () -> String.valueOf(p.exitValue()));

        // --- stdin, the direction the other rungs never exercise -----------
        say("stdinRoundTrip", () -> {
            Process cat = new ProcessBuilder("/bin/cat").start();
            OutputStream os = cat.getOutputStream();
            os.write("hello-stdin\n".getBytes());
            os.flush();
            os.close();
            String echoed = readAll(cat.getInputStream());
            cat.waitFor();
            return echoed;
        });

        // --- waitFor with a timeout, on a child that outlives it -----------
        say("timedWaitFalse", () -> {
            Process slow = new ProcessBuilder("/bin/sleep", "5").start();
            boolean done = slow.waitFor(150, java.util.concurrent.TimeUnit.MILLISECONDS);
            slow.destroyForcibly();
            slow.waitFor();
            return String.valueOf(done);
        });

        // --- destroy: the `destroy0(pid, ...)` route -----------------------
        say("destroyEndsIt", () -> {
            Process victim = new ProcessBuilder("/bin/sleep", "30").start();
            victim.destroy();
            int rc = victim.waitFor();
            return String.valueOf(rc != 0 && !victim.isAlive());
        });

        // --- onExit: the reaper thread's `waitForProcessExit0(pid, true)` --
        say("onExitCode", () -> {
            Process quick = new ProcessBuilder("/bin/sh", "-c", "exit 3").start();
            return String.valueOf(quick.onExit().get().exitValue());
        });

        // --- redirectErrorStream: stderr merged into stdout ----------------
        say("merged", () -> {
            Process m = new ProcessBuilder("/bin/sh", "-c", "echo A; echo B 1>&2")
                    .redirectErrorStream(true).start();
            String all = readAll(m.getInputStream());
            m.waitFor();
            return "A=" + all.contains("A") + ",B=" + all.contains("B");
        });
        say("mergedErrStreamEmpty", () -> {
            Process m = new ProcessBuilder("/bin/sh", "-c", "echo B 1>&2")
                    .redirectErrorStream(true).start();
            m.waitFor();
            readAll(m.getInputStream());
            return String.valueOf(m.getErrorStream().read() == -1);
        });

        // --- File redirects: the caller hands the native a descriptor ------
        say("redirectOutputToFile", () -> {
            File f = File.createTempFile("craton-probe-out", ".txt");
            f.deleteOnExit();
            Process w = new ProcessBuilder("/bin/sh", "-c", "echo file-line")
                    .redirectOutput(f).start();
            w.waitFor();
            return Files.readString(f.toPath()).trim();
        });
        say("redirectInputFromFile", () -> {
            File f = File.createTempFile("craton-probe-in", ".txt");
            f.deleteOnExit();
            Files.writeString(f.toPath(), "from-file\n");
            Process r = new ProcessBuilder("/bin/cat").redirectInput(f).start();
            String got = readAll(r.getInputStream());
            r.waitFor();
            return got;
        });

        // --- INHERIT: the fds[i] == i case ---------------------------------
        say("inheritExit", () -> {
            Process i = new ProcessBuilder("/bin/true")
                    .redirectOutput(ProcessBuilder.Redirect.INHERIT)
                    .redirectError(ProcessBuilder.Redirect.INHERIT)
                    .start();
            return String.valueOf(i.waitFor());
        });

        // --- An argument that is legitimately empty ------------------------
        say("emptyArgKept", () -> {
            Process e = new ProcessBuilder("/bin/sh", "-c",
                    "printf '[%s]' \"$1\" \"$2\"", "sh", "", "z").start();
            String got = readAll(e.getInputStream());
            e.waitFor();
            return got;
        });

        // --- A command that cannot be spawned ------------------------------
        say("noSuchCommand", () -> {
            try {
                new ProcessBuilder("/definitely/not/a/binary").start();
                return "no-exception";
            } catch (java.io.IOException expected) {
                return "IOException";
            }
        });

        System.out.println("DONE");
    }
}
