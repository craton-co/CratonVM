import java.nio.file.*;
public class PathProbe {
  static void show(String label, Path p) {
    StringBuilder names = new StringBuilder();
    for (int i = 0; i < p.getNameCount(); i++) names.append(p.getName(i)).append(i<p.getNameCount()-1?",":"");
    System.out.println(label + " | str=" + p + " | root=" + p.getRoot() + " | nc=" + p.getNameCount()
        + " | names=[" + names + "] | parent=" + p.getParent() + " | abs=" + p.isAbsolute());
  }
  public static void main(String[] a) {
    show("drive-abs ", Paths.get("C:\\foo\\bar"));
    show("drive-fwd ", Paths.get("C:/foo/bar"));
    show("drive-rel ", Paths.get("C:foo"));
    show("unc       ", Paths.get("\\\\server\\share\\d\\e"));
    show("root-rel  ", Paths.get("\\foo\\bar"));
    show("relative  ", Paths.get("a\\b\\c"));
    show("dot-abs   ", Paths.get(".").toAbsolutePath());
    show("of-slash-x", Path.of("/", "dummy-resources/parent"));
  }
}
