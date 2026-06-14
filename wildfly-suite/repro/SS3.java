import java.net.*;
public class SS3 {
  public static void main(String[] a) throws Exception {
    try (ServerSocket s = new ServerSocket(0)) { s.setReuseAddress(true); System.out.println("SS.setReuseAddress ok reuse="+s.getReuseAddress()); }
    try (DatagramSocket d = new DatagramSocket(0)) { d.setReuseAddress(true); System.out.println("DG.setReuseAddress ok reuse="+d.getReuseAddress()); }
    System.out.println("done");
  }
}
