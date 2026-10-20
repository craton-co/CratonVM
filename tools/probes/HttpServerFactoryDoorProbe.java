import com.sun.net.httpserver.HttpServer;

import java.net.InetSocketAddress;

/**
 * Which factory you call decides which CLASS carries your server, and therefore
 * which native registrations answer.
 *
 * `net_phase_e.rs` mints two different runtime classes for the same public API:
 *
 *   * `create(InetSocketAddress, int)` -> `re10_create_server(.., class_name)`,
 *     which its caller passes `HS_IMPL_CLASS` (`sun/net/httpserver/
 *     HttpServerImpl`);
 *   * `create()` (no-arg) -> `re10_create_unbound_server`, which mints under
 *     the PUBLIC name `com/sun/net/httpserver/HttpServer`, deliberately: the
 *     `alias_class` snapshot taken at the end of the registrar cannot see the
 *     phase-72 natives registered afterwards, and this factory's callers are
 *     exactly those.
 *
 * Native dispatch keys on the receiver's runtime class, so the two factories
 * reach DIFFERENT registration sets. A probe that only ever calls the two-arg
 * factory sees zero invocations on every `com/sun/net/httpserver/HttpServer`
 * instance method and can conclude those rows are unreachable. They are not.
 * This probe walks the no-arg path so the counters can say so.
 *
 * Run it under `--dump-native-registry` and compare the invocation counts on
 * `com/sun/net/httpserver/HttpServer` against a run of
 * `HttpServerWildcardAddressProbe`, which takes the other door.
 */
public class HttpServerFactoryDoorProbe {

    public static void main(String[] args) throws Exception {
        // The no-arg factory: documented as returning an UNBOUND server, so
        // bind() is the required next call.
        HttpServer server = HttpServer.create();
        System.out.println("no-arg create() class  = " + server.getClass().getName());

        server.bind(new InetSocketAddress("127.0.0.1", 0), 0);
        System.out.println("after bind, getAddress = " + norm(server.getAddress()));

        server.createContext("/probe", exchange -> exchange.close());
        System.out.println("createContext          = ok");

        server.setExecutor(null);
        System.out.println("getExecutor            = " + server.getExecutor());

        server.start();
        System.out.println("started, getAddress    = " + norm(server.getAddress()));
        server.removeContext("/probe");
        System.out.println("removeContext          = ok");
        server.stop(0);
        System.out.println("stopped                = ok");

        // The two-arg factory, for contrast, in the same run.
        HttpServer other = HttpServer.create(new InetSocketAddress(0), 0);
        System.out.println("two-arg create() class = " + other.getClass().getName());
        other.stop(0);
    }

    /** See HttpServerWildcardAddressProbe.norm: the port is noise, the bind is not. */
    static String norm(Object o) {
        String s = String.valueOf(o);
        int c = s.lastIndexOf(':');
        if (c < 0 || c == s.length() - 1) {
            return s;
        }
        String tail = s.substring(c + 1);
        for (int i = 0; i < tail.length(); i++) {
            if (!Character.isDigit(tail.charAt(i))) {
                return s;
            }
        }
        return s.substring(0, c + 1) + "<ephemeral>";
    }
}
