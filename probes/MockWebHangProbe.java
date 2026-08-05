import mockwebserver3.MockResponse;
import mockwebserver3.MockWebServer;
import okhttp3.Headers;

import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.URI;

/**
 * Does a plain HttpURLConnection GET against an in-VM MockWebServer ever stall?
 *
 * Shape lifted from Spring Security's OIDC discovery probe, which is what
 * OAuth2ResourceServerAutoConfigurationTests drives: one MockWebServer, three
 * sequential GETs answered 404, 404, 200, with a 30s read timeout. On HotSpot
 * every iteration is milliseconds. The bug under test is a request whose
 * response never arrives, which the real suite only sees as a
 * SocketTimeoutException 30 seconds later.
 *
 * Prints a line per slow iteration and a final SLOW=<n> tally so a harness can
 * grep a verdict instead of reading timings.
 */
public final class MockWebHangProbe {

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        int timeoutMs = args.length > 1 ? Integer.parseInt(args[1]) : 30_000;
        int slow = 0;
        long worst = 0;

        for (int i = 0; i < iterations; i++) {
            MockWebServer server = new MockWebServer();
            server.start();
            server.enqueue(new MockResponse(404, Headers.of(), ""));
            server.enqueue(new MockResponse(404, Headers.of(), ""));
            server.enqueue(new MockResponse(200, Headers.of("Content-Type", "application/json"), "{}"));

            String url = server.url("/test").toString();
            long start = System.nanoTime();
            for (int r = 0; r < 3; r++) {
                HttpURLConnection c = (HttpURLConnection) URI.create(url).toURL().openConnection();
                c.setConnectTimeout(timeoutMs);
                c.setReadTimeout(timeoutMs);
                int code = c.getResponseCode();
                InputStream in = (code >= 400) ? c.getErrorStream() : c.getInputStream();
                if (in != null) {
                    in.readAllBytes();
                    in.close();
                }
            }
            long ms = (System.nanoTime() - start) / 1_000_000L;
            worst = Math.max(worst, ms);
            if (ms > 1000) {
                slow++;
                System.out.println("SLOW iter=" + i + " " + ms + "ms");
            }
            server.close();
        }
        System.out.println("SLOW=" + slow + " of " + iterations + " worst=" + worst + "ms");
    }
}
