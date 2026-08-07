/**
 * Which `close()` in `ProcessImpl.destroy` is the one with the null
 * `closeLock`? The three closes are stdin, stdout, stderr in that order and
 * each is a different class, so the stack trace names the answer directly.
 * Closing them one at a time first isolates it even if the trace is short.
 */
public class DestroyTraceProbe {
    static void tryClose(String label, AutoCloseable c) {
        System.out.println(label + ".class=" + c.getClass().getName());
        try {
            c.close();
            System.out.println(label + ".close=ok");
        } catch (Throwable t) {
            System.out.println(label + ".close=threw " + t.getClass().getName()
                    + ": " + t.getMessage());
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println(label + ".at " + e);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Process p = new ProcessBuilder("/bin/sleep", "5").start();
        tryClose("stdin", p.getOutputStream());
        tryClose("stdout", p.getInputStream());
        tryClose("stderr", p.getErrorStream());
        p.destroyForcibly();
        System.out.println("exit=" + p.waitFor());
        System.out.println("DONE");
    }
}
