import java.net.InetSocketAddress;
import java.net.StandardSocketOptions;
import java.nio.ByteBuffer;
import java.nio.channels.DatagramChannel;
import java.nio.channels.Selector;

/**
 * W7-9 section 6's five residual triples, called directly. Each row prints the
 * outcome class rather than asserting, because three of the five are documented
 * as deliberately-not-fixed and the point is to see WHICH failure each gives.
 *
 * AbstractMethodError is the shape the record predicts for an unregistered
 * method on a minted receiver; null is the shape it says would be worse.
 */
public class W79ResidualProbe {
    interface Call { Object run() throws Exception; }

    static void t(String label, Call c) {
        String out;
        try {
            Object v = c.run();
            out = (v == null) ? "null" : ("OK " + v.getClass().getName());
        } catch (Throwable e) {
            out = e.getClass().getName() + (e.getMessage() == null ? "" : ": " + e.getMessage());
        }
        System.out.println(label + " => " + out);
    }

    public static void main(String[] a) throws Exception {
        // Residual 1: Selector.provider() -- recorded FIXED 2026-08-12.
        try (Selector sel = Selector.open()) {
            t("Selector.provider()", () -> sel.provider());
        }

        try (DatagramChannel dc = DatagramChannel.open()) {
            // Residual 2: implemented only in a registrar that is dead in the
            // default build.
            t("DatagramChannel.setOption(SO_RCVBUF)",
                    () -> dc.setOption(StandardSocketOptions.SO_RCVBUF, 8192));
            t("DatagramChannel.getRemoteAddress() unconnected", () -> dc.getRemoteAddress());

            dc.bind(new InetSocketAddress("127.0.0.1", 0));
            dc.connect((InetSocketAddress) dc.getLocalAddress());
            t("DatagramChannel.getRemoteAddress() connected", () -> dc.getRemoteAddress());

            // Residual 3: the scattering/gathering pair -- "genuinely absent,
            // and genuinely reachable".
            ByteBuffer[] bufs = { ByteBuffer.allocate(4), ByteBuffer.allocate(4) };
            bufs[0].put(new byte[] { 1, 2, 3, 4 }).flip();
            bufs[1].put(new byte[] { 5, 6, 7, 8 }).flip();
            t("DatagramChannel.write(ByteBuffer[],int,int)", () -> dc.write(bufs, 0, 2));
            ByteBuffer[] rd = { ByteBuffer.allocate(4), ByteBuffer.allocate(4) };
            t("DatagramChannel.read(ByteBuffer[],int,int)", () -> dc.read(rd, 0, 2));
        }
        System.out.println("RESULT done");
    }
}
