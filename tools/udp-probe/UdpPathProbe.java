import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.DatagramChannel;

/** Which DatagramChannel operations actually work end-to-end? */
public class UdpPathProbe {
    public static void main(String[] args) throws Exception {
        DatagramChannel a = DatagramChannel.open();
        a.bind(new InetSocketAddress("127.0.0.1", 0));
        int port = ((InetSocketAddress) a.getLocalAddress()).getPort();
        System.out.println("bound port=" + port + " impl=" + a.getClass().getName());

        DatagramChannel b = DatagramChannel.open();
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        try {
            b.send(ByteBuffer.wrap("hi".getBytes()), new InetSocketAddress("127.0.0.1", port));
            System.out.println("OK   send");
        } catch (Throwable t) {
            System.out.println("FAIL send -> " + t);
        }
        a.configureBlocking(false);
        try {
            ByteBuffer buf = ByteBuffer.allocate(64);
            Object src = a.receive(buf);
            System.out.println("OK   receive src=" + src + " n=" + buf.position());
        } catch (Throwable t) {
            System.out.println("FAIL receive -> " + t);
        }
        // The multicast surface is the one piece still implemented in
        // datagram.rs. It must resolve the very same channel that open()/bind()
        // created — that cross-module hand-off is exactly what the split
        // registry used to break.
        try (DatagramChannel m = DatagramChannel.open()) {
            m.bind(new InetSocketAddress("0.0.0.0", 0));
            java.net.InetAddress group = java.net.InetAddress.getByName("239.9.9.9");
            java.net.NetworkInterface nif =
                    java.net.NetworkInterface.getByInetAddress(
                            java.net.InetAddress.getByName("127.0.0.1"));
            java.nio.channels.MembershipKey key = m.join(group, nif);
            System.out.println("OK   join valid=" + key.isValid());
            key.drop();
            System.out.println("OK   drop valid=" + key.isValid());
        } catch (Throwable t) {
            System.out.println("FAIL multicast -> " + t);
        }

        a.close();
        b.close();
        System.out.println("DONE");
        System.exit(0);
    }
}
