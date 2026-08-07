/**
 * `ProcessHandle.destroy()` / `destroyForcibly()` on a process this JVM did not
 * spawn.
 *
 * `ProcessHandleImpl.destroy0(pid, startTime, forcibly)` is the native, and its
 * `startTime` argument is the JDK's guard against a recycled pid: the real
 * implementation compares it to the process's actual start time and refuses to
 * signal if they disagree. CratonVM reports `STARTTIME_ANY` (0) from `isAlive0`
 * for every process, so every handle carries 0 and there is nothing to compare —
 * which is why the bridge declines to signal a non-child at all.
 *
 * Both rungs use a grandchild whose stdio is redirected away from the shell's
 * pipe. That is not incidental: without it, `readAllBytes()` on the launcher
 * races the reader's `processExited()` hook and can block for the child's whole
 * lifetime, leaving the subject dead before its pid is used — in HotSpot too.
 * See `ForeignHandleProbe.launchGrandchild`.
 *
 * `destroyOwnChild` is the control. If the foreign rung fails and this one
 * passes, the gap is about ownership; if both fail, it is about signalling.
 */
public class ForeignDestroyProbe {

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

    static long launchGrandchild(String seconds) throws Exception {
        Process launcher = new ProcessBuilder("/bin/sh", "-c",
                "sleep " + seconds + " >/dev/null 2>&1 & echo $!").start();
        String printed = new String(launcher.getInputStream().readAllBytes()).trim();
        launcher.waitFor();
        return Long.parseLong(printed);
    }

    /** Ask the OS, not the JDK, whether the pid is still there. */
    static boolean procExists(long pid) {
        return new java.io.File("/proc/" + pid).exists();
    }

    static void reap(long pid) throws Exception {
        new ProcessBuilder("/bin/kill", "-9", String.valueOf(pid))
                .redirectErrorStream(true).start().waitFor();
    }

    /** Poll /proc rather than sleeping a fixed amount: a signal is not instant. */
    static boolean goneWithin(long pid, long millis) throws Exception {
        long deadline = System.nanoTime() + millis * 1_000_000L;
        while (System.nanoTime() < deadline) {
            if (!procExists(pid)) {
                return true;
            }
            Thread.sleep(20);
        }
        return !procExists(pid);
    }

    public static void main(String[] args) throws Exception {
        say("foreignDestroyForcibly", () -> {
            long pid = launchGrandchild("30");
            if (!procExists(pid)) {
                return "subject-not-running";
            }
            java.util.Optional<ProcessHandle> h = ProcessHandle.of(pid);
            if (h.isEmpty()) {
                return "no-handle";
            }
            boolean claimed = h.get().destroyForcibly();
            boolean gone = goneWithin(pid, 3000);
            reap(pid);
            return "returned=" + claimed + ",gone=" + gone;
        });

        say("foreignDestroy", () -> {
            long pid = launchGrandchild("30");
            if (!procExists(pid)) {
                return "subject-not-running";
            }
            java.util.Optional<ProcessHandle> h = ProcessHandle.of(pid);
            if (h.isEmpty()) {
                return "no-handle";
            }
            boolean claimed = h.get().destroy();
            boolean gone = goneWithin(pid, 3000);
            reap(pid);
            return "returned=" + claimed + ",gone=" + gone;
        });

        // Control: our own child, the case every other probe covers.
        say("destroyOwnChild", () -> {
            Process own = new ProcessBuilder("/bin/sleep", "30").start();
            boolean claimed = own.toHandle().destroyForcibly();
            own.waitFor();
            return "returned=" + claimed + ",gone=" + !own.isAlive();
        });

        System.out.println("DONE");
    }
}
