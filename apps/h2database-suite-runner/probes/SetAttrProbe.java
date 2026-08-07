import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.spi.FileSystemProvider;

/**
 * Pure-JDK witness for the Windows `Files.setAttribute` AbstractMethodError.
 * No H2, no test framework: `Files.setAttribute` resolves the provider from the
 * path and calls `provider.setAttribute(...)`, which `FileSystemProvider`
 * declares abstract and every concrete provider overrides. Each step below is
 * printed separately so a failure names which one broke.
 */
public class SetAttrProbe {

    public static void main(String... args) throws Exception {
        Path file = Files.createTempFile("setattr", ".tmp");
        file.toFile().deleteOnExit();
        System.out.println("file=" + file);

        FileSystemProvider p = file.getFileSystem().provider();
        System.out.println("provider.class=" + p.getClass().getName());
        System.out.println("provider.scheme=" + p.getScheme());

        // 1. The override exists, and reflection finds it on the concrete class.
        try {
            java.lang.reflect.Method m = p.getClass().getMethod("setAttribute",
                    Path.class, String.class, Object.class, LinkOption[].class);
            System.out.println("reflected declaring=" + m.getDeclaringClass().getName()
                    + " abstract=" + java.lang.reflect.Modifier.isAbstract(m.getModifiers()));
        } catch (Throwable t) {
            System.out.println("reflect FAILED: " + t);
        }

        // 2. The same call through a FileSystemProvider-typed receiver, which is
        //    what java.nio.file.Files does.
        step("provider.setAttribute via FileSystemProvider-typed local", () -> {
            p.setAttribute(file, attrName(), attrValue());
        });

        // 3. The call Files.setAttribute makes.
        step("Files.setAttribute", () -> {
            Files.setAttribute(file, attrName(), attrValue());
        });

        // 4. Read it back so a silent no-op is not mistaken for a pass.
        step("Files.getAttribute readback", () -> {
            System.out.println("    readback " + attrName() + "="
                    + Files.getAttribute(file, attrName()));
        });

        // 5. The sibling the same dispatch question applies to.
        step("Files.readAttributes(BasicFileAttributes)", () -> {
            System.out.println("    size="
                    + Files.readAttributes(file, java.nio.file.attribute.BasicFileAttributes.class).size());
        });

        // Leave the file deletable.
        try {
            Files.setAttribute(file, attrName(), Boolean.FALSE);
        } catch (Throwable ignored) {
            // best effort
        }
        System.out.println("=== DONE");
    }

    private static String attrName() {
        return isWindows() ? "dos:readonly" : "unix:mode";
    }

    private static Object attrValue() {
        return isWindows() ? Boolean.TRUE : Integer.valueOf(0644);
    }

    private static boolean isWindows() {
        return System.getProperty("os.name", "").toLowerCase().contains("win");
    }

    private interface Step {
        void run() throws IOException;
    }

    private static void step(String label, Step s) {
        try {
            s.run();
            System.out.println("OK   " + label);
        } catch (Throwable t) {
            System.out.println("FAIL " + label + " -> " + t);
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println("       at " + e);
            }
        }
        System.out.flush();
    }
}
