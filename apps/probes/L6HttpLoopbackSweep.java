import java.io.*;
import java.net.*;
import java.util.*;

/** L6 — the connection-state surface of `HttpURLConnection`/`URLConnection`,
 *  against a loopback server this probe starts itself.
 *
 *  The lane's retirement measured 30 rows it could not observe:
 *  `getInputStream`, `getResponseCode`, `getResponseMessage`,
 *  `getHeaderField(s)`, `getHeaderFieldKey`, `getContentLength`, and the
 *  date-header accessors. Every one of them needs a server to answer, and the
 *  probe tree had none — so those rows carried a verdict of "unobserved"
 *  rather than a measurement.
 *
 *  The fixture is a single-threaded `ServerSocket` on 127.0.0.1:0 serving
 *  canned, byte-fixed responses. Nothing time-varying reaches a printed row:
 *  the port is never printed, `Date`/`Last-Modified`/`Expires` are constants
 *  in the canned response rather than the wall clock, and the server closes
 *  every connection so no keep-alive pool state leaks between rows.
 *
 *  A row that throws prints the throwable's class and message, so "this VM
 *  cannot open a server socket at all" is itself a legible result rather than
 *  an empty file.
 */
public class L6HttpLoopbackSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    /** The bound port is ephemeral, so any value that quotes it — and a
     *  `FileNotFoundException`'s message is exactly that — would print a
     *  different string on every run and read as a differing row. */
    static String portTok = null;

    static String esc(String s) {
        if (s == null) return "null";
        s = s.replace("\r", "\\r").replace("\n", "\\n").replace("\t", "\\t");
        if (portTok != null) s = s.replace(portTok, ":PORT");
        return s;
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage())); }
    }

    // ---- the fixture -------------------------------------------------------

    static final String DATE = "Mon, 01 Jan 2024 00:00:00 GMT";
    static final String LMOD = "Sun, 31 Dec 2023 12:00:00 GMT";
    static final String EXPI = "Tue, 02 Jan 2024 00:00:00 GMT";

    static ServerSocket server;
    static volatile boolean stop = false;
    static volatile int served = 0;

    static String readLine(InputStream in) throws IOException {
        StringBuilder sb = new StringBuilder();
        int c;
        while ((c = in.read()) != -1) {
            if (c == '\r') continue;
            if (c == '\n') break;
            sb.append((char) c);
        }
        return sb.toString();
    }

    /** Canned response for a path. Body bytes are ASCII so lengths are fixed. */
    static byte[] response(String method, String path, String reqBody) {
        String status;
        StringBuilder h = new StringBuilder();
        String body;
        boolean chunked = false;

        if (path.equals("/ok")) {
            status = "HTTP/1.1 200 OK";
            body = "hello world";
            h.append("Content-Type: text/plain; charset=utf-8\r\n");
            h.append("X-Test: alpha\r\n");
            h.append("X-Multi: one\r\n");
            h.append("X-Multi: two\r\n");
            h.append("Last-Modified: ").append(LMOD).append("\r\n");
            h.append("Expires: ").append(EXPI).append("\r\n");
        } else if (path.equals("/404")) {
            status = "HTTP/1.1 404 Not Found";
            body = "missing";
            h.append("Content-Type: text/plain\r\n");
        } else if (path.equals("/302")) {
            status = "HTTP/1.1 302 Found";
            body = "";
            h.append("Location: /ok\r\n");
        } else if (path.equals("/204")) {
            status = "HTTP/1.1 204 No Content";
            body = "";
        } else if (path.equals("/chunked")) {
            status = "HTTP/1.1 200 OK";
            body = "abcde";
            chunked = true;
            h.append("Content-Type: text/plain\r\n");
        } else if (path.equals("/gzipname")) {
            // Content-Encoding is reported, never applied: the bytes are plain.
            status = "HTTP/1.1 200 OK";
            body = "plainbytes";
            h.append("Content-Encoding: identity\r\n");
        } else if (path.equals("/echo")) {
            status = "HTTP/1.1 200 OK";
            body = "method=" + method + " body=" + reqBody;
            h.append("Content-Type: text/plain\r\n");
        } else {
            status = "HTTP/1.1 400 Bad Request";
            body = "bad path";
        }

        StringBuilder sb = new StringBuilder();
        sb.append(status).append("\r\n");
        sb.append("Date: ").append(DATE).append("\r\n");
        sb.append(h);
        if (chunked) {
            sb.append("Transfer-Encoding: chunked\r\n");
        } else if (!status.startsWith("HTTP/1.1 204")) {
            sb.append("Content-Length: ").append(body.length()).append("\r\n");
        }
        sb.append("Connection: close\r\n\r\n");
        if (chunked) {
            sb.append("3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n");
        } else {
            sb.append(body);
        }
        byte[] out = new byte[sb.length()];
        for (int i = 0; i < sb.length(); i++) out[i] = (byte) sb.charAt(i);
        return out;
    }

    static void serveOne() throws IOException {
        Socket s = server.accept();
        try {
            s.setSoTimeout(10000);
            InputStream in = s.getInputStream();
            String request = readLine(in);
            String method = "GET", path = "/";
            int sp1 = request.indexOf(' ');
            int sp2 = sp1 < 0 ? -1 : request.indexOf(' ', sp1 + 1);
            if (sp1 > 0 && sp2 > sp1) {
                method = request.substring(0, sp1);
                path = request.substring(sp1 + 1, sp2);
            }
            int len = -1;
            boolean chunkedReq = false;
            String line;
            while (!(line = readLine(in)).isEmpty()) {
                String lower = line.toLowerCase(Locale.ROOT);
                if (lower.startsWith("content-length:")) {
                    len = Integer.parseInt(line.substring(15).trim());
                } else if (lower.startsWith("transfer-encoding:") && lower.contains("chunked")) {
                    chunkedReq = true;
                }
            }
            StringBuilder body = new StringBuilder();
            if (chunkedReq) {
                while (true) {
                    int n = Integer.parseInt(readLine(in).trim(), 16);
                    if (n == 0) { readLine(in); break; }
                    for (int i = 0; i < n; i++) body.append((char) in.read());
                    readLine(in);
                }
            } else if (len > 0) {
                for (int i = 0; i < len; i++) body.append((char) in.read());
            }
            OutputStream out = s.getOutputStream();
            out.write(response(method, path, body.toString()));
            out.flush();
            served++;
        } finally {
            try { s.close(); } catch (IOException ignored) { }
        }
    }

    static Thread startServer() throws IOException {
        server = new ServerSocket();
        server.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0), 50);
        Thread t = new Thread(() -> {
            while (!stop) {
                try { serveOne(); }
                catch (Throwable e) { if (!stop) { /* the client row records it */ } }
            }
        });
        t.setDaemon(true);
        t.start();
        return t;
    }

    static URL url(String path) throws IOException {
        return new URL("http://127.0.0.1:" + server.getLocalPort() + path);
    }

    static HttpURLConnection open(String path) throws IOException {
        HttpURLConnection c = (HttpURLConnection) url(path).openConnection();
        c.setConnectTimeout(15000);
        c.setReadTimeout(15000);
        return c;
    }

    static String slurp(InputStream in) throws IOException {
        if (in == null) return "null";
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        int c;
        while ((c = in.read()) != -1) b.write(c);
        in.close();
        return new String(b.toByteArray(), "UTF-8");
    }

    /** `getHeaderFields()` rendered order-independently; its null key is the
     *  status line, and its value lists preserve wire order. */
    static String renderFields(Map<String, List<String>> m) {
        if (m == null) return "null";
        List<String> keys = new ArrayList<>();
        for (String k : m.keySet()) keys.add(k == null ? "\u0000null" : k.toLowerCase(Locale.ROOT));
        Collections.sort(keys);
        StringBuilder sb = new StringBuilder();
        for (String k : keys) {
            String real = k.equals("\u0000null") ? null : k;
            List<String> v = null;
            for (Map.Entry<String, List<String>> e : m.entrySet()) {
                String ek = e.getKey();
                if (ek == null ? real == null : (real != null && ek.equalsIgnoreCase(real))) { v = e.getValue(); break; }
            }
            sb.append(k.equals("\u0000null") ? "null" : k).append("=").append(v).append(";");
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        try {
            startServer();
        } catch (Throwable e) {
            p("fixture start", "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
            System.out.println("rows " + rows);
            System.out.println("DONE L6HttpLoopbackSweep");
            return;
        }
        portTok = ":" + server.getLocalPort();
        p("fixture bound", server != null && server.isBound());

        // ---- 200: the whole accessor surface on one response
        tv("200 responseCode", () -> open("/ok").getResponseCode());
        tv("200 responseMessage", () -> open("/ok").getResponseMessage());
        tv("200 contentLength", () -> open("/ok").getContentLength());
        tv("200 contentLengthLong", () -> open("/ok").getContentLengthLong());
        tv("200 contentType", () -> open("/ok").getContentType());
        tv("200 contentEncoding", () -> open("/ok").getContentEncoding());
        tv("200 date", () -> open("/ok").getDate());
        tv("200 lastModified", () -> open("/ok").getLastModified());
        tv("200 expiration", () -> open("/ok").getExpiration());
        tv("200 headerField X-Test", () -> open("/ok").getHeaderField("X-Test"));
        tv("200 headerField lowercase", () -> open("/ok").getHeaderField("x-test"));
        tv("200 headerField absent", () -> open("/ok").getHeaderField("X-Absent"));
        tv("200 headerField multi", () -> open("/ok").getHeaderField("X-Multi"));
        tv("200 headerFieldKey 0", () -> open("/ok").getHeaderFieldKey(0));
        tv("200 headerField 0", () -> open("/ok").getHeaderField(0));
        tv("200 headerFieldKey 1", () -> open("/ok").getHeaderFieldKey(1));
        tv("200 headerFieldKey 99", () -> open("/ok").getHeaderFieldKey(99));
        tv("200 headerField 99", () -> open("/ok").getHeaderField(99));
        tv("200 headerFieldInt present", () -> open("/ok").getHeaderFieldInt("Content-Length", -7));
        tv("200 headerFieldInt absent", () -> open("/ok").getHeaderFieldInt("X-Absent", -7));
        tv("200 headerFieldInt unparsable", () -> open("/ok").getHeaderFieldInt("X-Test", -7));
        tv("200 headerFieldLong", () -> open("/ok").getHeaderFieldLong("Content-Length", -7L));
        tv("200 headerFieldDate present", () -> open("/ok").getHeaderFieldDate("Last-Modified", -7L));
        tv("200 headerFieldDate absent", () -> open("/ok").getHeaderFieldDate("X-Absent", -7L));
        tv("200 headerFieldDate unparsable", () -> open("/ok").getHeaderFieldDate("X-Test", -7L));
        tv("200 headerFields", () -> renderFields(open("/ok").getHeaderFields()));
        tv("200 headerFields immutable", () -> {
            Map<String, List<String>> m = open("/ok").getHeaderFields();
            try { m.put("X", new ArrayList<String>()); return "MUTABLE"; }
            catch (UnsupportedOperationException e) { return "unmodifiable"; }
        });
        tv("200 inputStream", () -> slurp(open("/ok").getInputStream()));
        tv("200 errorStream", () -> open("/ok").getErrorStream() == null ? "null" : "nonnull");
        tv("200 getContent class", () -> {
            Object o = open("/ok").getContent();
            return o == null ? "null" : o.getClass().getName();
        });
        tv("200 usingProxy", () -> open("/ok").usingProxy());
        tv("200 url path", () -> open("/ok").getURL().getPath());
        tv("200 requestMethod", () -> open("/ok").getRequestMethod());
        tv("200 doInput", () -> open("/ok").getDoInput());
        tv("200 doOutput", () -> open("/ok").getDoOutput());
        tv("200 read twice same", () -> {
            HttpURLConnection c = open("/ok");
            String a = slurp(c.getInputStream());
            String b;
            try { b = slurp(c.getInputStream()); } catch (Throwable t) { b = "THREW " + t.getClass().getName(); }
            return a + " / " + b;
        });
        tv("200 code then stream", () -> {
            HttpURLConnection c = open("/ok");
            int rc = c.getResponseCode();
            return rc + ":" + slurp(c.getInputStream());
        });
        tv("200 stream then code", () -> {
            HttpURLConnection c = open("/ok");
            String s = slurp(c.getInputStream());
            return s + ":" + c.getResponseCode();
        });
        tv("200 disconnect then code", () -> {
            HttpURLConnection c = open("/ok");
            int rc = c.getResponseCode();
            c.disconnect();
            return rc + ":" + c.getResponseCode();
        });
        tv("200 setRequestProperty after connect", () -> {
            HttpURLConnection c = open("/ok");
            c.getResponseCode();
            c.setRequestProperty("X-Late", "v");
            return "no throw";
        });
        tv("200 getRequestProperty after connect", () -> {
            HttpURLConnection c = open("/ok");
            c.getResponseCode();
            return c.getRequestProperty("X-Late");
        });

        // ---- 404: the error path
        tv("404 responseCode", () -> open("/404").getResponseCode());
        tv("404 responseMessage", () -> open("/404").getResponseMessage());
        tv("404 inputStream", () -> slurp(open("/404").getInputStream()));
        tv("404 errorStream", () -> slurp(open("/404").getErrorStream()));
        tv("404 contentLength", () -> open("/404").getContentLength());
        tv("404 code then errorStream", () -> {
            HttpURLConnection c = open("/404");
            int rc = c.getResponseCode();
            return rc + ":" + slurp(c.getErrorStream());
        });

        // ---- 204: no body at all
        tv("204 responseCode", () -> open("/204").getResponseCode());
        tv("204 contentLength", () -> open("/204").getContentLength());
        tv("204 inputStream", () -> slurp(open("/204").getInputStream()));

        // ---- 302: redirect, followed and not
        tv("302 followed code", () -> open("/302").getResponseCode());
        tv("302 followed body", () -> slurp(open("/302").getInputStream()));
        tv("302 unfollowed code", () -> {
            HttpURLConnection c = open("/302");
            c.setInstanceFollowRedirects(false);
            return c.getResponseCode();
        });
        tv("302 unfollowed location", () -> {
            HttpURLConnection c = open("/302");
            c.setInstanceFollowRedirects(false);
            return c.getHeaderField("Location");
        });
        tv("302 unfollowed url unchanged", () -> {
            HttpURLConnection c = open("/302");
            c.setInstanceFollowRedirects(false);
            c.getResponseCode();
            return c.getURL().getPath();
        });
        tv("302 followed url", () -> {
            HttpURLConnection c = open("/302");
            c.getResponseCode();
            return c.getURL().getPath();
        });

        // ---- chunked response
        tv("chunked body", () -> slurp(open("/chunked").getInputStream()));
        tv("chunked contentLength", () -> open("/chunked").getContentLength());
        tv("chunked transferEncoding", () -> open("/chunked").getHeaderField("Transfer-Encoding"));

        // ---- content-encoding reported
        tv("identity contentEncoding", () -> open("/gzipname").getContentEncoding());
        tv("identity body", () -> slurp(open("/gzipname").getInputStream()));

        // ---- POST, plain and streamed
        tv("POST echo", () -> {
            HttpURLConnection c = open("/echo");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            OutputStream o = c.getOutputStream();
            o.write("payload".getBytes("UTF-8"));
            o.close();
            return slurp(c.getInputStream());
        });
        tv("POST fixed length", () -> {
            HttpURLConnection c = open("/echo");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setFixedLengthStreamingMode(4);
            OutputStream o = c.getOutputStream();
            o.write("abcd".getBytes("UTF-8"));
            o.close();
            return slurp(c.getInputStream());
        });
        tv("POST fixed length overrun", () -> {
            HttpURLConnection c = open("/echo");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setFixedLengthStreamingMode(2);
            OutputStream o = c.getOutputStream();
            o.write("abcd".getBytes("UTF-8"));
            o.close();
            return "no throw";
        });
        tv("POST chunked streaming", () -> {
            HttpURLConnection c = open("/echo");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setChunkedStreamingMode(3);
            OutputStream o = c.getOutputStream();
            o.write("abcdef".getBytes("UTF-8"));
            o.close();
            return slurp(c.getInputStream());
        });
        tv("POST doOutput implies POST", () -> {
            HttpURLConnection c = open("/echo");
            c.setDoOutput(true);
            OutputStream o = c.getOutputStream();
            o.write("x".getBytes("UTF-8"));
            o.close();
            return c.getRequestMethod() + ":" + slurp(c.getInputStream());
        });
        tv("HEAD code", () -> {
            HttpURLConnection c = open("/ok");
            c.setRequestMethod("HEAD");
            return c.getResponseCode();
        });
        tv("PUT echo", () -> {
            HttpURLConnection c = open("/echo");
            c.setRequestMethod("PUT");
            c.setDoOutput(true);
            OutputStream o = c.getOutputStream();
            o.write("p".getBytes("UTF-8"));
            o.close();
            return slurp(c.getInputStream());
        });

        // ---- request-side state that only a real connection can settle
        tv("request property echoed to server", () -> {
            HttpURLConnection c = open("/ok");
            c.setRequestProperty("X-Sent", "v1");
            c.addRequestProperty("X-Sent", "v2");
            String got = c.getRequestProperty("X-Sent");
            c.getResponseCode();
            return got;
        });
        tv("getRequestProperties after connect", () -> {
            HttpURLConnection c = open("/ok");
            c.setRequestProperty("X-Sent", "v1");
            c.getResponseCode();
            try { return String.valueOf(c.getRequestProperties().get("X-Sent")); }
            catch (Throwable t) { return "THREW " + t.getClass().getName(); }
        });
        tv("connect twice is a no-op", () -> {
            HttpURLConnection c = open("/ok");
            c.connect();
            c.connect();
            return c.getResponseCode();
        });
        tv("setDoOutput after connect", () -> {
            HttpURLConnection c = open("/ok");
            c.connect();
            try { c.setDoOutput(true); return "no throw"; }
            catch (IllegalStateException e) { return "IllegalStateException"; }
        });
        tv("getOutputStream without doOutput", () -> {
            HttpURLConnection c = open("/echo");
            return c.getOutputStream() == null ? "null" : "nonnull";
        });
        tv("bad method rejected", () -> {
            HttpURLConnection c = open("/ok");
            c.setRequestMethod("BREW");
            return "accepted";
        });
        tv("connect to closed port", () -> {
            int port;
            try (ServerSocket t = new ServerSocket(0, 0, InetAddress.getByName("127.0.0.1"))) {
                port = t.getLocalPort();
            }
            HttpURLConnection c = (HttpURLConnection) new URL("http://127.0.0.1:" + port + "/ok").openConnection();
            c.setConnectTimeout(4000);
            return c.getResponseCode();
        });

        stop = true;
        try { server.close(); } catch (IOException ignored) { }
        System.out.println("rows " + rows);
        System.out.println("DONE L6HttpLoopbackSweep");
    }
}
