// Interpreter round i5 wave 101, lane L1: the IOException texts of a failed
// ProcessBuilder.start(), and their cause, against HotSpot 25 on the same host.
//
// The JDK's ProcessBuilder.start wraps whatever its spawn native throws as
//   Cannot run program "<prog>"[ (in directory "<dir>")]: <native message>
// with the native's IOException as the cause. The native's message is the OS
// half only: Windows `CreateProcess error=<code>, <FormatMessage text without
// the final period>`, UNIX `error=<errno>, <strerror>`. CratonVM's natives
// answered `ProcessBuilder.start failed: program="...", args=[...]: <Rust
// error>` (and the empty-program arm a whole `Cannot run program "": ...`,
// which the JDK wrapped a second time). Fixed in native-io/src/process.rs
// (`spawn_failure_detail`, `spawn_child(.., jdk_wraps)`), not built or run.
//
// Directory paths are printed as <dir>. The OS texts are the English ones
// (the Windows texts are the system's FormatMessage texts in its default
// language; the block below is an English host's. Measured on a Russian-locale
// host in the w101 build: the append row has NO `CreateFile error=N, ` prefix).
//
// HOTSPOT_EXPECTED_BEGIN (Windows 11, English; derived from ProcessImpl_md.c, not measured)
// missing: java.io.IOException: Cannot run program "cratonvm-w101-no-such-program": CreateProcess error=2, The system cannot find the file specified
// missing.cause: java.io.IOException: CreateProcess error=2, The system cannot find the file specified
// missing-in-dir: java.io.IOException: Cannot run program "cratonvm-w101-no-such-program" (in directory "<dir>"): CreateProcess error=2, The system cannot find the file specified
// bad-dir: java.io.IOException: Cannot run program "cmd" (in directory "<dir>"): CreateProcess error=267, The directory name is invalid
// append-bad-dir: java.io.IOException: Cannot run program "cmd": The system cannot find the path specified
// empty: java.io.IOException: Cannot run program "": CreateProcess error=87, The parameter is incorrect
// done
// HOTSPOT_EXPECTED_END
//
// Linux (wave 102: MEASURED on OpenJDK 25.0.4 -- the OS half is
// `Exec failed, error: <n> (<strerror>) `, trailing space; rows below as corrected):
// missing: java.io.IOException: Cannot run program "cratonvm-w101-no-such-program": Exec failed, error: 2 (No such file or directory) 
// missing.cause: java.io.IOException: Exec failed, error: 2 (No such file or directory) 
// missing-in-dir: java.io.IOException: Cannot run program "cratonvm-w101-no-such-program" (in directory "<dir>"): Exec failed, error: 2 (No such file or directory) 
// bad-dir: java.io.IOException: Cannot run program "sh" (in directory "<dir>"): Exec failed, error: 2 (No such file or directory) 
// append-bad-dir: java.io.IOException: Cannot run program "sh": <dir>/x.log (No such file or directory)
// empty: java.io.IOException: Cannot run program "": Exec failed, error: 2 (No such file or directory) 
// done
import java.io.File;
import java.io.IOException;
import java.nio.file.Files;

public class L1W101ProcessSpawnFailureTexts {
    static final boolean WIN = System.getProperty("os.name").startsWith("Windows");

    static String run(ProcessBuilder pb, String dir) {
        try {
            Process p = pb.start();
            p.destroyForcibly();
            return "started";
        } catch (IOException e) {
            String text = e.getClass().getName() + ": " + e.getMessage();
            return dir == null ? text : text.replace(dir, "<dir>");
        }
    }

    static String cause(ProcessBuilder pb) {
        try {
            Process p = pb.start();
            p.destroyForcibly();
            return "started";
        } catch (IOException e) {
            Throwable c = e.getCause();
            return c == null ? "null" : c.getClass().getName() + ": " + c.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        String missing = "cratonvm-w101-no-such-program";
        System.out.println("missing: " + run(new ProcessBuilder(missing), null));
        System.out.println("missing.cause: " + cause(new ProcessBuilder(missing)));

        File tmp = Files.createTempDirectory("l1w101").toFile();
        try {
            System.out.println("missing-in-dir: "
                    + run(new ProcessBuilder(missing).directory(tmp), tmp.toString()));
        } finally {
            tmp.delete();
        }

        File gone = new File(tmp, "not-there");
        ProcessBuilder badDir = WIN
                ? new ProcessBuilder("cmd", "/c", "exit", "0")
                : new ProcessBuilder("sh", "-c", "exit 0");
        System.out.println("bad-dir: " + run(badDir.directory(gone), gone.toString()));

        // Windows opens an append redirect through the native
        // ProcessImpl.openForAtomicAppend (its `CreateFile error=` text); UNIX
        // through FileOutputStream.
        ProcessBuilder append = WIN
                ? new ProcessBuilder("cmd", "/c", "exit", "0")
                : new ProcessBuilder("sh", "-c", "exit 0");
        append.redirectOutput(ProcessBuilder.Redirect.appendTo(new File(gone, "x.log")));
        System.out.println("append-bad-dir: " + run(append, gone.toString()));

        System.out.println("empty: " + run(new ProcessBuilder(""), null));
        System.out.println("done");
    }
}
