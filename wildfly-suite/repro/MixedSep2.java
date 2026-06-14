import java.nio.file.*;
public class MixedSep2 {
  public static void main(String[] a) throws Exception {
    Path p = Paths.get(a[0]);
    System.out.println("getNameCount     = " + p.getNameCount());
    System.out.println("normalize        = " + p.normalize());
    System.out.println("toAbsolute.norm  = " + p.toAbsolutePath().normalize());
    Path jm = p.normalize().resolve("jboss-modules.jar");
    System.out.println("norm.resolve(jm) = " + jm);
    System.out.println("notExists(norm)  = " + Files.notExists(jm));
    System.out.println("exists(toReal?)  = " + Files.exists(p.toAbsolutePath().normalize().resolve("jboss-modules.jar")));
  }
}
