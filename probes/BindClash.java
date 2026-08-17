import java.net.*;
import java.nio.channels.*;
public class BindClash {
    static void p(String s, Object o){ System.out.println("## " + s + " = " + o); }
    public static void main(String[] a) throws Exception {
        DatagramSocket ds = new DatagramSocket();
        SocketAddress la = ds.getLocalSocketAddress();
        p("DatagramSocket.getLocalSocketAddress", la);
        p("  isBound", ds.isBound());
        p("  getReuseAddress", ds.getReuseAddress());
        // 1. plain channel bind, no reuse
        DatagramChannel c1 = DatagramChannel.open();
        try { c1.bind(la); p("channel.bind(same) NO-reuse", "SUCCEEDED (bad) -> " + c1.getLocalAddress()); }
        catch (Exception e) { p("channel.bind(same) NO-reuse THREW", e.getClass().getName()+": "+e.getMessage()); }
        c1.close();
        // 2. with SO_REUSEADDR, which is what netty's DatagramChannelConfig may set
        DatagramChannel c2 = DatagramChannel.open();
        c2.setOption(StandardSocketOptions.SO_REUSEADDR, Boolean.TRUE);
        try { c2.bind(la); p("channel.bind(same) WITH-reuse", "SUCCEEDED -> " + c2.getLocalAddress()); }
        catch (Exception e) { p("channel.bind(same) WITH-reuse THREW", e.getClass().getName()+": "+e.getMessage()); }
        c2.close();
        // 3. second plain DatagramSocket on the same port
        try { DatagramSocket d2 = new DatagramSocket(((InetSocketAddress) la).getPort()); p("new DatagramSocket(port)", "SUCCEEDED -> " + d2.getLocalSocketAddress()); d2.close(); }
        catch (Exception e) { p("new DatagramSocket(port) THREW", e.getClass().getName()+": "+e.getMessage()); }
        ds.close();
    }
}
