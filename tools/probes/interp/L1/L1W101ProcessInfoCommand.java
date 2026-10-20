// Interpreter round i5 wave 101, lane L1: ProcessHandle.Info of a child this
// VM spawned, against HotSpot 25 on the same host.
//
// Linux: HotSpot's `info0` (ProcessHandleImpl_linux.c) reads `command` from
// the link /proc/<pid>/exe -- the RESOLVED executable (`/usr/bin/dash` for a
// child started as `sh` on Ubuntu) -- and `arguments` from argv[1..];
// `Info.commandLine()` joins the two. CratonVM answered argv[0] (`sh`) as the
// command. Fixed in native-io/src/process.rs (`os_process_cmdline`,
// `OsCmdline`), not built or run.
//
// Windows: `command` is the image path (QueryFullProcessImageNameW), while
// `arguments` and `commandLine` are empty (HotSpot reads no command line of
// another process there).
//
// The child is `sh -c "sleep 30; true"` (the `; true` keeps the shell from
// exec-ing `sleep` in place) or `cmd /c ping -n 30 127.0.0.1`; it is killed
// at the end.
//
// HOTSPOT_EXPECTED_BEGIN (Windows 11; derived, not measured)
// command.present=true
// command.is-shell=true
// arguments=empty
// commandLine.present=false
// user.present=true
// start.present=true
// done
// HOTSPOT_EXPECTED_END
//
// Linux (derived from ProcessHandleImpl_linux.c / ProcessHandleImpl_unix.c, not measured):
// command.present=true
// command.is-shell=true
// arguments=[-c, sleep 30; true]
// commandLine.present=true
// commandLine.is-joined=true
// user.present=true
// start.present=true
// done
import java.nio.file.Path;
import java.util.Arrays;
import java.util.Optional;

public class L1W101ProcessInfoCommand {
    static final boolean WIN = System.getProperty("os.name").startsWith("Windows");

    public static void main(String[] args) throws Exception {
        ProcessBuilder pb = WIN
                ? new ProcessBuilder("cmd", "/c", "ping", "-n", "30", "127.0.0.1")
                : new ProcessBuilder("sh", "-c", "sleep 30; true");
        pb.redirectOutput(ProcessBuilder.Redirect.DISCARD);
        pb.redirectError(ProcessBuilder.Redirect.DISCARD);
        Process p = pb.start();
        try {
            // Let the child finish exec (its /proc record is the parent's before).
            Thread.sleep(300);
            ProcessHandle.Info info = p.toHandle().info();
            Optional<String> command = info.command();
            System.out.println("command.present=" + command.isPresent());
            String expected = WIN
                    ? null
                    : Path.of("/bin/sh").toRealPath().toString();
            boolean shell = command.isPresent()
                    && (WIN
                            ? command.get().toLowerCase().endsWith("\\cmd.exe")
                            : command.get().equals(expected));
            System.out.println("command.is-shell=" + shell);
            System.out.println("arguments=" + info.arguments().map(Arrays::toString).orElse("empty"));
            Optional<String> line = info.commandLine();
            System.out.println("commandLine.present=" + line.isPresent());
            if (line.isPresent()) {
                System.out.println("commandLine.is-joined="
                        + line.get().equals(command.orElse("?") + " -c sleep 30; true"));
            }
            System.out.println("user.present=" + info.user().isPresent());
            System.out.println("start.present=" + info.startInstant().isPresent());
        } finally {
            p.descendants().forEach(ProcessHandle::destroyForcibly);
            p.destroyForcibly();
            p.waitFor();
        }
        System.out.println("done");
    }
}
