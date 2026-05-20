import java.io.*;
import java.util.Properties;
public class PathProbe {
  public static void main(String[] a) throws Exception {
    File conf = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/conf/catalina.properties");
    System.out.println("len=" + conf.length());
    try (FileInputStream in = new FileInputStream(conf)) {
      byte[] buf = new byte[64];
      int n = in.read(buf);
      System.out.println("read n=" + n);
      if (n > 0) System.out.println("first=" + new String(buf, 0, Math.max(0,n)));
    }
    Properties p = new Properties();
    try (FileInputStream in = new FileInputStream(conf)) {
      p.load(in);
    }
    System.out.println("props size=" + p.size());
    String cl = p.getProperty("common.loader");
    System.out.println("common.loader=" + cl);
  }
}
