import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.FileTime;
import java.util.HashMap;
import java.util.Map;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

/**
 * A native registered on the ABSTRACT `FileSystemProvider` answers for every
 * provider, not just the default one. This checks the non-default case: a zipfs
 * entry, whose `ZipFileSystemProvider` has its own concrete `setAttribute` /
 * `readAttributes`, must still behave — if our registration hijacks them, a
 * zip entry's attributes would be read off the real filesystem instead.
 */
public class ZipAttrProbe {

    public static void main(String... args) throws Exception {
        Path zip = Files.createTempFile("zipattr", ".zip");
        Files.delete(zip);
        try (ZipOutputStream out = new ZipOutputStream(Files.newOutputStream(zip))) {
            out.putNextEntry(new ZipEntry("hello.txt"));
            out.write("hello".getBytes("UTF-8"));
            out.closeEntry();
        }
        System.out.println("zip=" + zip + " size=" + Files.size(zip));

        Map<String, String> env = new HashMap<>();
        try (FileSystem fs = FileSystems.newFileSystem(URI.create("jar:" + zip.toUri()), env)) {
            System.out.println("zipfs.provider=" + fs.provider().getClass().getName());
            Path entry = fs.getPath("/hello.txt");

            step("zip readAttributes(basic:*)", () ->
                    System.out.println("    " + Files.readAttributes(entry, "basic:size")));
            step("zip Files.size", () ->
                    System.out.println("    size=" + Files.size(entry)));
            step("zip setAttribute(lastModifiedTime)", () ->
                    Files.setAttribute(entry, "lastModifiedTime",
                            FileTime.fromMillis(1_000_000_000L)));
            step("zip getAttribute(lastModifiedTime)", () ->
                    System.out.println("    mtime=" + Files.getAttribute(entry, "lastModifiedTime")));
        }
        Files.deleteIfExists(zip);
        System.out.println("=== DONE");
    }

    private interface Step {
        void run() throws Exception;
    }

    private static void step(String label, Step s) {
        try {
            s.run();
            System.out.println("OK   " + label);
        } catch (Throwable t) {
            System.out.println("FAIL " + label + " -> " + t);
        }
        System.out.flush();
    }
}
