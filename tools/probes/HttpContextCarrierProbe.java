import com.sun.net.httpserver.HttpContext;
import com.sun.net.httpserver.HttpServer;

import java.net.InetSocketAddress;

/**
 * `createContext` hands back a carrier whose class is `com.sun.net.httpserver
 * .HttpContext`, which is ABSTRACT. This asks whether that is merely a wrong
 * `getClass()` or whether the object is unusable.
 *
 * The distinction matters and cannot be assumed either way. An abstract carrier
 * with natives registered on its own name works fine, because native dispatch
 * keys on the receiver class and never consults the JDK's bytecode. The same
 * carrier with NO natives registered reaches the abstract declaration instead,
 * and an abstract method has no body -- the documented shape there is
 * AbstractMethodError, not a wrong answer.
 *
 * Which of those happens depends on the arm: phase 72 registers the
 * `HttpContext` accessors, and phase 72 is reached only from
 * `register_synthetic_overrides`. So the real-JDK arms are the ones at risk,
 * and each accessor is asked separately -- a probe that stopped at the first
 * failure would report the reach of its own first call rather than the surface.
 */
public class HttpContextCarrierProbe {

    public static void main(String[] args) throws Exception {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        HttpContext ctx = server.createContext("/probe", exchange -> exchange.close());

        System.out.println("context class    = " + ctx.getClass().getName());
        ask("getPath", () -> ctx.getPath());
        ask("getServer", () -> String.valueOf(ctx.getServer() != null));
        ask("getHandler", () -> String.valueOf(ctx.getHandler() != null));
        ask("getAttributes", () -> String.valueOf(ctx.getAttributes() != null));

        server.stop(0);
    }

    interface Ask {
        String run() throws Throwable;
    }

    /** Each accessor reports its own outcome; one failure must not hide the next. */
    static void ask(String what, Ask a) {
        try {
            System.out.println(pad(what) + "= " + a.run());
        } catch (Throwable t) {
            String msg = t.getMessage();
            System.out.println(pad(what) + "! " + t.getClass().getName()
                    + (msg == null ? "" : ": " + msg));
        }
    }

    static String pad(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 17) {
            b.append(' ');
        }
        return b.toString();
    }
}
