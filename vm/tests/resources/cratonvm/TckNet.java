package cratonvm;

import java.net.URI;
import java.net.URL;
import java.net.MalformedURLException;
import java.net.URISyntaxException;

/**
 * JCK-style conformance tests for java.net (NEW-16.2).
 *
 * All tests are static, take no arguments, and return 1 on pass / 0 on fail.
 * No sockets are opened — only URL/URI parsing per RFC 3986.
 */
public class TckNet {

    // URL.getProtocol returns the scheme
    public static int url_getProtocol() {
        try {
            URL u = new URL("https://example.com/path");
            if (!"https".equals(u.getProtocol())) return 0;
            return 1;
        } catch (MalformedURLException e) {
            return 0;
        }
    }

    // URL.getHost returns the authority host
    public static int url_getHost() {
        try {
            URL u = new URL("http://example.com:8080/foo");
            if (!"example.com".equals(u.getHost())) return 0;
            return 1;
        } catch (MalformedURLException e) {
            return 0;
        }
    }

    // URL.getPort returns explicit port, -1 if unspecified
    public static int url_getPort_explicit() {
        try {
            URL u = new URL("http://example.com:8080/foo");
            if (u.getPort() != 8080) return 0;
            return 1;
        } catch (MalformedURLException e) {
            return 0;
        }
    }

    public static int url_getPort_default() {
        try {
            URL u = new URL("http://example.com/foo");
            if (u.getPort() != -1) return 0;
            return 1;
        } catch (MalformedURLException e) {
            return 0;
        }
    }

    // URL.getPath returns the path component
    public static int url_getPath() {
        try {
            URL u = new URL("http://example.com/a/b/c");
            if (!"/a/b/c".equals(u.getPath())) return 0;
            return 1;
        } catch (MalformedURLException e) {
            return 0;
        }
    }

    // URL.getQuery returns the query string
    public static int url_getQuery() {
        try {
            URL u = new URL("http://example.com/x?a=1&b=2");
            if (!"a=1&b=2".equals(u.getQuery())) return 0;
            return 1;
        } catch (MalformedURLException e) {
            return 0;
        }
    }

    // Malformed URL throws
    public static int url_malformed_throws() {
        try {
            new URL("not a url at all");
            return 0;
        } catch (MalformedURLException e) {
            return 1;
        }
    }

    // URI parsing
    public static int uri_parse() {
        try {
            URI u = new URI("https://user@example.com:443/path?q=v#frag");
            if (!"https".equals(u.getScheme())) return 0;
            if (!"example.com".equals(u.getHost())) return 0;
            if (u.getPort() != 443) return 0;
            if (!"/path".equals(u.getPath())) return 0;
            return 1;
        } catch (URISyntaxException e) {
            return 0;
        }
    }

    // URI getScheme
    public static int uri_getScheme() {
        try {
            URI u = new URI("file:///tmp/x");
            if (!"file".equals(u.getScheme())) return 0;
            return 1;
        } catch (URISyntaxException e) {
            return 0;
        }
    }

    // URI relative
    public static int uri_relative() {
        try {
            URI u = new URI("/relative/path");
            if (u.isAbsolute()) return 0;
            if (!"/relative/path".equals(u.getPath())) return 0;
            return 1;
        } catch (URISyntaxException e) {
            return 0;
        }
    }
}
