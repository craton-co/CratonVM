// Interpreter round i4 wave 98, lane L1: the java.lang.Process /
// ProcessHandle answers a program can read without timing, for a child this
// VM spawned, the current process, and pids it did not spawn (0, the
// Windows System process 4 / Linux init 1, an exited child, Long.MAX_VALUE).
// Children: Windows `ping -n 30 127.0.0.1` and `cmd /c exit 3`; elsewhere
// `sleep 30` and `sh -c "exit 3"`. Platform-dependent rows are compared on
// the same host (HotSpot vs CratonVM), so the block below is the Windows one.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags, Windows 11)
// of(0)=false
// of(system)=false
// of(MAX)=false
// current: alive=true supportsNormalTermination=false parent=true
// current.info: command=true arguments=false start=true cpu=true user=true
// current.destroy: java.lang.IllegalStateException: destroy of current process not allowed
// current.destroyForcibly: java.lang.IllegalStateException: destroy of current process not allowed
// child: alive=true handle-equals-of=true in-children=true info.command=true children=1
// child.exitValue: java.lang.IllegalThreadStateException: process has not exited
// child.toString: Process[pid=<pid>, exitValue="not exited"]
// child.handle.toString=<pid>
// child.destroy: supportsNormalTermination=false
// child.waitFor=1 alive=false of(pid)=false
// child.toString after: Process[pid=<pid>, exitValue=1]
// exit3: waitFor=3 exitValue=3 onExit-same=true handle.onExit-pid-same=true
// exit3.handle.isAlive=false destroy-after-exit=false
// done
// HOTSPOT_EXPECTED_END
import java.util.List;
import java.util.Optional;
import java.util.concurrent.TimeUnit;

public class L1W98ProcessHandleAnswers {
    static final boolean WIN = System.getProperty("os.name").startsWith("Windows");

    static String thrown(Runnable r) {
        try {
            r.run();
            return "none";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("of(0)=" + ProcessHandle.of(0).isPresent());
        System.out.println("of(system)=" + ProcessHandle.of(WIN ? 4 : 1).isPresent());
        System.out.println("of(MAX)=" + ProcessHandle.of(Long.MAX_VALUE).isPresent());

        ProcessHandle cur = ProcessHandle.current();
        System.out.println("current: alive=" + cur.isAlive()
                + " supportsNormalTermination=" + cur.supportsNormalTermination()
                + " parent=" + cur.parent().isPresent());
        ProcessHandle.Info ci = cur.info();
        System.out.println("current.info: command=" + ci.command().isPresent()
                + " arguments=" + ci.arguments().isPresent()
                + " start=" + ci.startInstant().isPresent()
                + " cpu=" + ci.totalCpuDuration().isPresent()
                + " user=" + ci.user().isPresent());
        System.out.println("current.destroy: " + thrown(() -> cur.destroy()));
        System.out.println("current.destroyForcibly: " + thrown(() -> cur.destroyForcibly()));

        List<String> longCmd = WIN ? List.of("ping", "-n", "30", "127.0.0.1") : List.of("sleep", "30");
        Process p = new ProcessBuilder(longCmd)
                .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start();
        long pid = p.pid();
        Optional<ProcessHandle> byPid = ProcessHandle.of(pid);
        boolean inChildren = cur.children().anyMatch(h -> h.pid() == pid);
        System.out.println("child: alive=" + p.isAlive()
                + " handle-equals-of=" + (byPid.isPresent() && byPid.get().equals(p.toHandle()))
                + " in-children=" + inChildren
                + " info.command=" + p.info().command().isPresent()
                + " children=" + p.children().count());
        System.out.println("child.exitValue: " + thrown(() -> p.exitValue()));
        System.out.println("child.toString: " + p.toString().replace(Long.toString(pid), "<pid>"));
        System.out.println("child.handle.toString="
                + p.toHandle().toString().replace(Long.toString(pid), "<pid>"));
        System.out.println("child.destroy: supportsNormalTermination=" + p.supportsNormalTermination());
        p.destroy();
        boolean done = p.waitFor(30, TimeUnit.SECONDS);
        int code = done ? p.exitValue() : -999;
        // Windows: TerminateProcess(h, 1); UNIX: SIGTERM -> 143.
        int shown = (!WIN && code == 143) ? 1 : code;
        System.out.println("child.waitFor=" + shown + " alive=" + p.isAlive()
                + " of(pid)=" + ProcessHandle.of(pid).isPresent());
        System.out.println("child.toString after: "
                + p.toString().replace(Long.toString(pid), "<pid>").replace("exitValue=143", "exitValue=1"));

        List<String> exitCmd = WIN ? List.of("cmd", "/c", "exit 3") : List.of("sh", "-c", "exit 3");
        Process q = new ProcessBuilder(exitCmd).start();
        ProcessHandle qh = q.toHandle();
        int w = q.waitFor();
        Process same = q.onExit().get(30, TimeUnit.SECONDS);
        ProcessHandle hs = qh.onExit().get(30, TimeUnit.SECONDS);
        System.out.println("exit3: waitFor=" + w + " exitValue=" + q.exitValue()
                + " onExit-same=" + (same == q)
                + " handle.onExit-pid-same=" + (hs.pid() == q.pid()));
        System.out.println("exit3.handle.isAlive=" + qh.isAlive()
                + " destroy-after-exit=" + qh.destroy());
        System.out.println("done");
    }
}
