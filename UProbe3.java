import java.net.*;
import java.io.File;
public class UProbe3 {
  public static void main(String[] a) throws Exception {
    File jar = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/lib/catalina.jar");
    System.out.println("getPath=[" + jar.getPath() + "]");
    System.out.println("getAbsolutePath=[" + jar.getAbsolutePath() + "]");
    File abs = jar.getAbsoluteFile();
    System.out.println("absFile.getPath=[" + abs.getPath() + "]");
    System.out.println("isDirectory=" + jar.isDirectory());
    URI direct = new URI("file", null, "/C:/craton/CratonVM/apps/apache-tomcat-10.1.31/lib/catalina.jar", null);
    System.out.println("direct uri=[" + direct + "]");
    System.out.println("direct uri.path=[" + direct.getPath() + "]");
    System.out.println("direct uri.raw=[" + direct.getRawSchemeSpecificPart() + "]");
    System.out.println("direct.toString=[" + direct.toString() + "]");
    URL du = direct.toURL();
    System.out.println("direct.toURL=[" + du + "]");
  }
}
