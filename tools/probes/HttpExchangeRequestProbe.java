import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;

import java.io.ByteArrayOutputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.atomic.AtomicReference;

/**
 * `docs/known-issues/jdk-only/the-httpserver-family-four-defects-20260902.md`
 * §6 records that `HttpExchange` is abstract and minted too, and that no probe
 * in this tree ever served a request through it -- "the census reports only
 * what a run reaches, so its silence about a class is not a clearance." This
 * probe closes that gap: it starts a real server, sends a real HTTP request
 * with a body, and asks every documented `HttpExchange` accessor from inside
 * the handler, then reads the client's view of the response back out.
 *
 * Each accessor is asked separately, same reason as `HttpContextCarrierProbe`:
 * a probe that stops at the first failure reports its own reach, not the
 * surface.
 */
public class HttpExchangeRequestProbe {

    public static void main(String[] args) throws Exception {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        AtomicReference<String> report = new AtomicReference<>("");
        server.createContext("/probe", exchange -> handle(exchange, report));
        server.setExecutor(null);
        server.start();

        int port = server.getAddress().getPort();
        HttpClient client = HttpClient.newHttpClient();
        HttpRequest req = HttpRequest.newBuilder()
                .uri(URI.create("http://127.0.0.1:" + port + "/probe?q=1"))
                .header("X-Probe", "abc")
                .POST(HttpRequest.BodyPublishers.ofString("request-body"))
                .build();
        HttpResponse<String> resp = client.send(req, HttpResponse.BodyHandlers.ofString());

        server.stop(0);

        System.out.print(report.get());
        System.out.println("client status    = " + resp.statusCode());
        System.out.println("client body       = " + resp.body());
        System.out.println("client header      = " + resp.headers().firstValue("X-Reply").orElse("<absent>"));
    }

    static void handle(HttpExchange exchange, AtomicReference<String> report) {
        StringBuilder sb = new StringBuilder();
        ask(sb, "exchange class", () -> exchange.getClass().getName());
        ask(sb, "getRequestMethod", exchange::getRequestMethod);
        ask(sb, "getRequestURI", () -> exchange.getRequestURI().toString());
        ask(sb, "getHttpContext", () -> String.valueOf(exchange.getHttpContext() != null));
        ask(sb, "getRequestHeaders", () -> exchange.getRequestHeaders().getFirst("X-Probe"));
        ask(sb, "getLocalAddress", () -> String.valueOf(exchange.getLocalAddress() != null));
        ask(sb, "getRemoteAddress", () -> String.valueOf(exchange.getRemoteAddress() != null));
        ask(sb, "getProtocol", exchange::getProtocol);
        ask(sb, "requestBody bytes", () -> {
            byte[] body = exchange.getRequestBody().readAllBytes();
            return new String(body, StandardCharsets.UTF_8);
        });
        try {
            exchange.getResponseHeaders().add("X-Reply", "yes");
            byte[] out = "response-body".getBytes(StandardCharsets.UTF_8);
            exchange.sendResponseHeaders(200, out.length);
            try (OutputStream os = exchange.getResponseBody()) {
                os.write(out);
            }
            sb.append(pad("send+write")).append("= ok\n");
        } catch (Throwable t) {
            sb.append(pad("send+write")).append("! ").append(fail(t)).append('\n');
        }
        exchange.close();
        report.set(sb.toString());
    }

    interface Ask {
        String run() throws Throwable;
    }

    static void ask(StringBuilder sb, String what, Ask a) {
        try {
            sb.append(pad(what)).append("= ").append(a.run()).append('\n');
        } catch (Throwable t) {
            sb.append(pad(what)).append("! ").append(fail(t)).append('\n');
        }
    }

    static String fail(Throwable t) {
        String msg = t.getMessage();
        return t.getClass().getName() + (msg == null ? "" : ": " + msg);
    }

    static String pad(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 19) {
            b.append(' ');
        }
        return b.toString();
    }
}
