import java.net.*; import java.io.File;
public class TomProbe {
  static void show(String label, URL u) throws Exception {
    System.out.println(label+": url="+u+" | path="+u.getPath()+" | file="+u.getFile()+" | proto="+u.getProtocol());
  }
  public static void main(String[] a) throws Exception {
    File dir = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/lib");
    File cf = dir.getCanonicalFile();
    System.out.println("canonicalFile="+cf+" isDir="+cf.isDirectory());
    URI u1 = cf.toURI();
    System.out.println("toURI="+u1+" toString="+u1.toString());
    String s = u1.toString().replace("!/", "%21/");
    URI u2 = new URI(s);
    System.out.println("reparsedURI="+u2);
    URL url = u2.toURL();
    show("DIR", url);
    // a jar
    File jar = new File(dir, "catalina.jar").getCanonicalFile();
    URL jurl = new URI(jar.toURI().toString().replace("!/","%21/")).toURL();
    show("JAR", jurl);
    URLClassLoader ucl = new URLClassLoader(new URL[]{url, jurl}, TomProbe.class.getClassLoader());
    Class<?> c = Class.forName("org.apache.catalina.startup.Catalina", false, ucl);
    System.out.println("loaded="+c.getName());
  }
}
