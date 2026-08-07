import java.lang.reflect.Method;
import java.util.Optional;

/**
 * Where does `ProcessHandle.of(foreignPid)` lose the answer in compatible mode?
 *
 * `of()` is one line of JDK bytecode — `ProcessHandleImpl.get(pid)`, which is
 * `long start = isAlive0(pid); return (start >= 0) ? Optional.of(new
 * ProcessHandleImpl(pid, start)) : Optional.empty();` — so there are only three
 * places the answer can go wrong, and this probe asks each of them separately
 * instead of inferring from the end result:
 *
 *   1. `isAlive0(pid)` itself, invoked reflectively. `--jdk-only` gets this
 *      right for the same pid, so if compatible mode also gets it right the
 *      native is not the suspect.
 *   2. `ProcessHandleImpl.get(pid)`, the caller, invoked reflectively — isolates
 *      an interception of `get` from one of `of`.
 *   3. `ProcessHandle.of(pid)`, the public route the defect was reported on.
 *
 * Needs `--add-opens java.base/java.lang=ALL-UNNAMED`. When the opens are
 * missing every reflective rung says so rather than guessing, so an
 * accidentally-unopened run cannot be mistaken for a result.
 */
public class HandleOfRouteProbe {

    interface Step {
        String run() throws Exception;
    }

    static void say(String key, Step step) {
        String value;
        try {
            value = step.run();
        } catch (Throwable t) {
            Throwable c = (t.getCause() != null) ? t.getCause() : t;
            value = "threw " + c.getClass().getName()
                    + (c.getMessage() == null ? "" : ": " + c.getMessage());
        }
        System.out.println(key + "=" + value);
    }

    public static void main(String[] args) throws Exception {
        Process launcher = new ProcessBuilder("/bin/sh", "-c", "sleep 20 & echo $!").start();
        final long pid = Long.parseLong(
                new String(launcher.getInputStream().readAllBytes()).trim());
        launcher.waitFor();

        final Class<?> impl = Class.forName("java.lang.ProcessHandleImpl");

        // 1. The native, directly. A start time >= 0 means "this process
        //    exists"; -1 is the only "no such process".
        say("isAlive0_foreign", () -> {
            Method m = impl.getDeclaredMethod("isAlive0", long.class);
            m.setAccessible(true);
            return String.valueOf((long) m.invoke(null, pid));
        });
        say("isAlive0_self", () -> {
            Method m = impl.getDeclaredMethod("isAlive0", long.class);
            m.setAccessible(true);
            return String.valueOf((long) m.invoke(null, ProcessHandle.current().pid()));
        });

        // 2. The one-line caller.
        say("ProcessHandleImpl_get", () -> {
            Method m = impl.getDeclaredMethod("get", long.class);
            m.setAccessible(true);
            Object o = m.invoke(null, pid);
            return String.valueOf(((Optional<?>) o).isPresent());
        });

        // 3. The public route.
        say("ProcessHandle_of", () -> String.valueOf(ProcessHandle.of(pid).isPresent()));

        // Cross-check that the process really is there, without the JDK.
        say("procEntryExists",
                () -> String.valueOf(new java.io.File("/proc/" + pid).exists()));

        new ProcessBuilder("/bin/kill", "-9", String.valueOf(pid))
                .redirectErrorStream(true).start().waitFor();
        System.out.println("DONE");
    }
}
