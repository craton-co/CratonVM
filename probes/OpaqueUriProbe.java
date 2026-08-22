import java.net.URI;
import java.net.URISyntaxException;

/**
 * An OPAQUE `java.net.URI` -- one whose scheme-specific part does not begin
 * with `/` -- stores its entire SSP as one undivided string. It has NO
 * authority, NO path, NO query and NO user-info; `?` and `@` inside the SSP
 * are ordinary characters. Only the FRAGMENT is split off.
 *
 * Spring's `WebClientUtils.getRequestDescription` leans on exactly that:
 * `if (uri.getRawUserInfo() == null && uri.getRawQuery() == null &&
 * uri.getRawFragment() == null) return sb.append(uri)`, with the comment
 * "also handles Opaque URI, which has only schemeSpecificPart". CratonVM
 * answered a non-null `rawQuery` for `mailto:user@example.com?subject=hello`,
 * so the method fell through to the hierarchical rebuild and produced
 * `GET mailto:` -- `WebClientUtilsTests.opaqueUriUnchanged`.
 */
public class OpaqueUriProbe {

    static int row = 0;

    static void p(String label, Object v) {
        System.out.println(label + " = " + v);
    }

    static void dump(String tag, String s) {
        URI u;
        try {
            u = new URI(s);
        }
        catch (URISyntaxException ex) {
            p(tag + " CTOR", "THREW URISyntaxException: " + ex.getMessage());
            return;
        }
        p(tag + " .toString", u.toString());
        p(tag + " .isOpaque", u.isOpaque());
        p(tag + " .isAbsolute", u.isAbsolute());
        p(tag + " .getScheme", u.getScheme());
        p(tag + " .getSchemeSpecificPart", u.getSchemeSpecificPart());
        p(tag + " .getRawSchemeSpecificPart", u.getRawSchemeSpecificPart());
        p(tag + " .getAuthority", u.getAuthority());
        p(tag + " .getRawAuthority", u.getRawAuthority());
        p(tag + " .getUserInfo", u.getUserInfo());
        p(tag + " .getRawUserInfo", u.getRawUserInfo());
        p(tag + " .getHost", u.getHost());
        p(tag + " .getPort", u.getPort());
        p(tag + " .getPath", u.getPath());
        p(tag + " .getRawPath", u.getRawPath());
        p(tag + " .getQuery", u.getQuery());
        p(tag + " .getRawQuery", u.getRawQuery());
        p(tag + " .getFragment", u.getFragment());
        p(tag + " .getRawFragment", u.getRawFragment());
        p(tag + " .normalize", u.normalize().toString());
        // The Spring predicate, verbatim.
        boolean plain = u.getRawUserInfo() == null && u.getRawQuery() == null
                && u.getRawFragment() == null;
        p(tag + " SPRING-plain", plain);
        p(tag + " SPRING-desc", springDescription(u));
        // Round-trip identity.
        try {
            URI again = new URI(u.toString());
            p(tag + " roundtrip.equals", again.equals(u));
            p(tag + " roundtrip.toString", again.toString());
        }
        catch (URISyntaxException ex) {
            p(tag + " roundtrip", "THREW " + ex.getMessage());
        }
    }

    /** WebClientUtils.getRequestDescription, verbatim. */
    static String springDescription(URI uri) {
        StringBuilder sb = new StringBuilder().append("GET").append(" ");
        if (uri.getRawUserInfo() == null && uri.getRawQuery() == null && uri.getRawFragment() == null) {
            return sb.append(uri).toString();
        }
        if (uri.getScheme() != null) {
            sb.append(uri.getScheme()).append(':');
        }
        if (uri.getHost() != null) {
            sb.append("//");
            String host = uri.getHost();
            if (host.indexOf(':') >= 0 && !host.startsWith("[") && !host.endsWith("]")) {
                sb.append('[').append(host).append(']');
            }
            else {
                sb.append(host);
            }
            if (uri.getPort() != -1) {
                sb.append(':').append(uri.getPort());
            }
        }
        if (uri.getRawPath() != null) {
            sb.append(uri.getRawPath());
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        // ---- opaque -----------------------------------------------------
        dump("O01", "mailto:user@example.com?subject=hello");
        dump("O02", "mailto:user@example.com");
        dump("O03", "mailto:a@b.com#frag");
        dump("O04", "mailto:a@b.com?s=1#frag");
        dump("O05", "news:comp.lang.java");
        dump("O06", "urn:isbn:096139210x");
        dump("O07", "tel:+1-816-555-1212");
        dump("O08", "urn:uuid:6e8bc430-9c3a-11d9-9669-0800200c9a66");
        dump("O09", "classpath:foo/bar.xml?a=b");
        dump("O10", "jar:file:/tmp/x.jar!/a/b.class");
        dump("O11", "http:comp.lang.java?q=1");
        dump("O12", "a:b/c?d=e");
        dump("O13", "mailto:?subject=hello");

        // ---- hierarchical, as the negative control ----------------------
        dump("H01", "https://api.example.com/search?q=test&page=1");
        dump("H02", "https://admin:secret@host/api?token=abc");
        dump("H03", "https://host/page?q=1#section");
        dump("H04", "http://[::1]:8080/path?q=1");
        dump("H05", "/api/search?q=test");
        dump("H06", "file:/tmp/x?a=b");
        dump("H07", "https://api.example.com/health");
        dump("H08", "https://host/page#frag?param=value");
        dump("H09", "//authority/only?q=1");
        dump("H10", "relative/path?q=1#f");
        // A colon inside a RELATIVE path is not a scheme delimiter, so this is
        // hierarchical with a query -- the counterexample to a naive
        // `text.contains(":")` opacity test.
        dump("H11", "/redirect:account?q=1");
        dump("H12", "redirect:account/x");

        // ---- the multi-argument constructors ----------------------------
        URI c1 = new URI("mailto", "user@example.com?subject=hello", null);
        p("C01 .toString", c1.toString());
        p("C01 .isOpaque", c1.isOpaque());
        p("C01 .getRawQuery", c1.getRawQuery());
        p("C01 .getRawSchemeSpecificPart", c1.getRawSchemeSpecificPart());
        p("C01 SPRING-desc", springDescription(c1));

        URI c2 = new URI("mailto", "user@example.com", "frag");
        p("C02 .toString", c2.toString());
        p("C02 .getRawFragment", c2.getRawFragment());
        p("C02 .getRawSchemeSpecificPart", c2.getRawSchemeSpecificPart());

        URI c3 = URI.create("mailto:user@example.com?subject=hello");
        p("C03 create .toString", c3.toString());
        p("C03 create .getRawQuery", c3.getRawQuery());
        p("C03 create .isOpaque", c3.isOpaque());

        // ---- resolve / relativize on an opaque base ---------------------
        p("R01 opaque.resolve", URI.create("mailto:a@b").resolve("c@d").toString());
        p("R02 base.resolve(opaque)",
                URI.create("https://h/a/b").resolve("mailto:a@b").toString());
        p("R03 equals-differing-ssp", URI.create("mailto:a@b?x=1")
                .equals(URI.create("mailto:a@b?x=2")));
        p("R04 compareTo", Integer.signum(URI.create("mailto:a@b?x=1")
                .compareTo(URI.create("mailto:a@b?x=2"))));
        p("R05 hashCode-stable", URI.create("mailto:a@b?x=1").hashCode()
                == URI.create("mailto:a@b?x=1").hashCode());
        p("R06 opaque.resolve(abs)",
                URI.create("mailto:a@b").resolve("https://h/p").toString());
        p("R07 opaque.resolve(empty)", URI.create("mailto:a@b").resolve("").toString());
        p("R08 hier.resolve(opaque-with-query)",
                URI.create("https://h/a/b").resolve("mailto:c@d?x=1").toString());
        p("R09 opaque.normalize", URI.create("mailto:a/../b@c").normalize().toString());
        p("R10 opaque.relativize",
                URI.create("mailto:a@b").relativize(URI.create("mailto:a@b/c")).toString());
        p("R11 hier.relativize(opaque)",
                URI.create("https://h/a/").relativize(URI.create("mailto:a@b")).toString());
        p("R12 opaque.getSSP-after-resolve",
                URI.create("https://h/a/b").resolve("mailto:c@d?x=1").getSchemeSpecificPart());

        // The adjacent hierarchical rows, measured so the opaque fix is not
        // credited with -- or blamed for -- anything in this neighbourhood.
        p("R13 hier.resolve(empty)", URI.create("https://h/a/b?q=1").resolve("").toString());
        p("R14 hier.resolve(lone-fragment)",
                URI.create("https://h/a/b?q=1").resolve("#f").toString());
        p("R15 hier.resolve(abs-with-dots)",
                URI.create("https://h/a/b").resolve("https://x/p/../q").toString());
        p("R16 hier.resolve(rel)", URI.create("https://h/a/b").resolve("c").toString());

        // `URI.resolve` is RFC 2396, not RFC 3986, and its empty-reference and
        // scheme-carrying-reference arms are where that shows. Every row is
        // measured, not derived.
        String[][] rr = {
            {"https://h/a/b?q=1", ""},
            {"https://h/a/b?q=1", "#f"},
            {"https://h/a/b?q=1", "?q=2"},
            {"https://h/a/b?q=1", "?q=2#f"},
            {"https://h/a/b?q=1", "."},
            {"https://h/a/b?q=1", ".."},
            {"https://h/a/b?q=1", "/x"},
            {"https://h/a/b?q=1", "c"},
            {"https://h/a/b?q=1", "//other/p"},
            {"https://h/a/b?q=1", "//other/p/../q"},
            {"https://h/a/b?q=1", "https://x/p/../q"},
            {"https://h/a/b?q=1", "https://x/p/../q?z=1#w"},
            {"https://h/a/b?q=1", "mailto:c@d?x=1"},
            {"https://h/a/", ""},
            {"https://h/a", ""},
            {"https://h/", ""},
            {"https://h", ""},
            {"https://h/a/b#g", "#g"},
            {"https://h/a/b#g", ""},
            {"https://h/a/b", "c/../d"},
            {"https://h/a/b", "./e"},
            {"/base/path?q=1", ""},
            {"/base/path?q=1", "x"},
            {"mailto:a@b", "#f"},
            {"https://h/a/b?q=1", "/p/../q"},
            {"https://h/a/b?q=1", "/p/./q"},
            {"https://h/a/b", "../../x"},
            {"https://h/a/b", "?"},
            {"https://h/a/b", "#"},
            {"file:/a/b", ""},
            {"https://h/a/b", "//other"},
            {"https://h/a/b?q=1", "//other?z=2"},
            {"https://h/a/b", "c#f"},
            {"https://h/a/b", "//other/p?z=2#w"},
        };
        String[] nn = {
            "https://h/a/../../x", "/a/../../x", "https://h/a/b/../c", "a/../../b",
            "https://h/a/./b", "https://h/a/b/..", "https://h/a/b/.", "/../x",
            "https://h/../x", "a/b/../../../c", "https://h/a//b", "./a/b",
        };
        int k = 0;
        for (String u : nn) {
            k++;
            try {
                p(String.format("N%02d %s .normalize", k, u), URI.create(u).normalize().toString());
            }
            catch (Exception ex) {
                p(String.format("N%02d %s .normalize", k, u), "THREW " + ex);
            }
        }
        int i = 0;
        for (String[] pair : rr) {
            i++;
            String label = String.format("S%02d %s resolve %s", i, pair[0],
                    pair[1].isEmpty() ? "<empty>" : pair[1]);
            try {
                p(label, URI.create(pair[0]).resolve(pair[1]).toString());
            }
            catch (Exception ex) {
                p(label, "THREW " + ex.getClass().getName() + ": " + ex.getMessage());
            }
        }
    }
}
