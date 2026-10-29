import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;
import java.util.concurrent.*;

/**
 * `ProcessBuilder.start()` — `WORKER-4-1` N3's sharpest identity row, and the
 * only one where the TWO CRATONVM MODES DISAGREE WITH EACH OTHER:
 *
 * ```text
 *   HotSpot               java.lang.ProcessImpl
 *   CratonVM --jdk-only   java.lang.ProcessImpl        <- correct
 *   CratonVM --real-jdk   cratonvm.synthetic.Process   <- a name no JDK declares
 * ```
 *
 * A synthetic class name reaching an application in COMPATIBLE mode is worse
 * than an abstract one in a specific way: `getClass().getName()` is what a log
 * line, a serialized record and a `switch` on class name all read, and
 * `cratonvm.synthetic.Process` is a name that exists nowhere else in the world.
 *
 * The interesting question is not the name, though — it is whether the strict
 * mode's answer is a WORKING `ProcessImpl` or merely a better-named object. If
 * `--jdk-only` runs the JDK's own process machinery correctly end to end, then
 * compatible mode is declining a path that works. Every case below is therefore
 * BEHAVIOUR, not identity, except the two that say `class`.
 */
public class W4Process {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            ck(tag, "threw:" + e.getClass().getName());
        }
    }

    static String slurp(InputStream in) throws IOException {
        return new String(in.readAllBytes(), StandardCharsets.UTF_8).trim();
    }

    public static void main(String[] args) throws Exception {
        // ---- identity, for the record ------------------------------------
        Process p0 = new ProcessBuilder("true").start();
        ck("class", p0.getClass().getName());
        ck("isProcessSubclass", p0 instanceof Process);
        p0.waitFor();

        // ---- exit codes --------------------------------------------------
        ck("exit.true", new ProcessBuilder("true").start().waitFor());
        ck("exit.false", new ProcessBuilder("false").start().waitFor());
        ck("exit.code7", new ProcessBuilder("sh", "-c", "exit 7").start().waitFor());
        Process pe = new ProcessBuilder("sh", "-c", "exit 3").start();
        pe.waitFor();
        ck("exitValue", pe.exitValue());
        ck("isAlive.afterExit", pe.isAlive());

        // ---- stdout / stderr ---------------------------------------------
        Process po = new ProcessBuilder("sh", "-c", "echo out; echo err 1>&2").start();
        ck("stdout", slurp(po.getInputStream()));
        ck("stderr", slurp(po.getErrorStream()));
        ck("stdout.exit", po.waitFor());

        // ---- stdin --------------------------------------------------------
        Process pi = new ProcessBuilder("cat").start();
        try (OutputStream o = pi.getOutputStream()) {
            o.write("piped\n".getBytes(StandardCharsets.UTF_8));
        }
        ck("stdin.echoed", slurp(pi.getInputStream()));
        ck("stdin.exit", pi.waitFor());

        // ---- redirects ------------------------------------------------------
        Process pr = new ProcessBuilder("sh", "-c", "echo both 1>&2")
                .redirectErrorStream(true).start();
        ck("redirectErrorStream", slurp(pr.getInputStream()));
        pr.waitFor();
        File tmp = File.createTempFile("w4proc", ".txt");
        Process pf = new ProcessBuilder("sh", "-c", "echo tofile")
                .redirectOutput(tmp).start();
        pf.waitFor();
        ck("redirectOutput", new String(
                java.nio.file.Files.readAllBytes(tmp.toPath()), StandardCharsets.UTF_8).trim());
        tmp.delete();

        // ---- environment and working directory --------------------------------
        ProcessBuilder eb = new ProcessBuilder("sh", "-c", "echo $W4VAR");
        eb.environment().put("W4VAR", "from-env");
        Process pv = eb.start();
        ck("environment", slurp(pv.getInputStream()));
        pv.waitFor();
        Process pd = new ProcessBuilder("pwd").directory(new File("/")).start();
        ck("directory", slurp(pd.getInputStream()));
        pd.waitFor();

        // ---- waitFor(timeout), destroy, onExit ---------------------------------
        Process ps = new ProcessBuilder("sleep", "5").start();
        ck("waitFor.timesOut", ps.waitFor(300, TimeUnit.MILLISECONDS));
        ck("isAlive.duringSleep", ps.isAlive());
        ps.destroy();
        ck("waitFor.afterDestroy", ps.waitFor(5, TimeUnit.SECONDS));
        ck("isAlive.afterDestroy", ps.isAlive());
        Process pk = new ProcessBuilder("sleep", "5").start();
        pk.destroyForcibly();
        ck("destroyForcibly.waited", pk.waitFor(5, TimeUnit.SECONDS));
        Process px = new ProcessBuilder("true").start();
        ck("onExit.get", px.onExit().get(5, TimeUnit.SECONDS).exitValue());

        // ---- pid and handle ------------------------------------------------------
        // A LONG-LIVED child, deliberately. `info()` reads /proc/<pid>, which
        // is gone the moment the child is reaped, so asking it about a
        // `true` that has already exited measures a race rather than a
        // capability — and the two VMs would not even be racing the same
        // spawn cost.
        Process pp = new ProcessBuilder("sleep", "2").start();
        long pid = pp.pid();
        ck("pid.positive", pid > 0);
        ck("toHandle.pidMatches", pp.toHandle().pid() == pid);
        ck("isAlive.beforeInfo", pp.isAlive());
        ck("info.commandPresent", pp.info().command().isPresent());
        ck("info.commandLinePresent", pp.info().commandLine().isPresent());
        ck("info.argumentsPresent", pp.info().arguments().isPresent());
        ck("info.commandEndsWithSleep",
                pp.info().command().map(c -> c.endsWith("sleep")).orElse(false));
        ck("info.userPresent", pp.info().user().isPresent());
        ck("info.startInstantPresent", pp.info().startInstant().isPresent());
        pp.destroyForcibly();
        pp.waitFor();

        // ---- failure modes ---------------------------------------------------------
        ckT("start.noSuchCommand", () -> new ProcessBuilder("/no/such/binary-xyz").start());
        ckT("start.emptyCommand", () -> new ProcessBuilder(new ArrayList<String>()).start());
        ckT("exitValue.whileAlive", () -> {
            Process a = new ProcessBuilder("sleep", "2").start();
            try {
                return a.exitValue();
            } finally {
                a.destroyForcibly();
            }
        });

        System.out.println("PASS W4Process");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
