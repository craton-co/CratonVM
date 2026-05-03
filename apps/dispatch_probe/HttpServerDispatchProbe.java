import com.sun.net.httpserver.HttpServer;
import java.net.*;

/**
 * Repro for the HttpServer.getAddress().getPort() dispatch bug.
 *
 * If `HttpServer.getAddress()` returns an object whose recorded class id
 * resolves to `java.lang.String` (instead of `java.net.InetSocketAddress`),
 * `getPort()` invokevirtual lands on the wrong class and trips
 * NoSuchMethodError.
 */
public class HttpServerDispatchProbe {
    public static void main(String[] a) throws Exception {
        HttpServer srv = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        InetSocketAddress addr = srv.getAddress();
        System.out.println("addr.class=" + addr.getClass().getName());
        // The bug: this invokevirtual was landing on String.getPort()
        int port = addr.getPort();
        System.out.println("addr.getPort()=" + port);
        System.out.println("OK");
    }
}
