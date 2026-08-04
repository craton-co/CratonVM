import mockwebserver3.MockResponse;
import mockwebserver3.MockWebServer;
import okhttp3.Headers;
import org.springframework.http.client.SimpleClientHttpRequestFactory;
import org.springframework.web.client.RestTemplate;

/**
 * Is a configured read timeout still honoured once the caller is JIT-compiled?
 *
 * Spring's SimpleClientHttpRequestFactory.prepareConnection does
 *   if (this.readTimeout >= 0) connection.setReadTimeout(this.readTimeout);
 * With the field at 30000 CratonVM was observed passing 500 to the native
 * setter, but only inside a warm Spring context — so warm the method here by
 * INVOCATIONS, then measure how long a request to a server that never answers
 * takes to give up. That elapsed time IS the timeout that was applied.
 */
public final class ReadTimeoutHotProbe {

    public static void main(String[] args) throws Exception {
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        int configured = args.length > 1 ? Integer.parseInt(args[1]) : 30000;

        SimpleClientHttpRequestFactory factory = new SimpleClientHttpRequestFactory();
        factory.setConnectTimeout(configured);
        factory.setReadTimeout(configured);
        RestTemplate rest = new RestTemplate();
        rest.setRequestFactory(factory);

        MockWebServer warmer = new MockWebServer();
        warmer.start();
        String warmUrl = warmer.url("/warm").toString();
        for (int i = 0; i < warm; i++) {
            warmer.enqueue(new MockResponse(200, Headers.of("Content-Type", "application/json"), "{}"));
            try {
                rest.getForObject(warmUrl, String.class);
            } catch (RuntimeException ignored) {
                // a warm-up hiccup must not end the probe
            }
        }
        System.out.println("warmed " + warm + " requests; field readTimeout=" + readField(factory));

        // A server with an EMPTY queue never answers, so the client waits out
        // exactly the read timeout that was actually applied.
        MockWebServer silent = new MockWebServer();
        silent.start();
        String url = silent.url("/silent").toString();
        long start = System.nanoTime();
        try {
            rest.getForObject(url, String.class);
            System.out.println("UNEXPECTED: silent server answered");
        } catch (RuntimeException expected) {
            long ms = (System.nanoTime() - start) / 1_000_000L;
            long slack = configured / 4;
            String verdict = (ms >= configured - slack) ? "OK" : "EARLY";
            System.out.println("applied_timeout=" + ms + "ms configured=" + configured + "ms " + verdict);
        }
        warmer.close();
        silent.close();
    }

    private static Object readField(Object o) throws Exception {
        java.lang.reflect.Field f = o.getClass().getDeclaredField("readTimeout");
        f.setAccessible(true);
        return f.get(o);
    }
}
