package cratonvm;
import java.io.*;
import java.nio.file.*;
public class TckFiles {
    public static int files_createTempFile() {
        try {
            Path p = Files.createTempFile("tck", ".tmp");
            try { return Files.exists(p) ? 1 : 0; }
            finally { Files.deleteIfExists(p); }
        } catch (Exception e) { return 0; }
    }
    public static int files_write_readAllBytes() {
        try {
            Path p = Files.createTempFile("tck", ".tmp");
            try {
                byte[] data = {1, 2, 3, 4, 5};
                Files.write(p, data);
                byte[] read = Files.readAllBytes(p);
                if (read.length != 5) return 0;
                for (int i = 0; i < 5; i++) if (read[i] != data[i]) return 0;
                return 1;
            } finally { Files.deleteIfExists(p); }
        } catch (Exception e) { return 0; }
    }
    public static int files_delete() {
        try {
            Path p = Files.createTempFile("tck", ".tmp");
            Files.delete(p);
            return !Files.exists(p) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int files_isDirectory() {
        try {
            Path tmp = Files.createTempDirectory("tckdir");
            try { return Files.isDirectory(tmp) ? 1 : 0; }
            finally { Files.deleteIfExists(tmp); }
        } catch (Exception e) { return 0; }
    }
    public static int files_isRegularFile() {
        try {
            Path p = Files.createTempFile("tck", ".tmp");
            try { return Files.isRegularFile(p) ? 1 : 0; }
            finally { Files.deleteIfExists(p); }
        } catch (Exception e) { return 0; }
    }
    public static int files_size() {
        try {
            Path p = Files.createTempFile("tck", ".tmp");
            try {
                Files.write(p, new byte[42]);
                return Files.size(p) == 42 ? 1 : 0;
            } finally { Files.deleteIfExists(p); }
        } catch (Exception e) { return 0; }
    }
    public static int files_copy() {
        try {
            Path src = Files.createTempFile("tck_src", ".tmp");
            Path dst = Path.of(src.toString() + ".copy");
            try {
                Files.write(src, new byte[]{1, 2, 3});
                Files.copy(src, dst, StandardCopyOption.REPLACE_EXISTING);
                byte[] read = Files.readAllBytes(dst);
                return read.length == 3 ? 1 : 0;
            } finally { Files.deleteIfExists(src); Files.deleteIfExists(dst); }
        } catch (Exception e) { return 0; }
    }
    public static int files_move() {
        try {
            Path src = Files.createTempFile("tck_mv", ".tmp");
            Path dst = Path.of(src.toString() + ".moved");
            try {
                Files.write(src, new byte[]{10, 20});
                Files.move(src, dst, StandardCopyOption.REPLACE_EXISTING);
                return (!Files.exists(src) && Files.exists(dst)) ? 1 : 0;
            } finally { Files.deleteIfExists(src); Files.deleteIfExists(dst); }
        } catch (Exception e) { return 0; }
    }
}
