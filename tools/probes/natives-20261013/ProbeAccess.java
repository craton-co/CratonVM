import java.nio.file.*;
import java.nio.file.attribute.*;
import java.io.*;

public class ProbeAccess {
    static void line(String label, Path p) {
        String r;
        try {
            r = "R=" + Files.isReadable(p) + " W=" + Files.isWritable(p) + " X=" + Files.isExecutable(p)
              + " E=" + Files.exists(p) + " D=" + Files.isDirectory(p);
        } catch (Throwable t) { r = "THROW " + t; }
        System.out.println(label + ": " + r);
    }
    static void check(String label, Path p, AccessMode... modes) {
        try { p.getFileSystem().provider().checkAccess(p, modes); System.out.println(label + ": ok " + java.util.Arrays.toString(modes)); }
        catch (Throwable t) { System.out.println(label + ": " + t.getClass().getName() + ": " + t.getMessage()); }
    }
    public static void main(String[] a) throws Exception {
        Path tmp = Files.createTempFile("pa", ".txt");
        Files.writeString(tmp, "x");
        Path dir = Files.createTempDirectory("pad");
        line("tmpfile", tmp);
        line("tmpdir", dir);
        line("missing", dir.resolve("nope"));
        line("windir", Paths.get("C:/Windows"));
        line("notepad", Paths.get("C:/Windows/System32/notepad.exe"));
        line("hosts", Paths.get("C:/Windows/System32/drivers/etc/hosts"));
        line("cmd", Paths.get("C:/Windows/System32/cmd.exe"));
        line("pagefile", Paths.get("C:/pagefile.sys"));
        line("sysvolinfo", Paths.get("C:/System Volume Information"));
        line("root", Paths.get("C:/"));
        check("tmpfile", tmp, AccessMode.READ, AccessMode.WRITE);
        check("tmpfile-x", tmp, AccessMode.EXECUTE);
        check("windir-w", Paths.get("C:/Windows/System32/cmd.exe"), AccessMode.WRITE);
        check("missing", dir.resolve("nope"), AccessMode.READ);
        check("sysvol-r", Paths.get("C:/System Volume Information"), AccessMode.READ);
        // read-only file
        Path ro = Files.createTempFile("pro", ".txt");
        ro.toFile().setReadOnly();
        line("readonly", ro);
        check("readonly-w", ro, AccessMode.WRITE);
        Files.copy(tmp, dir.resolve("cp.txt"), StandardCopyOption.COPY_ATTRIBUTES);
        System.out.println("copy-attr ok");
        ro.toFile().setWritable(true);
        Files.delete(ro); Files.delete(tmp); Files.delete(dir.resolve("cp.txt")); Files.delete(dir);
    }
}
