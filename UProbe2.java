import java.net.*;
import java.io.File;
public class UProbe2 {
  public static void main(String[] a) throws Exception {
    File jar = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/lib/catalina.jar");
    URI uri = jar.toURI();
    System.out.println("uri=" + uri);
    System.out.println("uri.scheme=" + uri.getScheme());
    System.out.println("uri.path=" + uri.getPath());
    URL u1 = uri.toURL();
    System.out.println("uri.toURL()=" + u1);
    URL u2 = new URL("file:/C:/craton/CratonVM/apps/apache-tomcat-10.1.31/lib/catalina.jar");
    System.out.println("new URL(file:..)=" + u2);
    System.out.println("u2.proto=" + u2.getProtocol() + " path=" + u2.getPath());
  }
}
