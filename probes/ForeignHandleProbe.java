import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;

/**
 * `ProcessHandle` for a process this JVM did NOT spawn — the `NOT_A_CHILD` path.
 *
 * `ProcessHandleImpl.waitForProcessExit0(pid, reap)` is the only native that can
 * tell the JDK "that pid is not my child": it answers the `@Native` constant
 * `NOT_A_CHILD` (-2), and the JDK then falls back to polling `isAlive0(pid)`
 * until the process really ends. Any other negative value is taken at face
 * value as the process's exit status, so a -1 there completes
 * `ProcessHandle.of(pid).onExit()` immediately, with a fabricated -1, while the
 * process is still running.
 *
 * The subject is a GRANDCHILD. `sh` forks it and exits, so it is reparented and
 * `waitpid` from this JVM gives ECHILD, which is exactly the case the constant
 * exists for. A direct child takes the ordinary path and measures nothing.
 *
 * # Why these rungs and not others
 *
 * Not timing comparisons — a shared build host makes those worthless. Each is
 * one-way, so a slow host cannot manufacture a pass:
 *
 *   * `onExitWaitsWhileAlive` bounds the wait at 400 ms against a process that
 *     lives 30 s. Load can only make that timeout take longer in wall time; the
 *     future cannot complete unless something completed it, so
 *     `completed-while-alive` is reachable only by the defect.
 *   * `goneAfterExternalKill` is the other half, and it only means anything in
 *     combination with the first: once the future is known NOT to have
 *     completed early, waiting on it after the process really dies fails — by
 *     timing out — if the polling fallback never terminates because `isAlive0`
 *     will not report a foreign process as gone.
 *
 * A third rung asked the same question of a `sleep 1` that ended on its own, and
 * it was removed for racing: the subject could exit before `ProcessHandle.of`
 * ran, so `of()` answered empty and the rung printed `no-handle` — seen once in
 * three runs on real HotSpot, which is exactly rare enough to be misread as a
 * regression in whatever is under test. It measured nothing the deterministic
 * rung above does not.
 *
 * DELIBERATELY NOT MEASURED HERE: `ProcessHandle.destroy()/destroyForcibly()` on
 * a foreign handle. CratonVM refuses to signal a pid it did not spawn — a
 * documented safety choice, since it keeps no start time and so cannot run the
 * JDK's staleness check against a recycled pid — and HotSpot does signal it.
 * That is a real divergence, filed separately; ending the subject here goes
 * through `/bin/kill` as an ordinary child process so that this probe measures
 * the `NOT_A_CHILD` path and nothing else.
 */
public class ForeignHandleProbe {

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

    /** Launch a backgrounded `sleep` that outlives its shell, and return its pid. */
    static long launchGrandchild(String seconds) throws Exception {
        Process launcher = new ProcessBuilder("/bin/sh", "-c",
                "sleep " + seconds + " & echo $!").start();
        String printed = new String(launcher.getInputStream().readAllBytes()).trim();
        launcher.waitFor();
        return Long.parseLong(printed);
    }

    static void kill(long pid) throws Exception {
        new ProcessBuilder("/bin/kill", "-9", String.valueOf(pid))
                .redirectErrorStream(true).start().waitFor();
    }

    public static void main(String[] args) throws Exception {
        final long pid = launchGrandchild("30");

        Optional<ProcessHandle> found = ProcessHandle.of(pid);
        say("foreignHandlePresent", () -> String.valueOf(found.isPresent()));
        if (found.isEmpty()) {
            kill(pid);
            System.out.println("DONE");
            return;
        }
        final ProcessHandle handle = found.get();
        say("foreignHandleAlive", () -> String.valueOf(handle.isAlive()));
        say("foreignHandlePid", () -> String.valueOf(handle.pid() == pid));

        final CompletableFuture<ProcessHandle> exit = handle.onExit();
        say("onExitWaitsWhileAlive", () -> {
            try {
                exit.get(400, TimeUnit.MILLISECONDS);
                return "completed-while-alive";
            } catch (TimeoutException expected) {
                return "still-waiting";
            }
        });

        // End the 30 s subject through an ordinary child process, so this rung
        // measures `isAlive0`, not `destroy0`. The `exit.get` here IS the
        // assertion that the polling fallback terminates: it throws if not.
        say("goneAfterExternalKill", () -> {
            kill(pid);
            exit.get(30, TimeUnit.SECONDS);
            return String.valueOf(handle.isAlive());
        });

        // A pid that cannot exist: `of()` must answer empty rather than mint a
        // handle to nothing.
        say("absentPidHasNoHandle", () -> String.valueOf(ProcessHandle.of(0x7ffffff0L).isEmpty()));

        System.out.println("DONE");
    }
}
