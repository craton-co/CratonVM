import java.io.*;
import java.net.*;
import java.util.*;

/** L6 — the pure-logic half of the HTTP stack: `java.net.URLConnection` (7
 *  owning rows), `java.net.HttpURLConnection` (30), and the two implementation
 *  classes reached through `URL.openConnection`,
 *  `sun.net.www.protocol.http.HttpURLConnection` (31) and
 *  `sun.net.www.protocol.https.HttpsURLConnectionImpl` (67).
 *
 *  **No connection is opened and no handshake is attempted.** The lane page is
 *  explicit that a live TLS handshake is the wrong instrument and that the
 *  regression corpus already covers it. What is asked here is the part that is
 *  bookkeeping and validation, and therefore reproducible:
 *
 *    * `setRequestMethod`'s allow-list and its state rules,
 *    * `setFixedLengthStreamingMode` / `setChunkedStreamingMode` argument
 *      validation and their mutual exclusion,
 *    * the timeout setters' negative-argument contract,
 *    * `setRequestProperty` / `addRequestProperty` null-key handling, the
 *      case-insensitive lookup, and the ORDER `getRequestProperties` reports,
 *    * `getHeaderFieldDate` / `getHeaderFieldInt` / `getHeaderFieldLong`
 *      parsing over a fixture header map, which is where the RFC-1123,
 *      RFC-850 and asctime formats diverge,
 *    * the "already connected" refusals, and which setters are still legal,
 *    * the static `HttpURLConnection` response-code constants.
 *
 *  A subclass supplies the header fixture so the parsing rows never need a
 *  server. `getHeaderFieldKey(i)`/`getHeaderField(i)` ordering is asked of the
 *  same fixture.
 *
 *  Nothing prints a resolved address, a timing, or a hash-container iteration
 *  order that the two VMs may choose independently: the request-property map
 *  is rendered through a SORTED view where the spec does not fix its order,
 *  and the indexed header accessors are asked of a fixture whose order the
 *  subclass itself defines.
 */
public class L6HttpLogicSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    /** A `HttpURLConnection` with a fixed header fixture and no transport. */
    static class Fixture extends HttpURLConnection {
        final String[][] hdr = {
            {null, "HTTP/1.1 200 OK"},
            {"Content-Type", "text/html; charset=utf-8"},
            {"Content-Length", "1234"},
            {"Date", "Sun, 06 Nov 1994 08:49:37 GMT"},
            {"Last-Modified", "Sunday, 06-Nov-94 08:49:37 GMT"},
            {"Expires", "Sun Nov  6 08:49:37 1994"},
            {"X-Bad-Date", "not a date at all"},
            {"X-Int", "42"},
            {"X-Not-Int", "4x2"},
            {"X-Long", "9007199254740993"},
            {"X-Empty", ""},
            {"Set-Cookie", "a=1"},
            {"set-cookie", "b=2"},
        };
        Fixture() throws Exception { super(new URL("http://fixture.invalid/p")); }
        Fixture(URL u) { super(u); }
        public void connect() { connected = true; }
        public void disconnect() { connected = false; }
        public boolean usingProxy() { return false; }
        public String getHeaderFieldKey(int i) { return i < hdr.length ? hdr[i][0] : null; }
        public String getHeaderField(int i) { return i < hdr.length ? hdr[i][1] : null; }
        public String getHeaderField(String k) {
            for (int i = hdr.length - 1; i >= 0; i--)
                if (hdr[i][0] != null && hdr[i][0].equalsIgnoreCase(k)) return hdr[i][1];
            return null;
        }
        public int getResponseCode() { return 200; }
        public String getResponseMessage() { return "OK"; }
    }

    static String sortedMap(Map<String, List<String>> m) {
        if (m == null) return "null";
        TreeMap<String, List<String>> t = new TreeMap<>(String.CASE_INSENSITIVE_ORDER);
        t.putAll(m);
        StringBuilder sb = new StringBuilder("{");
        for (Map.Entry<String, List<String>> e : t.entrySet())
            sb.append(e.getKey()).append("=").append(e.getValue()).append(" ");
        return sb.append("}").toString();
    }

    public static void main(String[] args) throws Exception {
        // ---- setRequestMethod: the allow-list, the case rule, and the state rule
        for (String m : new String[] {
                "GET", "POST", "HEAD", "OPTIONS", "PUT", "DELETE", "TRACE",
                "PATCH", "CONNECT", "get", "Get", "", "BOGUS", "GET ",
                " GET", "GET\r\nX: y"})
            tv("setRequestMethod " + esc(m), () -> {
                HttpURLConnection c = new Fixture();
                c.setRequestMethod(m);
                return c.getRequestMethod();
            });
        tv("setRequestMethod null", () -> {
            HttpURLConnection c = new Fixture();
            c.setRequestMethod(null);
            return c.getRequestMethod();
        });
        tv("default method", () -> new Fixture().getRequestMethod());
        tv("setRequestMethod after connect", () -> {
            HttpURLConnection c = new Fixture();
            c.connect();
            c.setRequestMethod("POST");
            return c.getRequestMethod();
        });

        // ---- the streaming modes: validation and mutual exclusion
        tv("fixedLength int -1", () -> { HttpURLConnection c = new Fixture(); c.setFixedLengthStreamingMode(-1); return "ok"; });
        tv("fixedLength int 0", () -> { HttpURLConnection c = new Fixture(); c.setFixedLengthStreamingMode(0); return "ok"; });
        tv("fixedLength long -1", () -> { HttpURLConnection c = new Fixture(); c.setFixedLengthStreamingMode(-1L); return "ok"; });
        tv("fixedLength long big", () -> { HttpURLConnection c = new Fixture(); c.setFixedLengthStreamingMode(1L << 40); return "ok"; });
        tv("chunked -1 accepted", () -> { HttpURLConnection c = new Fixture(); c.setChunkedStreamingMode(-1); return "ok"; });
        tv("chunked 0 accepted", () -> { HttpURLConnection c = new Fixture(); c.setChunkedStreamingMode(0); return "ok"; });
        tv("fixed then chunked", () -> {
            HttpURLConnection c = new Fixture();
            c.setFixedLengthStreamingMode(10);
            c.setChunkedStreamingMode(10);
            return "ok";
        });
        tv("chunked then fixed", () -> {
            HttpURLConnection c = new Fixture();
            c.setChunkedStreamingMode(10);
            c.setFixedLengthStreamingMode(10);
            return "ok";
        });
        tv("fixed twice", () -> {
            HttpURLConnection c = new Fixture();
            c.setFixedLengthStreamingMode(10);
            c.setFixedLengthStreamingMode(20);
            return "ok";
        });
        tv("fixed after connect", () -> {
            HttpURLConnection c = new Fixture();
            c.connect();
            c.setFixedLengthStreamingMode(10);
            return "ok";
        });
        tv("chunked after connect", () -> {
            HttpURLConnection c = new Fixture();
            c.connect();
            c.setChunkedStreamingMode(10);
            return "ok";
        });

        // ---- redirects, instance and static
        tv("default instance follow", () -> new Fixture().getInstanceFollowRedirects());
        tv("static follow default", () -> HttpURLConnection.getFollowRedirects());
        tv("set instance false", () -> {
            HttpURLConnection c = new Fixture();
            c.setInstanceFollowRedirects(false);
            return c.getInstanceFollowRedirects();
        });
        tv("static setter does not move instance", () -> {
            HttpURLConnection c = new Fixture();
            boolean before = c.getInstanceFollowRedirects();
            HttpURLConnection.setFollowRedirects(false);
            boolean after = c.getInstanceFollowRedirects();
            HttpURLConnection.setFollowRedirects(true);
            return before + " " + after;
        });

        // ---- timeouts: the negative-argument contract
        for (int t : new int[] {-1, 0, 1, Integer.MAX_VALUE}) {
            tv("setConnectTimeout " + t, () -> {
                URLConnection c = new Fixture();
                c.setConnectTimeout(t);
                return c.getConnectTimeout();
            });
            tv("setReadTimeout " + t, () -> {
                URLConnection c = new Fixture();
                c.setReadTimeout(t);
                return c.getReadTimeout();
            });
        }

        // ---- request properties: null keys, case folding, and multi-value order
        tv("setRequestProperty null key", () -> {
            URLConnection c = new Fixture();
            c.setRequestProperty(null, "v");
            return "ok";
        });
        tv("addRequestProperty null key", () -> {
            URLConnection c = new Fixture();
            c.addRequestProperty(null, "v");
            return "ok";
        });
        tv("setRequestProperty null value", () -> {
            URLConnection c = new Fixture();
            c.setRequestProperty("K", null);
            return String.valueOf(c.getRequestProperty("K"));
        });
        tv("getRequestProperty null key", () -> String.valueOf(new Fixture().getRequestProperty(null)));
        tv("getRequestProperty absent", () -> String.valueOf(new Fixture().getRequestProperty("Nope")));
        tv("case-insensitive lookup", () -> {
            URLConnection c = new Fixture();
            c.setRequestProperty("X-Key", "v1");
            return c.getRequestProperty("x-key") + " " + c.getRequestProperty("X-KEY");
        });
        tv("set replaces, add appends", () -> {
            URLConnection c = new Fixture();
            c.setRequestProperty("K", "1");
            c.addRequestProperty("K", "2");
            c.setRequestProperty("K", "3");
            c.addRequestProperty("k", "4");
            return c.getRequestProperty("K") + " | " + sortedMap(c.getRequestProperties());
        });
        tv("getRequestProperties unmodifiable", () -> {
            URLConnection c = new Fixture();
            c.setRequestProperty("K", "1");
            try { c.getRequestProperties().put("Z", new ArrayList<>()); return "modifiable"; }
            catch (Throwable e) { return e.getClass().getName(); }
        });
        tv("value list unmodifiable", () -> {
            URLConnection c = new Fixture();
            c.setRequestProperty("K", "1");
            try { c.getRequestProperties().get("K").add("2"); return "modifiable"; }
            catch (Throwable e) { return e.getClass().getName(); }
        });
        tv("setRequestProperty after connect", () -> {
            URLConnection c = new Fixture();
            c.connect();
            c.setRequestProperty("K", "1");
            return "ok";
        });
        tv("getRequestProperties after connect", () -> {
            URLConnection c = new Fixture();
            c.connect();
            return sortedMap(c.getRequestProperties());
        });

        // ---- the header accessors over the fixture
        Fixture f = new Fixture();
        for (int i = 0; i < 15; i++) {
            final int k = i;
            tv("headerFieldKey " + i, () -> String.valueOf(f.getHeaderFieldKey(k)));
            tv("headerField " + i, () -> String.valueOf(f.getHeaderField(k)));
        }
        tv("headerField by name", () -> f.getHeaderField("Content-Type"));
        tv("headerField by name folded", () -> f.getHeaderField("CONTENT-TYPE"));
        tv("headerField absent", () -> String.valueOf(f.getHeaderField("No-Such")));
        tv("contentLength", () -> f.getContentLength());
        tv("contentLengthLong", () -> f.getContentLengthLong());
        tv("contentType", () -> f.getContentType());
        tv("contentEncoding", () -> String.valueOf(f.getContentEncoding()));
        tv("date rfc1123", () -> f.getHeaderFieldDate("Date", -1L));
        tv("lastModified rfc850", () -> f.getHeaderFieldDate("Last-Modified", -1L));
        tv("expires asctime", () -> f.getHeaderFieldDate("Expires", -1L));
        tv("bad date default", () -> f.getHeaderFieldDate("X-Bad-Date", -7L));
        tv("absent date default", () -> f.getHeaderFieldDate("No-Such", -7L));
        tv("empty date default", () -> f.getHeaderFieldDate("X-Empty", -7L));
        tv("headerFieldInt", () -> f.getHeaderFieldInt("X-Int", -7));
        tv("headerFieldInt bad", () -> f.getHeaderFieldInt("X-Not-Int", -7));
        tv("headerFieldInt overflow", () -> f.getHeaderFieldInt("X-Long", -7));
        tv("headerFieldLong", () -> f.getHeaderFieldLong("X-Long", -7L));
        tv("getLastModified", () -> f.getLastModified());
        tv("getDate", () -> f.getDate());
        tv("getExpiration", () -> f.getExpiration());
        tv("responseCode", () -> f.getResponseCode());
        tv("responseMessage", () -> f.getResponseMessage());
        tv("getErrorStream default null", () -> String.valueOf(f.getErrorStream()));
        tv("usingProxy", () -> f.usingProxy());
        tv("getURL", () -> f.getURL().toString());
        tv("toString has class name", () -> f.toString().startsWith(f.getClass().getName()));

        // ---- the doInput/doOutput/useCaches flags and their refusals
        tv("default doInput", () -> new Fixture().getDoInput());
        tv("default doOutput", () -> new Fixture().getDoOutput());
        tv("default useCaches", () -> new Fixture().getUseCaches());
        tv("default allowUserInteraction", () -> new Fixture().getAllowUserInteraction());
        tv("default ifModifiedSince", () -> new Fixture().getIfModifiedSince());
        tv("setDoInput after connect", () -> { URLConnection c = new Fixture(); c.connect(); c.setDoInput(false); return "ok"; });
        tv("setDoOutput after connect", () -> { URLConnection c = new Fixture(); c.connect(); c.setDoOutput(true); return "ok"; });
        tv("setUseCaches after connect", () -> { URLConnection c = new Fixture(); c.connect(); c.setUseCaches(false); return "ok"; });
        tv("setIfModifiedSince after connect", () -> { URLConnection c = new Fixture(); c.connect(); c.setIfModifiedSince(1); return "ok"; });
        tv("setDoOutput true then read", () -> { URLConnection c = new Fixture(); c.setDoOutput(true); return c.getDoOutput(); });
        tv("getDefaultUseCaches", () -> new Fixture().getDefaultUseCaches());

        // ---- the response-code constants, which a shim may transcribe wrongly
        p("HTTP_OK", HttpURLConnection.HTTP_OK);
        p("HTTP_CREATED", HttpURLConnection.HTTP_CREATED);
        p("HTTP_ACCEPTED", HttpURLConnection.HTTP_ACCEPTED);
        p("HTTP_NO_CONTENT", HttpURLConnection.HTTP_NO_CONTENT);
        p("HTTP_PARTIAL", HttpURLConnection.HTTP_PARTIAL);
        p("HTTP_MULT_CHOICE", HttpURLConnection.HTTP_MULT_CHOICE);
        p("HTTP_MOVED_PERM", HttpURLConnection.HTTP_MOVED_PERM);
        p("HTTP_MOVED_TEMP", HttpURLConnection.HTTP_MOVED_TEMP);
        p("HTTP_SEE_OTHER", HttpURLConnection.HTTP_SEE_OTHER);
        p("HTTP_NOT_MODIFIED", HttpURLConnection.HTTP_NOT_MODIFIED);
        p("HTTP_USE_PROXY", HttpURLConnection.HTTP_USE_PROXY);
        p("HTTP_BAD_REQUEST", HttpURLConnection.HTTP_BAD_REQUEST);
        p("HTTP_UNAUTHORIZED", HttpURLConnection.HTTP_UNAUTHORIZED);
        p("HTTP_FORBIDDEN", HttpURLConnection.HTTP_FORBIDDEN);
        p("HTTP_NOT_FOUND", HttpURLConnection.HTTP_NOT_FOUND);
        p("HTTP_BAD_METHOD", HttpURLConnection.HTTP_BAD_METHOD);
        p("HTTP_CONFLICT", HttpURLConnection.HTTP_CONFLICT);
        p("HTTP_GONE", HttpURLConnection.HTTP_GONE);
        p("HTTP_PRECON_FAILED", HttpURLConnection.HTTP_PRECON_FAILED);
        p("HTTP_ENTITY_TOO_LARGE", HttpURLConnection.HTTP_ENTITY_TOO_LARGE);
        p("HTTP_UNSUPPORTED_TYPE", HttpURLConnection.HTTP_UNSUPPORTED_TYPE);
        p("HTTP_INTERNAL_ERROR", HttpURLConnection.HTTP_INTERNAL_ERROR);
        p("HTTP_NOT_IMPLEMENTED", HttpURLConnection.HTTP_NOT_IMPLEMENTED);
        p("HTTP_BAD_GATEWAY", HttpURLConnection.HTTP_BAD_GATEWAY);
        p("HTTP_UNAVAILABLE", HttpURLConnection.HTTP_UNAVAILABLE);
        p("HTTP_GATEWAY_TIMEOUT", HttpURLConnection.HTTP_GATEWAY_TIMEOUT);
        p("HTTP_VERSION", HttpURLConnection.HTTP_VERSION);

        // ---- the real implementation classes, built but never connected
        tv("http impl class", () -> new URL("http://h.invalid/p").openConnection().getClass().getName());
        tv("https impl class", () -> new URL("https://h.invalid/p").openConnection().getClass().getName());
        tv("http impl default method", () -> ((HttpURLConnection) new URL("http://h.invalid/p").openConnection()).getRequestMethod());
        tv("http impl setRequestMethod bogus", () -> {
            HttpURLConnection c = (HttpURLConnection) new URL("http://h.invalid/p").openConnection();
            c.setRequestMethod("BOGUS");
            return c.getRequestMethod();
        });
        tv("http impl properties", () -> {
            URLConnection c = new URL("http://h.invalid/p").openConnection();
            c.setRequestProperty("X-A", "1");
            c.addRequestProperty("x-a", "2");
            return c.getRequestProperty("X-A") + " | " + c.getRequestProperties().get("X-A");
        });
        tv("http impl usingProxy", () -> ((HttpURLConnection) new URL("http://h.invalid/p").openConnection()).usingProxy());
        tv("http impl instance follow", () -> ((HttpURLConnection) new URL("http://h.invalid/p").openConnection()).getInstanceFollowRedirects());
        tv("http impl streaming validation", () -> {
            HttpURLConnection c = (HttpURLConnection) new URL("http://h.invalid/p").openConnection();
            c.setChunkedStreamingMode(4096);
            try { c.setFixedLengthStreamingMode(10); return "no throw"; }
            catch (Throwable e) { return e.getClass().getName() + " msg=" + esc(e.getMessage()); }
        });
        tv("https impl is HttpURLConnection", () -> new URL("https://h.invalid/p").openConnection() instanceof HttpURLConnection);
        tv("https impl getCipherSuite before connect", () -> {
            javax.net.ssl.HttpsURLConnection c =
                (javax.net.ssl.HttpsURLConnection) new URL("https://h.invalid/p").openConnection();
            return c.getCipherSuite();
        });
        tv("https impl getLocalPrincipal before connect", () -> {
            javax.net.ssl.HttpsURLConnection c =
                (javax.net.ssl.HttpsURLConnection) new URL("https://h.invalid/p").openConnection();
            return String.valueOf(c.getLocalPrincipal());
        });
        tv("https impl hostnameVerifier not null", () -> {
            javax.net.ssl.HttpsURLConnection c =
                (javax.net.ssl.HttpsURLConnection) new URL("https://h.invalid/p").openConnection();
            return c.getHostnameVerifier() != null;
        });
        tv("https impl sslSocketFactory not null", () -> {
            javax.net.ssl.HttpsURLConnection c =
                (javax.net.ssl.HttpsURLConnection) new URL("https://h.invalid/p").openConnection();
            return c.getSSLSocketFactory() != null;
        });
        tv("https default hostnameVerifier not null",
           () -> javax.net.ssl.HttpsURLConnection.getDefaultHostnameVerifier() != null);
        tv("https default sslSocketFactory not null",
           () -> javax.net.ssl.HttpsURLConnection.getDefaultSSLSocketFactory() != null);
        tv("https setSSLSocketFactory null", () -> {
            javax.net.ssl.HttpsURLConnection c =
                (javax.net.ssl.HttpsURLConnection) new URL("https://h.invalid/p").openConnection();
            c.setSSLSocketFactory(null);
            return "no throw";
        });
        tv("https setHostnameVerifier null", () -> {
            javax.net.ssl.HttpsURLConnection c =
                (javax.net.ssl.HttpsURLConnection) new URL("https://h.invalid/p").openConnection();
            c.setHostnameVerifier(null);
            return "no throw";
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L6HttpLogicSweep");
    }
}
