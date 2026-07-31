import java.io.File;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

/**
 * JDK-only corpus: {@code ProcessHandle} -- current process, parent, info,
 * liveness, child process.
 *
 * "{@code ProcessHandle}" is a named P1 blocker: CratonVM's
 * {@code is_native_backed_jdk_stub} explicitly allows
 * {@code java/lang/ProcessHandle} and {@code java/lang/ProcessHandle$Info} to be
 * FABRICATED with a hand-written method table when boot bytes are unavailable.
 * Under {@code --jdk-only} the real classes must load and the platform leaves
 * must be bridges.
 *
 * Determinism: pids, command lines, users, start instants and cpu durations are
 * all host- and run-specific. NONE of them is printed -- only derived
 * predicates (present/absent, >0, alive/dead) and the child's exit code, which
 * this vector chooses itself.
 */
public class RJdkProcess {
    static final long T = 60;
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static boolean windows() {
        return File.separatorChar == '\\';
    }

    /** A command that exits with code 3 and prints nothing at all. */
    static List<String> exitThree() {
        return windows()
                ? Arrays.asList("cmd.exe", "/c", "exit 3")
                : Arrays.asList("/bin/sh", "-c", "exit 3");
    }

    /** A command that sleeps long enough to be observed alive, then exits. */
    static List<String> sleepLong() {
        return windows()
                ? Arrays.asList("cmd.exe", "/c", "ping -n 30 127.0.0.1 > NUL")
                : Arrays.asList("/bin/sh", "-c", "sleep 30");
    }

    static void currentProcess() {
        ProcessHandle self = ProcessHandle.current();
        check(self != null, "ProcessHandle.current()");
        check(self.pid() > 0, "current pid must be positive");
        check(self.isAlive(), "the current process must be alive");
        check(self.equals(ProcessHandle.current()), "current() must be equal across calls");
        check(self.hashCode() == ProcessHandle.current().hashCode(), "current() hashCode");
        check(ProcessHandle.of(self.pid()).isPresent(), "of(pid) must find the current process");
        check(ProcessHandle.of(self.pid()).get().equals(self), "of(pid) identity");

        // Destroying yourself is refused.
        boolean threw = false;
        try {
            self.destroy();
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "destroying the current process must raise IllegalStateException");

        // parent() is an Optional -- whether a parent is visible is
        // host-specific (a reaped or out-of-scope parent legitimately gives
        // empty), so only the call contract is asserted, never the value.
        Optional<ProcessHandle> parent = self.parent();
        check(parent != null, "parent() must never return null");

        // Info: every field is an Optional and every one of them is allowed to
        // be empty on a restricted platform. Assert the SHAPE, print nothing.
        ProcessHandle.Info info = self.info();
        check(info != null, "info() must never return null");
        check(info.command() != null, "info().command()");
        check(info.commandLine() != null, "info().commandLine()");
        check(info.arguments() != null, "info().arguments()");
        check(info.startInstant() != null, "info().startInstant()");
        check(info.totalCpuDuration() != null, "info().totalCpuDuration()");
        check(info.user() != null, "info().user()");
        check(info.toString() != null, "info().toString()");
        // If the command IS reported it must name an executable, not be blank.
        if (info.command().isPresent()) {
            check(!info.command().get().trim().isEmpty(), "reported command must not be blank");
        }
        if (info.startInstant().isPresent()) {
            check(info.startInstant().get().toEpochMilli() > 0, "start instant must be positive");
        }

        // onExit() on the CURRENT process is explicitly disallowed.
        threw = false;
        try {
            CompletableFuture<ProcessHandle> exit = self.onExit();
            check(exit == null, "unreachable");
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "onExit() on the current process must raise IllegalStateException");

        // allProcesses() may be restricted, but must return a usable stream.
        long visible = ProcessHandle.allProcesses().limit(4).count();
        check(visible >= 0, "allProcesses must be enumerable");
        check(ProcessHandle.of(Long.MAX_VALUE).isEmpty(),
                "an impossible pid must not resolve");
        System.out.println("CK RJdkProcess self alive=" + self.isAlive()
                + " pidPositive=" + (self.pid() > 0)
                + " infoNonNull=true parentOptionalNonNull=" + (parent != null));
    }

    static void childProcess() throws Exception {
        ProcessBuilder pb = new ProcessBuilder(exitThree());
        pb.redirectOutput(ProcessBuilder.Redirect.DISCARD);
        pb.redirectError(ProcessBuilder.Redirect.DISCARD);
        pb.redirectInput(ProcessBuilder.Redirect.INHERIT);
        Process p = pb.start();

        ProcessHandle h = p.toHandle();
        check(h.pid() == p.pid(), "Process.pid must agree with its handle");
        check(h.pid() != ProcessHandle.current().pid(), "the child is a different process");
        check(h.parent().isPresent(), "a freshly forked child must report a parent");
        check(h.parent().get().pid() == ProcessHandle.current().pid(),
                "the child's parent must be us");
        check(ProcessHandle.current().children().anyMatch(c -> c.pid() == h.pid()),
                "the child must appear in our children()");
        check(ProcessHandle.current().descendants().anyMatch(c -> c.pid() == h.pid()),
                "the child must appear in our descendants()");

        check(p.waitFor(T, TimeUnit.SECONDS), "the child did not exit in time");
        check(p.exitValue() == 3, "child exit code: " + p.exitValue());
        check(!p.isAlive() && !h.isAlive(), "the child must be dead after waitFor");
        check(h.onExit().get(T, TimeUnit.SECONDS) != null, "onExit must complete");
        check(p.onExit().get(T, TimeUnit.SECONDS).exitValue() == 3, "Process.onExit exit code");

        // exitValue() on a live process is an error.
        Process live = new ProcessBuilder(sleepLong())
                .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start();
        boolean threw = false;
        try {
            live.exitValue();
        } catch (IllegalThreadStateException expected) {
            threw = true;
        }
        check(threw, "exitValue() on a live process must throw IllegalThreadStateException");
        check(live.isAlive(), "the sleeper must be alive");
        check(!live.waitFor(50, TimeUnit.MILLISECONDS), "timed waitFor must time out");

        // destroyForcibly must actually kill it.
        live.destroyForcibly();
        check(live.waitFor(T, TimeUnit.SECONDS), "destroyForcibly did not terminate the child");
        check(!live.isAlive(), "the child must be dead after destroyForcibly");
        check(live.toHandle().onExit().get(T, TimeUnit.SECONDS) != null,
                "onExit must complete for a killed child");
        // Exit codes for a killed process are platform-specific, so the VALUE
        // is deliberately not asserted or printed -- only that reading it no
        // longer throws.
        live.exitValue();

        // A command that does not exist must fail, not fabricate a process.
        threw = false;
        try {
            new ProcessBuilder("cratonvm-no-such-executable-20260731").start();
        } catch (java.io.IOException expected) {
            threw = true;
        }
        check(threw, "starting a missing executable must raise IOException");
        System.out.println("CK RJdkProcess child exit=" + p.exitValue()
                + " parentIsUs=true killedThenDead=" + !live.isAlive());
    }

    static void environmentAndDirectory() throws Exception {
        ProcessBuilder pb = new ProcessBuilder(exitThree());
        pb.environment().put("CRATONVM_RJDKPROCESS", "1");
        check("1".equals(pb.environment().get("CRATONVM_RJDKPROCESS")), "environment write");
        check(pb.command().size() == exitThree().size(), "command list");
        check(pb.directory() == null, "default working directory is inherited (null)");
        pb.directory(new File(System.getProperty("java.io.tmpdir")));
        check(pb.directory() != null, "explicit working directory");
        pb.redirectOutput(ProcessBuilder.Redirect.DISCARD);
        pb.redirectError(ProcessBuilder.Redirect.DISCARD);
        Process p = pb.start();
        check(p.waitFor(T, TimeUnit.SECONDS), "env/dir child did not exit");
        check(p.exitValue() == 3, "env/dir child exit code");

        List<String> covered = new ArrayList<>(Arrays.asList(
                "current", "of", "parent", "children", "descendants", "info",
                "isAlive", "onExit", "destroyForcibly", "allProcesses"));
        Collections.sort(covered);
        System.out.println("CK RJdkProcess covered=" + covered);
    }

    public static void main(String[] args) throws Exception {
        currentProcess();
        childProcess();
        environmentAndDirectory();
        System.out.println("CK RJdkProcess checks=" + checks);
        System.out.println("PASS RJdkProcess (" + checks + " checks)");
    }
}
