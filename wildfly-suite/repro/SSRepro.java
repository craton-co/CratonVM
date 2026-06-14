import java.net.*;
public class SSRepro {
  public static void main(String[] a) throws Exception {
    System.out.println("=== new ServerSocket() then bind ===");
    try { ServerSocket s1 = new ServerSocket(); System.out.println("ss()=ok created"); s1.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0)); System.out.println("bind=ok port="+s1.getLocalPort()); s1.close(); }
    catch (Throwable t) { System.out.println("ss() path FAILED: "+t); t.printStackTrace(System.out); }
    System.out.println("=== new ServerSocket(port,backlog,addr) ===");
    try { ServerSocket s2 = new ServerSocket(0, 1, InetAddress.getByName("127.0.0.1")); System.out.println("ss(port,bl,addr)=ok port="+s2.getLocalPort()); s2.close(); }
    catch (Throwable t) { System.out.println("ss(3-arg) FAILED: "+t); }
    System.out.println("done");
  }
}
