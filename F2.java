import java.io.*;
public class F2 {
  public static void main(String[] a) throws Exception {
    File f = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/conf/catalina.properties");
    FileInputStream in = new FileInputStream(f);
    System.out.println("available="+in.available());
    int b0 = in.read();
    System.out.println("read()="+b0);
    byte[] buf = new byte[100];
    int n = in.read(buf, 0, 100);
    System.out.println("read(buf)="+n);
    if (n>0) System.out.println("first="+new String(buf,0,Math.min(n,40)));
    in.close();
    // also try FileReader-style
    FileInputStream in2 = new FileInputStream(f);
    byte[] all = in2.readAllBytes();
    System.out.println("readAllBytes="+all.length);
  }
}
