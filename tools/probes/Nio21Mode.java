import java.nio.file.*;
import java.nio.file.attribute.*;
import java.util.*;
public class Nio21Mode {
    public static void main(String[] a) throws Exception {
        Path d = Files.createTempDirectory("m21");
        boolean win = System.getProperty("os.name").startsWith("Windows");
        if (!win) {
            Path f = d.resolve("f");
            Files.createFile(f, PosixFilePermissions.asFileAttribute(PosixFilePermissions.fromString("rw-r-----")));
            System.out.println("createFile " + PosixFilePermissions.toString(Files.getPosixFilePermissions(f)));
            Path g = d.resolve("g");
            Files.newByteChannel(g, EnumSet.of(StandardOpenOption.CREATE_NEW, StandardOpenOption.WRITE),
                PosixFilePermissions.asFileAttribute(PosixFilePermissions.fromString("rw-------"))).close();
            System.out.println("newByteChannel " + PosixFilePermissions.toString(Files.getPosixFilePermissions(g)));
            Path h = d.resolve("h");
            Files.createFile(h);
            System.out.println("plain " + PosixFilePermissions.toString(Files.getPosixFilePermissions(h)));
        } else {
            Path f = d.resolve("f");
            Files.createFile(f);
            System.out.println(new TreeSet<>(Files.readAttributes(f, "dos:*").keySet()));
            System.out.println(Files.getAttribute(f, "dos:attributes").getClass().getName());
            Files.setAttribute(f, "dos:hidden", true);
            System.out.println(((Integer) Files.getAttribute(f, "dos:attributes") & 2) != 0);
        }
    }
}
