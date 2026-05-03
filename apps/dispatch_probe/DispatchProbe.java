import java.net.*;

/**
 * Wave-3 Task B follow-up dispatch probe.
 *
 * Validates that {@code invokevirtual} on a {@link java.net.InetSocketAddress}
 * receiver dispatches to {@code java.net.InetSocketAddress.getPort()} regardless
 * of how the receiver is referenced (concrete type, {@link Object} cast back).
 *
 * Symptom of the bug being pinned here: dispatch lands on
 * {@code java.lang.String.getPort()} (a non-existent method) when the receiver
 * object's recorded class id is stale / zero.
 */
public class DispatchProbe {
    public static void main(String[] a) throws Exception {
        InetSocketAddress addr = new InetSocketAddress("127.0.0.1", 8080);
        // Direct invocation -- should call InetSocketAddress.getPort()
        int p = addr.getPort();
        System.out.println("addr.getPort()=" + p);
        // Class identity
        System.out.println("addr.class=" + addr.getClass().getName());
        // Via Object reference
        Object o = addr;
        System.out.println("o.class=" + o.getClass().getName());
        // Cast back + invoke
        InetSocketAddress addr2 = (InetSocketAddress) o;
        int p2 = addr2.getPort();
        System.out.println("addr2.getPort()=" + p2);
        System.out.println("OK");
    }
}
