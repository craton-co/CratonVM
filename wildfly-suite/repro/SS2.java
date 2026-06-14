import java.net.*;
public class SS2 {
  public static void main(String[] a) throws Exception {
    ServerSocket s = new ServerSocket(0);
    System.out.println("created port="+s.getLocalPort());
    s.setReuseAddress(true);            // <- getImpl()
    System.out.println("setReuseAddress ok");
    s.close();
    System.out.println("done");
  }
}
