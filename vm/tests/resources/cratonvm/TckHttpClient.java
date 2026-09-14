package cratonvm;

import java.net.*;
import java.net.http.*;
import java.time.Duration;

/**
 * T4.3 conformance tests for java.net.http.
 *
 * Exercises the HTTP client API surface without actual network calls.
 * Every method returns 1 on pass, 0 on fail.
 */
public class TckHttpClient {

    public static int httpClient_newBuilder() {
        try {
            return HttpClient.newBuilder() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int httpClient_version() {
        try {
            HttpClient c = HttpClient.newBuilder().build();
            return c.version() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int httpClient_followRedirects() {
        try {
            HttpClient c = HttpClient.newBuilder().build();
            return c.followRedirects() == HttpClient.Redirect.NEVER ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int httpRequest_builder() {
        try {
            return HttpRequest.newBuilder(URI.create("http://example.com")) != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int httpRequest_method() {
        try {
            HttpRequest r = HttpRequest.newBuilder(URI.create("http://example.com")).build();
            return "GET".equals(r.method()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int httpRequest_uri() {
        try {
            URI u = URI.create("http://example.com/path");
            HttpRequest r = HttpRequest.newBuilder(u).build();
            return u.equals(r.uri()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int httpRequest_timeout() {
        try {
            HttpRequest r = HttpRequest.newBuilder(URI.create("http://example.com"))
                .timeout(Duration.ofSeconds(30)).build();
            return r.timeout().isPresent() ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int httpRequest_headers_empty() {
        try {
            HttpRequest r = HttpRequest.newBuilder(URI.create("http://example.com")).build();
            return r.headers() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int bodyHandlers_ofString() {
        try {
            return HttpResponse.BodyHandlers.ofString() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int bodyHandlers_discarding() {
        try {
            return HttpResponse.BodyHandlers.discarding() != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
}
