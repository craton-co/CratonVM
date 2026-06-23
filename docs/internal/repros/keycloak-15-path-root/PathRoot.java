import java.nio.file.*;

// Repro for keycloak-15-windows-path-root-parsing.
// CratonVM: Path.getRoot() returns null for a Windows drive path, and
// getNameCount() counts "C:" as a name element. HotSpot returns the drive root.
public class PathRoot {
    public static void main(String[] a) {
        Path p = Paths.get("C:\\foo\\bar");
        System.out.println("path       = " + p);
        System.out.println("getRoot()  = " + p.getRoot()       + "   (HotSpot: C:\\)");
        System.out.println("nameCount  = " + p.getNameCount()  + "   (HotSpot: 2)");
        System.out.println("name(0)    = " + p.getName(0)      + "   (HotSpot: foo)");
        boolean ok = "C:\\".equals(String.valueOf(p.getRoot())) && p.getNameCount() == 2;
        System.out.println("RESULT=" + (ok ? "OK" : "FAIL"));
    }
}
