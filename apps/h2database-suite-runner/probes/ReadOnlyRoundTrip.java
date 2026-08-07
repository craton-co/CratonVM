import java.io.File;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * The exact sequence `TestFileSystem.testSetReadOnly` performs, minus H2:
 * create, mark read-only through `Files.setAttribute`, assert `canWrite()` is
 * false, delete. Step three is the interesting one on Windows — `DeleteFileW`
 * refuses a FILE_ATTRIBUTE_READONLY file — so this establishes what the test is
 * entitled to expect before blaming the VM for the last step.
 */
public class ReadOnlyRoundTrip {

    public static void main(String... args) throws Exception {
        Path f = Files.createTempFile("rort", ".tmp");
        System.out.println("file=" + f);
        boolean win = System.getProperty("os.name", "").toLowerCase().contains("win");

        try {
            if (win) {
                Files.setAttribute(f, "dos:readonly", Boolean.TRUE);
            } else {
                Files.setPosixFilePermissions(f, java.nio.file.attribute.PosixFilePermissions
                        .fromString("r--r--r--"));
            }
            System.out.println("OK   setReadOnly");
        } catch (Throwable t) {
            System.out.println("FAIL setReadOnly -> " + t);
        }

        System.out.println("     canWrite=" + new File(f.toString()).canWrite()
                + "  (test asserts false)");

        try {
            boolean gone = Files.deleteIfExists(f);
            System.out.println("OK   delete gone=" + gone);
        } catch (Throwable t) {
            System.out.println("FAIL delete -> " + t);
        }
        System.out.println("     stillExists=" + Files.exists(f));

        // Clean up whatever state we are in.
        try {
            if (Files.exists(f)) {
                if (win) {
                    Files.setAttribute(f, "dos:readonly", Boolean.FALSE);
                }
                Files.deleteIfExists(f);
            }
        } catch (Throwable ignored) {
            // best effort
        }
        System.out.println("=== DONE");
    }
}
