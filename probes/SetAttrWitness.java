import java.io.File;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.attribute.FileTime;
import java.nio.file.spi.FileSystemProvider;

/**
 * Pure-JDK witness for the `Files.setAttribute` AbstractMethodError.
 * No H2 on the classpath. Portable: the dos: view is only asked for on
 * Windows, the basic: view on every platform.
 */
public final class SetAttrWitness {

    private static boolean windows() {
        return System.getProperty("os.name", "").toLowerCase().contains("win");
    }

    public static void main(String[] args) throws Exception {
        File tmp = File.createTempFile("setattrwitness", ".tmp");
        tmp.deleteOnExit();
        Path p = tmp.toPath();
        System.out.println("os=" + System.getProperty("os.name") + " file=" + p);

        FileSystemProvider prov = p.getFileSystem().provider();
        System.out.println("provider=" + prov.getClass().getName());
        for (Class<?> c = prov.getClass(); c != null; c = c.getSuperclass()) {
            StringBuilder sb = new StringBuilder("  chain: " + c.getName());
            try {
                java.lang.reflect.Method m = c.getDeclaredMethod("setAttribute",
                        Path.class, String.class, Object.class, LinkOption[].class);
                sb.append("  DECLARES setAttribute mods=0x")
                  .append(Integer.toHexString(m.getModifiers()));
            } catch (NoSuchMethodException e) {
                sb.append("  (no setAttribute)");
            }
            System.out.println(sb);
        }

        // A. basic:lastModifiedTime — supported on every platform.
        FileTime want = FileTime.fromMillis(1234567890000L);
        step("A1 set basic:lastModifiedTime", () -> Files.setAttribute(p, "basic:lastModifiedTime", want));
        step("A2 get basic:lastModifiedTime", () -> Files.getAttribute(p, "basic:lastModifiedTime"));
        step("A3 lastModified()", () -> tmp.lastModified());

        // B. dos:readonly — Windows only; on Linux HotSpot answers
        //    UnsupportedOperationException, which is itself the right answer.
        step("B1 set dos:readonly=true", () -> {
            Files.setAttribute(p, "dos:readonly", Boolean.TRUE);
            return "void";
        });
        step("B2 provider.setAttribute dos:readonly=true", () -> {
            prov.setAttribute(p, "dos:readonly", Boolean.TRUE, new LinkOption[0]);
            return "void";
        });
        step("B3 get dos:readonly", () -> Files.getAttribute(p, "dos:readonly"));
        step("B4 canWrite", () -> tmp.canWrite());

        // C. What H2's FileUtils.setReadOnly ends up asking for.
        step("C1 File.setReadOnly", () -> tmp.setReadOnly());
        step("C2 canWrite after setReadOnly", () -> tmp.canWrite());

        // Restore writability so deleteOnExit can clean up.
        try {
            if (windows()) {
                Files.setAttribute(p, "dos:readonly", Boolean.FALSE);
            } else {
                Files.setPosixFilePermissions(p,
                        java.nio.file.attribute.PosixFilePermissions.fromString("rw-r--r--"));
            }
        } catch (Throwable ignored) {
            // best effort
        }
        System.out.println("DONE");
    }

    private interface Step {
        Object run() throws Throwable;
    }

    private static void step(String label, Step s) {
        try {
            System.out.println(label + " OK -> " + s.run());
        } catch (Throwable t) {
            System.out.println(label + " FAILED: " + t);
        }
    }
}
