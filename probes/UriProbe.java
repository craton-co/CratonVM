import java.net.URI;
public class UriProbe {
  public static void main(String[] a) throws Exception {
    URI u = new URI("file:/tmp/dir/resource%23test1.txt");
    System.out.println("getPath      = " + u.getPath());
    System.out.println("getRawPath   = " + u.getRawPath());
    URI v = new URI("file:/tmp/a%20b/c%2Bd");
    System.out.println("getPath2     = " + v.getPath());
    System.out.println("getRawPath2  = " + v.getRawPath());
    URI w = new URI("http://h/x?q%3Da#f%23g");
    System.out.println("query        = " + w.getQuery() + " raw=" + w.getRawQuery());
    System.out.println("fragment     = " + w.getFragment() + " raw=" + w.getRawFragment());
    System.out.println("schemeSpecific = " + v.getSchemeSpecificPart() + " raw=" + v.getRawSchemeSpecificPart());
  }
}
