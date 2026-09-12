import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;

/** L6 — `java.net.URL` (18 rows), `URLEncoder`/`URLDecoder`, and the pure-logic
 *  half of `URLConnection`.
 *
 *  `URL` is not `URI`: it does no RFC-3986 validation, it delegates parsing to
 *  a per-protocol `URLStreamHandler`, and `equals`/`hashCode` on it are
 *  specified to be resolver-dependent for the HOST component. That last part is
 *  the trap — `URL.equals` may perform a DNS lookup — so **every comparison
 *  here is between two URLs with the SAME literal host text**, where the spec's
 *  answer does not depend on a resolver, and nothing prints a resolved address.
 *
 *  No connection is opened. `openConnection()` on a `file:`/`http:` URL builds
 *  the connection object without touching the network, and that object's
 *  header/property surface is the interesting part; `connect()` is never
 *  called.
 *
 *  `URLEncoder`/`URLDecoder` are the mechanical half: the reserved set, the
 *  `+`-vs-`%20` asymmetry (encoder writes `+`, decoder reads BOTH), malformed
 *  escapes, and the charset overloads.
 */
public class L6UrlSweep {
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

    static void dump(String tag, String spec) {
        tv(tag + " ctor", () -> new URL(spec).toString());
        tv(tag + " protocol", () -> new URL(spec).getProtocol());
        tv(tag + " authority", () -> new URL(spec).getAuthority());
        tv(tag + " userInfo", () -> new URL(spec).getUserInfo());
        tv(tag + " host", () -> new URL(spec).getHost());
        tv(tag + " port", () -> new URL(spec).getPort());
        tv(tag + " defaultPort", () -> new URL(spec).getDefaultPort());
        tv(tag + " path", () -> new URL(spec).getPath());
        tv(tag + " file", () -> new URL(spec).getFile());
        tv(tag + " query", () -> new URL(spec).getQuery());
        tv(tag + " ref", () -> new URL(spec).getRef());
        tv(tag + " toExternalForm", () -> new URL(spec).toExternalForm());
        tv(tag + " toURI", () -> new URL(spec).toURI().toString());
    }

    public static void main(String[] args) throws Exception {
        // ---- shapes that separate URL's parser from URI's
        dump("http", "http://user:pw@example.com:8080/a/b?q=1#f");
        dump("httpNoPath", "http://example.com");
        dump("httpDefaultPort", "http://example.com:80/p");
        dump("https", "https://example.com/p");
        dump("file", "file:/tmp/x");
        dump("fileTripleSlash", "file:///tmp/x");
        dump("fileHost", "file://server/share/x");
        dump("jar", "jar:file:/x.jar!/a/b");
        dump("ftp", "ftp://h/p");
        dump("emptyPort", "http://h:/p");
        dump("spaceInPath", "http://h/a b/c");
        dump("noSlashPath", "http://h?q");
        dump("refOnly", "http://h/p#");
        dump("queryEmpty", "http://h/p?");
        dump("relDotSegs", "http://h/a/./b/../c");
        dump("ipv6", "http://[::1]:80/p");
        dump("backslash", "http://h/a\\b");
        dump("upperProto", "HTTP://H/P");

        // ---- the protocols with no handler, and the malformed forms
        for (String s : new String[] {
                "", "no-colon", "unknownproto://h/p", "://h/p", "1abc://h/p",
                "http:", "http://h:notaport/p", "http://h:-1/p", "http://[::1/p"})
            tv("bad " + s, () -> new URL(s).toExternalForm());
        tv("null spec", () -> new URL((String) null).toExternalForm());

        // ---- the context constructors, which is where relative resolution lives
        String[][] ctx = {
            {"http://h/a/b/c", "d"}, {"http://h/a/b/c", "/d"},
            {"http://h/a/b/c", "../d"}, {"http://h/a/b/c", "?q"},
            {"http://h/a/b/c", "#f"}, {"http://h/a/b/c", ""},
            {"http://h/a/b/c", "//other/x"}, {"http://h/a/b/c", "https://o/x"},
            {"file:/a/b/c", "d"}, {"jar:file:/x.jar!/a/b", "c"},
        };
        for (String[] c : ctx)
            tv("ctx " + c[0] + " + " + c[1], () -> new URL(new URL(c[0]), c[1]).toExternalForm());
        tv("ctx null context", () -> new URL((URL) null, "http://h/p").toExternalForm());
        tv("ctx null spec", () -> new URL(new URL("http://h/p"), null).toExternalForm());

        // ---- the component constructors
        tv("ctor 3", () -> new URL("http", "h", "/p").toExternalForm());
        tv("ctor 4", () -> new URL("http", "h", 8080, "/p").toExternalForm());
        tv("ctor 4 negPort", () -> new URL("http", "h", -1, "/p").toExternalForm());
        tv("ctor 4 nullHost", () -> new URL("http", null, 80, "/p").toExternalForm());
        tv("ctor 4 nullFile", () -> new URL("http", "h", 80, null).toExternalForm());
        tv("ctor 3 unknownProto", () -> new URL("zzz", "h", "/p").toExternalForm());
        tv("ctor 4 ipv6 host", () -> new URL("http", "[::1]", 80, "/p").toExternalForm());
        tv("ctor 4 ipv6 bare", () -> new URL("http", "::1", 80, "/p").toExternalForm());

        // ---- equals/hashCode/sameFile between IDENTICAL host texts only, so
        // the spec's resolver dependence cannot make the answer host-specific
        String[][] pairs = {
            {"http://h/p", "http://h/p"}, {"http://h/p#a", "http://h/p#b"},
            {"http://h:80/p", "http://h/p"}, {"http://h/p", "HTTP://h/p"},
            {"http://h/P", "http://h/p"}, {"http://h/p?q", "http://h/p"},
            {"file:/a", "file:/a"},
        };
        for (String[] q : pairs) {
            tv("equals " + q[0] + " " + q[1], () -> new URL(q[0]).equals(new URL(q[1])));
            tv("hashEq " + q[0] + " " + q[1], () -> new URL(q[0]).hashCode() == new URL(q[1]).hashCode());
            tv("sameFile " + q[0] + " " + q[1], () -> new URL(q[0]).sameFile(new URL(q[1])));
        }
        tv("equals null", () -> new URL("http://h/p").equals(null));
        tv("equals other type", () -> new URL("http://h/p").equals("http://h/p"));

        // ---- URI <-> URL round-trips, where the two parsers disagree by design
        for (String s : new String[] {
                "http://h/a b", "http://h/a%20b", "http://h/p#f", "file:/tmp/x"})
            tv("toURI " + s, () -> {
                try { return new URL(s).toURI().toString(); }
                catch (Throwable e) { return "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()); }
            });

        // ---- openConnection WITHOUT connecting: the object, its class family,
        // and the property surface that is pure bookkeeping
        tv("openConnection file class", () -> {
            URLConnection c = new URL("file:/tmp/x").openConnection();
            return c.getClass().getSuperclass().getName();
        });
        tv("openConnection http is HttpURLConnection", () -> {
            URLConnection c = new URL("http://h/p").openConnection();
            return c instanceof HttpURLConnection;
        });
        tv("openConnection https is HttpsURLConnection", () -> {
            URLConnection c = new URL("https://h/p").openConnection();
            return (c instanceof javax.net.ssl.HttpsURLConnection) + " " + (c instanceof HttpURLConnection);
        });
        tv("openConnection null proxy", () -> {
            URLConnection c = new URL("http://h/p").openConnection(null);
            return c.getClass().getName();
        });
        tv("openConnection direct proxy", () -> {
            URLConnection c = new URL("http://h/p").openConnection(Proxy.NO_PROXY);
            return c instanceof HttpURLConnection;
        });

        // ---- URLEncoder / URLDecoder: the reserved set and the `+` asymmetry
        String[] enc = {
            "", "abc", "a b", "a+b", "a/b", "a?b", "a&b=c", "a%b", "a*b",
            "a-b_c.d~e", "a~b", "!*'()", "\u00e9", "\u4e2d\u6587",
            "\ud83d\ude00", "a\nb", "a\u0000b",
        };
        for (String s : enc) {
            tv("encode " + esc(s), () -> URLEncoder.encode(s, "UTF-8"));
            tv("encodeCS " + esc(s), () -> URLEncoder.encode(s, StandardCharsets.UTF_8));
            tv("encode-8859 " + esc(s), () -> URLEncoder.encode(s, "ISO-8859-1"));
            tv("roundtrip " + esc(s), () -> URLDecoder.decode(URLEncoder.encode(s, "UTF-8"), "UTF-8").equals(s));
        }
        String[] dec = {
            "", "abc", "a+b", "a%20b", "a%2Bb", "a%", "a%z", "a%2", "a%zz",
            "%C3%A9", "%c3%a9", "%E4%B8%AD", "a%00b", "+++", "%2F", "a%%20b",
        };
        for (String s : dec) {
            tv("decode " + esc(s), () -> URLDecoder.decode(s, "UTF-8"));
            tv("decodeCS " + esc(s), () -> URLDecoder.decode(s, StandardCharsets.UTF_8));
            tv("decode-8859 " + esc(s), () -> URLDecoder.decode(s, "ISO-8859-1"));
        }
        tv("encode null charset name", () -> URLEncoder.encode("a b", (String) null));
        tv("encode unknown charset", () -> URLEncoder.encode("a b", "NoSuchCharset-1"));
        tv("decode null charset name", () -> URLDecoder.decode("a+b", (String) null));
        tv("decode unknown charset", () -> URLDecoder.decode("a+b", "NoSuchCharset-1"));
        tv("encode null input", () -> URLEncoder.encode(null, "UTF-8"));
        tv("decode null input", () -> URLDecoder.decode(null, "UTF-8"));

        // ---- URLConnection's static helpers, which are pure logic
        for (String s : new String[] {
                "x.html", "x.htm", "x.txt", "x.gif", "x.jpg", "x.png", "x.class",
                "x.jar", "x.zip", "x.unknownext", "x", "", "dir/x.html", "X.HTML"})
            tv("guessContentTypeFromName " + s, () -> String.valueOf(URLConnection.guessContentTypeFromName(s)));
        tv("guessContentTypeFromName null", () -> String.valueOf(URLConnection.guessContentTypeFromName(null)));
        tv("guessContentTypeFromStream gif", () -> {
            byte[] b = "GIF89a....".getBytes(StandardCharsets.ISO_8859_1);
            return String.valueOf(URLConnection.guessContentTypeFromStream(new ByteArrayInputStream(b)));
        });
        tv("guessContentTypeFromStream html", () -> {
            byte[] b = "<html><body>".getBytes(StandardCharsets.ISO_8859_1);
            return String.valueOf(URLConnection.guessContentTypeFromStream(new ByteArrayInputStream(b)));
        });
        tv("guessContentTypeFromStream unmarkable", () -> {
            InputStream in = new InputStream() {
                public int read() { return -1; }
                public boolean markSupported() { return false; }
            };
            return String.valueOf(URLConnection.guessContentTypeFromStream(in));
        });
        tv("getDefaultAllowUserInteraction", () -> URLConnection.getDefaultAllowUserInteraction());
        tv("getFileNameMap not null", () -> URLConnection.getFileNameMap() != null);
        tv("fileNameMap html", () -> String.valueOf(URLConnection.getFileNameMap().getContentTypeFor("x.html")));

        System.out.println("rows " + rows);
        System.out.println("DONE L6UrlSweep");
    }
}
