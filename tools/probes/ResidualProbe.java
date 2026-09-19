import java.net.InetSocketAddress;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.AsynchronousSocketChannel;
import java.nio.channels.DatagramChannel;
import java.nio.channels.spi.AsynchronousChannelProvider;
import java.net.URI;

/** What is reachable for the four remaining residuals. */
public class ResidualProbe {
    static void t(String label, Call c) {
        String out;
        try {
            Object v = c.run();
            out = v == null ? "null" : (v.getClass().getName() + " [" + norm(v) + "]");
        } catch (Throwable e) {
            out = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(label + " => " + out);
    }

    /**
     * Erase an identity hash from a default toString.
     *
     * Object.toString() is `<class>@<Integer.toHexString(hashCode())>`, and that
     * hash is not comparable across two VMs -- nor across two runs of one. This
     * probe was diffed against HotSpot and reported six differing lines, four of
     * which were only the hash; the class names on those rows were identical.
     * A probe whose output is unstable cannot be scored, and the reader then has
     * to be told which lines to ignore, which is how a real divergence gets
     * ignored with them. Same repair as HttpServerWildcardAddressProbe's
     * ephemeral ports and UnderAllocationProbe's temp filenames.
     */
    static String norm(Object v) {
        String s = String.valueOf(v);
        String prefix = v.getClass().getName() + "@";
        if (s.startsWith(prefix) && s.length() > prefix.length()) {
            String tail = s.substring(prefix.length());
            if (tail.chars().allMatch(c -> Character.digit(c, 16) >= 0)) {
                return prefix + "<identity>";
            }
        }
        return s;
    }
    interface Call { Object run() throws Exception; }

    public static void main(String[] a) throws Exception {
        // (1) the provider singleton, and the two accessors
        t("AsynchronousChannelProvider.provider()", AsynchronousChannelProvider::provider);
        try (AsynchronousSocketChannel asc = AsynchronousSocketChannel.open()) {
            t("asc.provider()", asc::provider);
            t("asc.isOpen()", asc::isOpen);
        }
        try (AsynchronousServerSocketChannel assc = AsynchronousServerSocketChannel.open()) {
            t("assc.provider()", assc::provider);
            t("assc.isOpen()", assc::isOpen);
        }

        // (2) the DatagramChannel scattering pair
        try (DatagramChannel dc = DatagramChannel.open()) {
            dc.bind(new InetSocketAddress("127.0.0.1", 0));
            dc.connect((InetSocketAddress) dc.getLocalAddress());
            ByteBuffer[] w = { ByteBuffer.allocate(2), ByteBuffer.allocate(2) };
            w[0].put(new byte[] { 1, 2 }).flip();
            w[1].put(new byte[] { 3, 4 }).flip();
            t("dc.write(ByteBuffer[],0,2)", () -> dc.write(w, 0, 2));
            ByteBuffer[] r = { ByteBuffer.allocate(2), ByteBuffer.allocate(2) };
            t("dc.read(ByteBuffer[],0,2)", () -> dc.read(r, 0, 2));
            t("dc.provider()", dc::provider);
        } catch (Throwable e) {
            System.out.println("dc section aborted => " + e.getClass().getName());
        }

        // (3) HttpClient.sendAsync's future
        HttpClient client = HttpClient.newHttpClient();
        HttpRequest req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:1/")).build();
        try {
            Object fut = client.sendAsync(req, HttpResponse.BodyHandlers.ofString());
            System.out.println("sendAsync.class    = " + fut.getClass().getName());
            System.out.println("sendAsync.isDone   = "
                    + ((java.util.concurrent.CompletableFuture<?>) fut).isDone());
        } catch (Throwable e) {
            System.out.println("sendAsync.class    = " + e.getClass().getName());
            System.out.println("sendAsync.isDone   = unreached");
        }
        System.out.println("RESULT done");
    }
}
