import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;

/**
 * gci i18/dc (page gci-i17jh-thread-print-shows-no-frames-for-a-running-thread-20261014):
 * {@code jcmd <own pid> Thread.print} while two platform threads spin in named
 * methods. HotSpot's dump runs at a safepoint and lists a RUNNING thread's
 * frames; the VM printed such a thread RUNNABLE with no frame at all.
 *
 * <p>Threads: {@code gci-dc18-spin-int} spins in {@link #spinInt()};
 * {@code gci-dc18-spin-locked} spins in {@link #spinLocked()} holding the
 * monitor of a {@code Lock18}. Both are {@code Spinner} subclasses of Thread
 * (no lambda, no Runnable target). The main thread runs jcmd {@value #DUMPS}
 * times and counts, per dump, what each spinner's section lists.
 *
 * <p>stdout (deterministic; HotSpot prints every count as {@value #DUMPS}/{@value #DUMPS}):
 * <pre>
 * jcmd rc0 3/3
 * spin-int RUNNABLE 3/3
 * spin-int top spinInt 3/3
 * spin-int lists Spinner.run 3/3
 * spin-locked RUNNABLE 3/3
 * spin-locked top spinLocked 3/3
 * spin-locked locked Lock18 3/3
 * </pre>
 * stderr: {@code [gci-dc18] dump=<n> rc=<rc> ms=<ms>} and both spinners'
 * sections of every dump. jcmd: {@code -Dgci.jcmd=<path>}, else
 * {@code java.home/bin}, else {@code JAVA_HOME/bin}.
 */
public class GciDc18ThreadPrintProbe {
    static final long JCMD_TIMEOUT_S = 120;
    static final int DUMPS = 3;
    static final String INT_NAME = "gci-dc18-spin-int";
    static final String LOCKED_NAME = "gci-dc18-spin-locked";

    static volatile boolean stop;
    static volatile long sink;
    static volatile int started;

    static final class Lock18 {
    }

    static final Lock18 LOCK = new Lock18();

    static long spinInt() {
        long acc = 1;
        while (!stop) {
            for (int i = 0; i < 100_000; i++) {
                acc = acc * 31 + i;
            }
        }
        return acc;
    }

    static long spinLocked() {
        long acc = 7;
        synchronized (LOCK) {
            while (!stop) {
                for (int i = 0; i < 100_000; i++) {
                    acc = acc * 17 + i;
                }
            }
        }
        return acc;
    }

    static final class Spinner extends Thread {
        final boolean locked;

        Spinner(String name, boolean locked) {
            super(name);
            this.locked = locked;
            setDaemon(true);
        }

        @Override
        public void run() {
            synchronized (GciDc18ThreadPrintProbe.class) {
                started = started + 1;
            }
            long r = locked ? spinLocked() : spinInt();
            sink = r;
        }
    }

    static String jcmdPath() {
        String exe = System.getProperty("os.name").toLowerCase().contains("win") ? "jcmd.exe" : "jcmd";
        String jcmd = System.getProperty("gci.jcmd");
        if (jcmd != null) {
            return jcmd;
        }
        Path home = Path.of(System.getProperty("java.home"), "bin", exe);
        String envHome = System.getenv("JAVA_HOME");
        if (!Files.isRegularFile(home) && envHome != null) {
            home = Path.of(envHome, "bin", exe);
        }
        return home.toString();
    }

    /** Runs jcmd with its output in a file; returns {rc, output, ms}. rc -1: killed. */
    static Object[] jcmd(String jcmd, long pid, String command) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(jcmd);
        cmd.add(Long.toString(pid));
        cmd.add(command);
        Path out = Files.createTempFile("gci-dc18-jcmd-", ".txt");
        ProcessBuilder pb = new ProcessBuilder(cmd);
        pb.redirectErrorStream(true);
        pb.redirectOutput(ProcessBuilder.Redirect.to(out.toFile()));
        long t0 = System.nanoTime();
        Process p = pb.start();
        p.getOutputStream().close();
        int rc;
        if (p.waitFor(JCMD_TIMEOUT_S, TimeUnit.SECONDS)) {
            rc = p.exitValue();
        } else {
            p.destroyForcibly();
            p.waitFor();
            rc = -1;
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        String text = new String(Files.readAllBytes(out), StandardCharsets.UTF_8);
        Files.deleteIfExists(out);
        return new Object[] {rc, text, ms};
    }

    /** The lines of thread {@code name}'s section: its header line up to the next header. */
    static List<String> section(String dump, String name) {
        List<String> out = new ArrayList<>();
        boolean in = false;
        for (String line : dump.split("\\R")) {
            if (line.startsWith("\"")) {
                in = line.startsWith("\"" + name + "\"");
            }
            if (in) {
                out.add(line);
            }
        }
        return out;
    }

    static boolean runnable(List<String> sec) {
        for (String line : sec) {
            if (line.trim().equals("java.lang.Thread.State: RUNNABLE")) {
                return true;
            }
        }
        return false;
    }

    /** The first frame line's text after {@code at }, or "" when the section lists none. */
    static String top(List<String> sec) {
        for (String line : sec) {
            String t = line.trim();
            if (t.startsWith("at ")) {
                return t.substring(3);
            }
        }
        return "";
    }

    static boolean lists(List<String> sec, String needle) {
        for (String line : sec) {
            String t = line.trim();
            if (t.startsWith("at ") && t.contains(needle)) {
                return true;
            }
        }
        return false;
    }

    static boolean lockedLine(List<String> sec) {
        for (String line : sec) {
            String t = line.trim();
            if (t.startsWith("- locked <") && t.contains("GciDc18ThreadPrintProbe$Lock18")) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        Spinner a = new Spinner(INT_NAME, false);
        Spinner b = new Spinner(LOCKED_NAME, true);
        a.start();
        b.start();
        while (started < 2) {
            Thread.sleep(10);
        }
        // Into the loops (and, with a JIT, compiled) before the first dump.
        Thread.sleep(1000);
        long pid = ProcessHandle.current().pid();
        String jcmd = jcmdPath();
        int rc0 = 0;
        int intRunnable = 0;
        int intTop = 0;
        int intRun = 0;
        int lockedRunnable = 0;
        int lockedTop = 0;
        int lockedLock = 0;
        for (int d = 0; d < DUMPS; d++) {
            Object[] r = jcmd(jcmd, pid, "Thread.print");
            int rc = (Integer) r[0];
            String text = (String) r[1];
            long ms = (Long) r[2];
            if (rc == 0) {
                rc0++;
            }
            List<String> si = section(text, INT_NAME);
            List<String> sl = section(text, LOCKED_NAME);
            if (runnable(si)) {
                intRunnable++;
            }
            if (top(si).startsWith("GciDc18ThreadPrintProbe.spinInt(")) {
                intTop++;
            }
            if (lists(si, "GciDc18ThreadPrintProbe$Spinner.run(")) {
                intRun++;
            }
            if (runnable(sl)) {
                lockedRunnable++;
            }
            if (top(sl).startsWith("GciDc18ThreadPrintProbe.spinLocked(")) {
                lockedTop++;
            }
            if (lockedLine(sl)) {
                lockedLock++;
            }
            System.err.println("[gci-dc18] dump=" + d + " rc=" + rc + " ms=" + ms
                    + " int_lines=" + si.size() + " locked_lines=" + sl.size());
            for (String line : si) {
                System.err.println("[gci-dc18]   " + line);
            }
            for (String line : sl) {
                System.err.println("[gci-dc18]   " + line);
            }
            if (si.isEmpty() || sl.isEmpty()) {
                System.err.println("[gci-dc18] raw dump follows");
                System.err.println(text);
            }
        }
        stop = true;
        a.join(10_000);
        b.join(10_000);
        String n = "/" + DUMPS;
        System.out.println("jcmd rc0 " + rc0 + n);
        System.out.println("spin-int RUNNABLE " + intRunnable + n);
        System.out.println("spin-int top spinInt " + intTop + n);
        System.out.println("spin-int lists Spinner.run " + intRun + n);
        System.out.println("spin-locked RUNNABLE " + lockedRunnable + n);
        System.out.println("spin-locked top spinLocked " + lockedTop + n);
        System.out.println("spin-locked locked Lock18 " + lockedLock + n);
    }
}
