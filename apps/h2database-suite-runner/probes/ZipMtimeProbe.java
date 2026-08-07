import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

/**
 * What a zip entry's `lastModifiedTime` reads as BEFORE anything writes it.
 * `ZipAttrProbe` sets it first, so its readback cannot show whether the entry's
 * own stored timestamp is being reported or the containing archive's.
 */
public class ZipMtimeProbe {

    public static void main(String... args) throws Exception {
        Path zip = Files.createTempFile("zipmtime", ".zip");
        Files.delete(zip);
        try (ZipOutputStream out = new ZipOutputStream(Files.newOutputStream(zip))) {
            ZipEntry e = new ZipEntry("hello.txt");
            // A fixed, obviously-not-now stamp so the two candidates are
            // distinguishable: 2001-01-01T00:00:00Z.
            e.setTime(978_307_200_000L);
            out.putNextEntry(e);
            out.write("hello".getBytes("UTF-8"));
            out.closeEntry();
        }
        System.out.println("archive mtime = " + Files.getLastModifiedTime(zip));

        try (FileSystem fs = FileSystems.newFileSystem(URI.create("jar:" + zip.toUri()),
                new HashMap<String, String>())) {
            Path entry = fs.getPath("/hello.txt");
            System.out.println("entry   mtime = " + Files.getAttribute(entry, "lastModifiedTime"));
            System.out.println("entry   size  = " + Files.getAttribute(entry, "size"));
            System.out.println("entry   isDir = " + Files.getAttribute(entry, "isDirectory"));
        }
        Files.deleteIfExists(zip);
        System.out.println("=== DONE");
    }
}
