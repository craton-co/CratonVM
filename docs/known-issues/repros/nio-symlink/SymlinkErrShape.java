import java.nio.file.*;

/**
 * Diffs the SHAPE of the exception java.nio.file operations throw when the host
 * refuses them, rather than whether they succeed. Run under both VMs and diff.
 * Every line is paired (property + value) so a null is visible as a null.
 */
public class SymlinkErrShape {

    static void shape(String label, Runnable r) {
        try {
            r.run();
            System.out.println(label + " | NO_EXCEPTION");
        } catch (Throwable t) {
            Throwable c = (t instanceof RuntimeException && t.getCause() != null) ? t.getCause() : t;
            System.out.println(label + " | class=" + c.getClass().getName());
            System.out.println(label + " | message=" + c.getMessage());
            if (c instanceof FileSystemException) {
                FileSystemException f = (FileSystemException) c;
                System.out.println(label + " | file=" + f.getFile());
                System.out.println(label + " | otherFile=" + f.getOtherFile());
                System.out.println(label + " | reason=" + f.getReason());
            }
        }
    }

    interface IoRun { void run() throws Exception; }

    static void t(String label, IoRun r) {
        shape(label, () -> {
            try { r.run(); } catch (Exception e) { throw new RuntimeException(e); }
        });
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("symerr");
        Path target = dir.resolve("target.txt");
        Files.write(target, "hello".getBytes());
        Path targetDir = dir.resolve("targetdir");
        Files.createDirectory(targetDir);

        // The separator the VM puts in an exception is a property of the VM, not
        // of the host, so print what Path.toString() says first: on Windows these
        // two disagreed (Path correct, exception text forward-slashed) and the
        // pairing is what localizes that.
        System.out.println("pathToString | " + dir);

        t("createSymbolicLink(file)", () -> Files.createSymbolicLink(dir.resolve("l1"), target));
        t("createSymbolicLink(dir)", () -> Files.createSymbolicLink(dir.resolve("l2"), targetDir));
        t("createSymbolicLink(relative)", () -> Files.createSymbolicLink(dir.resolve("l3"), Paths.get("target.txt")));

        // Controls: failures that do NOT need a privilege, so any difference here
        // is unrelated to the symlink gap.
        t("readSymbolicLink(non-link)", () -> Files.readSymbolicLink(target));
        t("newByteChannel(missing)", () -> Files.newByteChannel(dir.resolve("nope.txt")));
        t("createDirectory(existing)", () -> Files.createDirectory(targetDir));
        t("delete(missing)", () -> Files.delete(dir.resolve("nope2.txt")));

        // A non-empty directory is DirectoryNotEmptyException, a SUBCLASS of
        // FileSystemException -- so this line distinguishes the two only if the
        // probe prints the class name, which is why `class=` is printed for
        // every case above. CratonVM raised the supertype here until 2026-08-11,
        // and a caller's `catch (DirectoryNotEmptyException)` recursion branch
        // therefore never ran.
        Files.write(targetDir.resolve("child.txt"), "child".getBytes());
        t("delete(non-empty-dir)", () -> Files.delete(targetDir));
    }
}
