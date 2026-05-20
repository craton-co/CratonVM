import java.net.*;
import java.io.File;
public class UCLProbe {
  public static void main(String[] a) throws Exception {
    File jar = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/lib/catalina.jar");
    System.out.println("jar exists=" + jar.exists());
    URL u = jar.toURI().toURL();
    System.out.println("url=" + u);
    URL[] urls = { u };
    URLClassLoader ucl = new URLClassLoader(urls, UCLProbe.class.getClassLoader());
    Class<?> c = Class.forName("org.apache.catalina.startup.Catalina", false, ucl);
    System.out.println("loaded=" + c.getName());

    File dir = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/lib");
    URL du = dir.toURI().toURL();
    System.out.println("dirurl=" + du);
    URLClassLoader ucl2 = new URLClassLoader(new URL[]{du}, UCLProbe.class.getClassLoader());
    try {
      Class<?> c2 = Class.forName("org.apache.catalina.startup.Catalina", false, ucl2);
      System.out.println("dirloaded=" + c2.getName());
    } catch (Throwable t) {
      System.out.println("dir-fail=" + t);
    }
  }
}
