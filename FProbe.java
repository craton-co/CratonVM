import java.io.*; import java.util.Properties;
public class FProbe {
  public static void main(String[] a) throws Exception {
    File f = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/conf/catalina.properties");
    System.out.println("exists="+f.exists()+" len="+f.length());
    try (FileInputStream in = new FileInputStream(f)) {
      Properties p = new Properties();
      p.load(in);
      System.out.println("propcount="+p.size());
      System.out.println("common.loader=["+p.getProperty("common.loader")+"]");
    } catch (Throwable t) { System.out.println("FAIL:"); t.printStackTrace(); }
  }
}
