import java.net.*;
public class SSRepro {
  public static void main(String[] a) throws Exception {
    InetAddress bind = InetAddress.getByName("localhost");
    System.out.println("bindAddr=" + bind);
    ServerSocket ss = new ServerSocket(0, 50, bind);
    System.out.println("isBound=" + ss.isBound());
    InetAddress ia = ss.getInetAddress();
    System.out.println("getInetAddress=" + ia);
    System.out.println("getLocalSocketAddress=" + ss.getLocalSocketAddress());
    System.out.println("getHostAddress=" + (ia==null ? "NULL->would NPE" : ia.getHostAddress()));
    ss.close();
  }
}
