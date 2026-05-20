import java.io.*;
public class F4 {
  public static void main(String[] a) throws Exception {
    File f = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/conf/catalina.properties");
    System.out.println("File class="+f.getClass().getName());
    System.out.println("f.exists="+f.exists()+" f.length="+f.length()+" f.canRead="+f.canRead());
    System.out.println("f.getPath="+f.getPath()+" abs="+f.getAbsolutePath());
    FileInputStream in = new FileInputStream(f);
    System.out.println("FIS class="+in.getClass().getName());
    in.close();
    // Try a relative path
    FileInputStream in2 = new FileInputStream("UCLProbe.java");
    System.out.println("UCLProbe.java avail="+in2.available());
    in2.close();
  }
}
