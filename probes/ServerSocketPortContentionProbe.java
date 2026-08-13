import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.StandardSocketOptions;
import java.nio.channels.ServerSocketChannel;

/**
 * Paired probe for {@code ServerSocket} bind semantics when another process
 * already listens on the port, diffed against the host JDK.
 *
 * Windows does not answer "is this port taken" the way Linux does. Whether a
 * second bind succeeds depends on which of {@code SO_REUSEADDR} and
 * {@code SO_EXCLUSIVEADDRUSE} each side set, and the two sides here are
 * different processes — so "the port is in use" is not one fact, it is a
 * negotiation, and a VM that gets its half wrong either steals ports it should
 * not or fails to take ports it should.
 *
 * This exists because a Spring Boot class that starts an embedded container on
 * the default port 8080 failed 36/66 under CratonVM while HotSpot passed 66/66
 * on the same host, at the same moment, with the same unrelated process
 * listening on 0.0.0.0:8080 — which rules out "the port was busy" as a neutral
 * environmental explanation and makes the bind path itself the variable.
 *
 * Each line prints an outcome as a value. Run both VMs back to back, with and
 * without an occupant.
 */
public class ServerSocketPortContentionProbe {

    static String bindOutcome(int port, boolean reuse, String bindAddr) {
        try (ServerSocket ss = new ServerSocket()) {
            ss.setReuseAddress(reuse);
            ss.bind(new InetSocketAddress(bindAddr, port), 50);
            return "OK localAddr=" + ss.getLocalSocketAddress();
        } catch (IOException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        int port = (args.length > 0) ? Integer.parseInt(args[0]) : 8080;
        System.out.println("port=" + port);

        // The four combinations that matter. Spring Boot's embedded Tomcat
        // binds the wildcard address; the loopback rows are the control that
        // says whether a failure is about the port or about the address.
        System.out.println("wildcard reuse=false : " + bindOutcome(port, false, "0.0.0.0"));
        System.out.println("wildcard reuse=true  : " + bindOutcome(port, true, "0.0.0.0"));
        System.out.println("loopback reuse=false : " + bindOutcome(port, false, "127.0.0.1"));
        System.out.println("loopback reuse=true  : " + bindOutcome(port, true, "127.0.0.1"));

        // Control: a port nothing should hold. If this fails, the run is
        // measuring something other than contention.
        int free = port + 10007;
        System.out.println("control port=" + free + " : " + bindOutcome(free, false, "0.0.0.0"));

        String dflt;
        try (ServerSocket ss = new ServerSocket(port, 50)) {
            dflt = "OK localAddr=" + ss.getLocalSocketAddress();
        } catch (Throwable t) {
            dflt = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println("new ServerSocket(port,50) : " + dflt);

        // The rows that actually matter for an embedded container.
        //
        // Tomcat's NioEndpoint and Jetty's ServerConnector do NOT use
        // java.net.ServerSocket — they bind a java.nio ServerSocketChannel,
        // and the two APIs do not behave the same way on Windows. ServerSocket
        // quietly asserts SO_EXCLUSIVEADDRUSE, so its `setReuseAddress(true)`
        // cannot take a port another process holds. A ServerSocketChannel with
        // StandardSocketOptions.SO_REUSEADDR gets the real Win32 SO_REUSEADDR,
        // which can. Measuring only the ServerSocket rows and generalising is
        // how a bind divergence hides.
        System.out.println("channel reuse=default : " + channelBind(port, null));
        System.out.println("channel reuse=false   : " + channelBind(port, Boolean.FALSE));
        System.out.println("channel reuse=true    : " + channelBind(port, Boolean.TRUE));
        System.out.println("channel control " + free + " : " + channelBind(free, Boolean.TRUE));
    }

    static String channelBind(int port, Boolean reuse) {
        try (ServerSocketChannel ch = ServerSocketChannel.open()) {
            String applied = "default";
            if (reuse != null) {
                ch.setOption(StandardSocketOptions.SO_REUSEADDR, reuse);
                applied = String.valueOf(ch.getOption(StandardSocketOptions.SO_REUSEADDR));
            } else {
                applied = "readback=" + ch.getOption(StandardSocketOptions.SO_REUSEADDR);
            }
            ch.bind(new InetSocketAddress("0.0.0.0", port), 50);
            return "OK " + applied + " localAddr=" + ch.getLocalAddress();
        } catch (IOException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }
}
