import java.nio.file.*;
import java.io.*;
import java.nio.charset.StandardCharsets;

/** Repro for SC-resource-io-family Cause C: Files.newOutputStream typed exceptions
 *  (AccessDeniedException on a directory; NoSuchFileException on a missing parent),
 *  plus a normal write regression guard. */
public class NewOutputStreamRepro {
    static int pass = 0, fail = 0;
    static void check(String name, boolean ok, String detail) {
        if (ok) pass++; else fail++;
        System.out.println((ok ? "PASS " : "FAIL ") + name + (detail == null ? "" : " :: " + detail));
    }

    public static void main(String[] a) throws Exception {
        // Directory → AccessDeniedException (a FileSystemException/IOException subtype).
        Path dir = Files.createTempDirectory("nosdir");
        try (OutputStream os = Files.newOutputStream(dir)) {
            check("dir open throws", false, "no exception");
        } catch (Exception e) {
            check("dir open == AccessDeniedException", e instanceof java.nio.file.AccessDeniedException,
                    e.getClass().getName());
        }

        // Missing parent directory → NoSuchFileException.
        Path missingParent = dir.resolve("no-such-subdir").resolve("file.txt");
        try (OutputStream os = Files.newOutputStream(missingParent)) {
            check("missing-parent open throws", false, "no exception");
        } catch (Exception e) {
            check("missing-parent == NoSuchFileException", e instanceof java.nio.file.NoSuchFileException,
                    e.getClass().getName());
        }

        // Regression: normal write to a real file works.
        Path f = Files.createTempFile("nos", ".txt");
        try (OutputStream os = Files.newOutputStream(f)) {
            os.write("hello".getBytes(StandardCharsets.UTF_8));
        }
        check("normal write works", "hello".equals(Files.readString(f)), Files.readString(f));

        Files.deleteIfExists(f);
        Files.deleteIfExists(dir);
        System.out.println("RESULT pass=" + pass + " fail=" + fail);
        if (fail != 0) System.exit(1);
    }
}
