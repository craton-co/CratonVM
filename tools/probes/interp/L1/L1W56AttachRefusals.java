// Interpreter round i2 wave 56, lane L1: what `VirtualMachine.attach(id)`
// throws for an id that is no process this VM can attach to, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-in-process-attach-answers-any-pid-and-ignores-allowattachself-FIXED-20261020.md).
//
// Rows: a null id; an id that is no number; this process's pid and pid 0,
// each checked against what HotSpot's `HotSpotVirtualMachine` does for the
// saved `jdk.attach.allowAttachSelf` this run sees (attached when it is ""
// or "true", else `IOException("Can not attach to current VM")`; CratonVM
// sets the property to "true" by default, HotSpot leaves it unset, so the
// row prints only whether the answer agrees with the property); a live
// process that is no JVM (`sleep` / `ping`), its exception's class only (the
// message is the platform's, and HotSpot on Linux waits ~10 s for it). Each
// row twice (the first-call row again).
//
// Run:
//   javac -d /tmp/l1w56attach tools/probes/interp/L1/L1W56AttachRefusals.java
//   java     -cp /tmp/l1w56attach L1W56AttachRefusals
//   cratonvm -cp /tmp/l1w56attach L1W56AttachRefusals
//
// Expected stdout (HotSpot 25.0.3, default flags): MEASURED on the Windows
// box, three runs, the same each time:
//
//   null id: java.lang.NullPointerException [id cannot be null]
//   not a number: com.sun.tools.attach.AttachNotSupportedException [Invalid process identifier: abc]
//   own pid: agrees with the property
//   pid 0: agrees with the property
//   another process: com.sun.tools.attach.AttachNotSupportedException
//   (the five rows again, the same)
//
// CratonVM before wave 56 (MEASURED, debug build, Windows, both modes):
// `null id: attached`, `not a number: attached`, `pid 0: differs: attached`,
// `another process: attached` (the in-process handle answered every id). Since wave 56 (`CRATONVM_ATTACH_CHECKS` on, the
// default from the lane's last commit): HotSpot's lines
// (`runtime::instrument::attach_refusal`).
import com.sun.tools.attach.VirtualMachine;

public class L1W56AttachRefusals {
    static String attempt(String id) {
        try {
            VirtualMachine vm = VirtualMachine.attach(id);
            vm.detach();
            return "attached";
        } catch (Throwable t) {
            return t.getClass().getName() + " [" + t.getMessage() + "]";
        }
    }

    static String classOnly(String answer) {
        int at = answer.indexOf(" [");
        return at < 0 ? answer : answer.substring(0, at);
    }

    static Process otherProcess() throws Exception {
        boolean windows = System.getProperty("os.name").toLowerCase().contains("win");
        ProcessBuilder pb = windows
                ? new ProcessBuilder("ping", "-n", "60", "127.0.0.1")
                : new ProcessBuilder("sh", "-c", "trap '' QUIT; echo ready; exec sleep 60");
        if (windows) pb.redirectOutput(ProcessBuilder.Redirect.DISCARD);
        pb.redirectError(ProcessBuilder.Redirect.DISCARD);
        Process child = pb.start();
        // The Unix attach client sends SIGQUIT. Keep the non-JVM target alive
        // and wait until its ignored disposition is installed: otherwise a
        // timing-dependent process disappearance changes HotSpot's exception.
        if (!windows) {
            try (var output = new java.io.BufferedReader(new java.io.InputStreamReader(child.getInputStream()))) {
                if (!"ready".equals(output.readLine())) {
                    child.destroyForcibly();
                    child.waitFor();
                    throw new IllegalStateException("non-JVM attach target did not become ready");
                }
            }
        }
        return child;
    }

    static void rows() throws Exception {
        String property = System.getProperty("jdk.attach.allowAttachSelf");
        boolean allowed = "".equals(property) || Boolean.parseBoolean(property);
        System.out.println("null id: " + attempt(null));
        System.out.println("not a number: " + attempt("abc"));
        String own = attempt(Long.toString(ProcessHandle.current().pid()));
        boolean ownAgrees = allowed
                ? own.equals("attached")
                : own.equals("java.io.IOException [Can not attach to current VM]");
        System.out.println("own pid: " + (ownAgrees ? "agrees with the property" : "differs: " + own));
        String zero = attempt("0");
        boolean zeroAgrees = allowed
                ? !zero.equals("attached")
                : zero.equals("java.io.IOException [Can not attach to current VM]");
        System.out.println("pid 0: " + (zeroAgrees ? "agrees with the property" : "differs: " + zero));
        Process other = otherProcess();
        try {
            System.out.println("another process: " + classOnly(attempt(Long.toString(other.pid()))));
        } finally {
            other.destroyForcibly();
            other.waitFor();
        }
    }

    public static void main(String[] args) throws Exception {
        rows();
        rows();
    }
}
