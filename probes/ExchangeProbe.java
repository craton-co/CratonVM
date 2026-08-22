import mockwebserver3.*;
import org.springframework.http.client.reactive.*;
import org.springframework.web.reactive.function.client.WebClient;
import java.time.Duration;

/**
 * The WebClientIntegrationTests unit of work, isolated: fresh MockWebServer +
 * WebClient + one GET + close, exactly as every one of the 170 parameterized
 * tests does. Reproduces the whole per-test gap (HotSpot ~4-7 ms, CratonVM
 * ~30-56 ms) without JUnit, so a perf profile is all exchange and no harness.
 *
 * Usage: ExchangeProbe <connector> <iters>
 *   connector: reactor | jdk | jetty | httpcomponents
 */
public class ExchangeProbe {
    static Object sink;

    static ClientHttpConnector make(String which) {
        switch (which) {
            case "reactor": return new ReactorClientHttpConnector();
            case "jdk": return new JdkClientHttpConnector();
            case "jetty": return new JettyClientHttpConnector();
            default: return new HttpComponentsClientHttpConnector();
        }
    }

    static void one(ClientHttpConnector c) throws Exception {
        MockWebServer s = new MockWebServer();
        s.start();
        WebClient wc = WebClient.builder().clientConnector(c)
                .baseUrl(s.url("/").toString()).build();
        s.enqueue(new MockResponse.Builder().body("Hello Spring!")
                .setHeader("Content-Type", "text/plain").build());
        String r = wc.get().uri("/g").retrieve().bodyToMono(String.class)
                .block(Duration.ofSeconds(20));
        if (!"Hello Spring!".equals(r)) throw new IllegalStateException("bad " + r);
        s.takeRequest(5, java.util.concurrent.TimeUnit.SECONDS);
        s.close();
        sink = r;
    }

    public static void main(String[] args) throws Exception {
        String which = args.length > 0 ? args[0] : "jetty";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        ClientHttpConnector c = make(which);
        for (int i = 0; i < 5; i++) one(c);                 // warm
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) one(c);
        long d = System.nanoTime() - t0;
        System.out.printf("exchange[%s] %8.2f ms/op  (n=%d)%n", which, d / 1e6 / n, n);
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
