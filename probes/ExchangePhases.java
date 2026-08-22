import mockwebserver3.*;
import org.springframework.http.client.reactive.*;
import org.springframework.web.reactive.function.client.WebClient;
import java.time.Duration;
import java.util.concurrent.TimeUnit;

/**
 * Splits the WebClientIntegrationTests unit of work into its phases so the
 * ~25 ms/exchange CratonVM cost can be attributed. Each phase is timed in its
 * own loop; the "whole" arm at the end is the sum check.
 */
public class ExchangePhases {
    static Object sink;

    static ClientHttpConnector make(String w) {
        switch (w) {
            case "reactor": return new ReactorClientHttpConnector();
            case "jdk": return new JdkClientHttpConnector();
            case "jetty": return new JettyClientHttpConnector();
            default: return new HttpComponentsClientHttpConnector();
        }
    }

    interface Phase { void run() throws Exception; }

    static void bench(String name, int warm, int n, Phase p) throws Exception {
        for (int i = 0; i < warm; i++) p.run();
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) p.run();
        long d = System.nanoTime() - t0;
        System.out.printf("%-38s %8.2f ms/op%n", name, d / 1e6 / n);
        System.out.flush();
    }

    public static void main(String[] args) throws Exception {
        String which = args.length > 0 ? args[0] : "jetty";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        ClientHttpConnector conn = make(which);

        bench("A server new+start+close", 3, n, () -> {
            MockWebServer s = new MockWebServer(); s.start(); s.close();
        });

        MockWebServer srv = new MockWebServer();
        srv.start();
        String base = srv.url("/").toString();

        bench("B WebClient.builder().build()", 3, n, () ->
            sink = WebClient.builder().clientConnector(conn).baseUrl(base).build());

        WebClient wc = WebClient.builder().clientConnector(conn).baseUrl(base).build();

        // C: request on a warm server AND warm client — the steady-state HTTP path
        bench("C GET (warm server+client)", 3, n, () -> {
            srv.enqueue(new MockResponse.Builder().body("Hello Spring!")
                    .setHeader("Content-Type", "text/plain").build());
            String r = wc.get().uri("/g").retrieve().bodyToMono(String.class)
                    .block(Duration.ofSeconds(20));
            if (!"Hello Spring!".equals(r)) throw new IllegalStateException("bad " + r);
            srv.takeRequest(5, TimeUnit.SECONDS);
        });

        // D: build the reactive pipeline but never subscribe — assembly only
        bench("D assemble Mono (no subscribe)", 3, n, () ->
            sink = wc.get().uri("/g").retrieve().bodyToMono(String.class));

        // E: the whole per-test shape
        bench("E whole (fresh server+client+GET)", 3, n, () -> {
            MockWebServer s = new MockWebServer(); s.start();
            WebClient w = WebClient.builder().clientConnector(conn)
                    .baseUrl(s.url("/").toString()).build();
            s.enqueue(new MockResponse.Builder().body("Hello Spring!")
                    .setHeader("Content-Type", "text/plain").build());
            String r = w.get().uri("/g").retrieve().bodyToMono(String.class)
                    .block(Duration.ofSeconds(20));
            if (!"Hello Spring!".equals(r)) throw new IllegalStateException("bad " + r);
            s.takeRequest(5, TimeUnit.SECONDS);
            s.close();
        });

        srv.close();
        System.out.println("PHASES-DONE " + (sink != null));
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
