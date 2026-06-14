import java.util.zip.ZipFile;
public class ZipProbe {
  public static void main(String[] a) throws Exception {
    ZipFile zf = new ZipFile(a[0]);
    System.out.println("getName -> " + zf.getName());
    System.out.println("size -> " + zf.size());
    System.out.println("getEntry(MANIFEST) -> " + zf.getEntry("META-INF/MANIFEST.MF"));
    System.out.println("entries -> " + (zf.entries()==null?"NULL":"ok"));
  }
}
