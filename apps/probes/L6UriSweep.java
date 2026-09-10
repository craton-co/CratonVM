import java.net.*;

/** L6 — `java.net.URI`, 30 owning `Bridge` registrations, every one bucket A.
 *
 *  The lane page calls this "the best mechanical wave here": no VM-filled
 *  state, no service loading, no I/O. What it exercises is RFC 3986 parsing,
 *  which the JDK implements exactly and a hand-written native approximates.
 *
 *  Every row asks a CONTRACT EDGE rather than a happy path, because that is
 *  where the first 48 defects of this campaign lived. The shapes that separate
 *  implementations: opaque vs hierarchical, an empty authority, `file:///`,
 *  IPv6 literals in brackets, percent-encoding in each component (raw vs
 *  decoded accessors), `resolve`/`relativize` round-trips, `normalize` walking
 *  `..` above the root, and the case rules of
 *  `equals`/`hashCode`/`compareTo` — scheme and host are case-insensitive,
 *  path is not.
 *
 *  `URISyntaxException` is printed with its REASON and INDEX, not just its
 *  type: a bare exception type assertion cannot say which check fired.
 *
 *  Nothing here prints a value the two VMs may choose independently. `URI` has
 *  no identity in its rendering and its `hashCode` is a pure function of its
 *  components, so both are stable across runs of the same VM.
 */
public class L6UriSweep {
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
        catch (URISyntaxException e) {
            p(tag, "URISyntaxException reason=" + esc(e.getReason())
                   + " index=" + e.getIndex() + " input=" + esc(e.getInput())
                   + " msg=" + esc(e.getMessage()));
        }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    /** Every accessor of one URI, so a fix to one that breaks another cannot hide. */
    static void dump(String tag, String spec) {
        tv(tag + " ctor", () -> new URI(spec).toString());
        tv(tag + " scheme", () -> new URI(spec).getScheme());
        tv(tag + " ssp", () -> new URI(spec).getSchemeSpecificPart());
        tv(tag + " rawSsp", () -> new URI(spec).getRawSchemeSpecificPart());
        tv(tag + " authority", () -> new URI(spec).getAuthority());
        tv(tag + " rawAuthority", () -> new URI(spec).getRawAuthority());
        tv(tag + " userInfo", () -> new URI(spec).getUserInfo());
        tv(tag + " rawUserInfo", () -> new URI(spec).getRawUserInfo());
        tv(tag + " host", () -> new URI(spec).getHost());
        tv(tag + " port", () -> new URI(spec).getPort());
        tv(tag + " path", () -> new URI(spec).getPath());
        tv(tag + " rawPath", () -> new URI(spec).getRawPath());
        tv(tag + " query", () -> new URI(spec).getQuery());
        tv(tag + " rawQuery", () -> new URI(spec).getRawQuery());
        tv(tag + " fragment", () -> new URI(spec).getFragment());
        tv(tag + " rawFragment", () -> new URI(spec).getRawFragment());
        tv(tag + " isAbsolute", () -> new URI(spec).isAbsolute());
        tv(tag + " isOpaque", () -> new URI(spec).isOpaque());
        tv(tag + " hashCode", () -> new URI(spec).hashCode());
        tv(tag + " normalize", () -> new URI(spec).normalize().toString());
        tv(tag + " toASCIIString", () -> new URI(spec).toASCIIString());
    }

    public static void main(String[] args) {
        // ---- the two shapes the RFC separates, and the ones that are neither
        dump("hier", "http://user:pw@example.com:8080/a/b?q=1#f");
        dump("opaque", "mailto:someone@example.com");
        dump("opaqueFrag", "news:comp.lang.java#top");
        dump("relative", "a/b/c?q#f");
        dump("emptyAuth", "file:///tmp/x");
        dump("authNoPath", "http://example.com");
        dump("rootPath", "http://example.com/");
        dump("noScheme", "//example.com/p");
        dump("schemeOnly", "s:");
        dump("empty", "");
        dump("fragOnly", "#frag");
        dump("queryOnly", "?q=1");
        dump("dotSegs", "http://h/a/./b/../c/");
        dump("aboveRoot", "http://h/../../x");
        dump("relAboveRoot", "../../x/y");
        dump("ipv6", "http://[::1]:80/p");
        dump("ipv6full", "http://[2001:db8:0:0:0:0:2:1]/p");
        dump("ipv6zone", "http://[fe80::1%25eth0]/p");
        dump("ipv4", "http://127.0.0.1:9/p");
        dump("regName", "http://ex_am-ple.com./p");
        dump("pctPath", "http://h/a%20b/c%2Fd");
        dump("pctQuery", "http://h/p?a%3Db=c%26d");
        dump("pctUser", "http://us%3Aer:p%40w@h/p");
        dump("pctFrag", "http://h/p#a%20b");
        dump("pctNonAscii", "http://h/%C3%A9");
        dump("rawNonAscii", "http://h/\u00e9");
        dump("plusNotSpace", "http://h/p?a+b=c");
        dump("emptyPort", "http://h:/p");
        dump("zeroPort", "http://h:0/p");
        dump("bigPort", "http://h:65535/p");
        dump("caseScheme", "HTTP://EXAMPLE.COM/Path");
        dump("trailingDot", "http://h/a/b/.");
        dump("doubleSlashPath", "http://h//a//b");
        dump("colonInFirstSeg", "a:b/c");
        dump("jarNested", "jar:file:/x.jar!/a/b");
        dump("uncPath", "file://server/share/x");
        dump("windowsish", "file:///C:/tmp/x");

        // ---- the constructor's own validation, by reason and index
        String[] bad = {
            "http://h/ space", "http://h/%", "http://h/%zz", "http://h/%2",
            "http://[::1/p", "http://h:port/p", "http://h:-1/p",
            "1http://h/p", ":no-scheme", "http://h/p#a#b",
            "http://us@er@h/p", "http://h/p?q#f#g", "\u0000",
            "http://", "http://@/p", "http://h:99999999999/p",
            "http://h/a<b", "http://h/a>b", "http://h/a\"b", "http://h/a{b}",
            "http://h/a|b", "http://h/a\\b", "http://h/a^b",
            "  http://h/p  ", "http:\u00e9",
        };
        for (String s : bad) tv("bad " + s, () -> new URI(s).toString());

        // ---- URI.create wraps the checked exception; the wrapper's shape matters
        for (String s : new String[] {"http://h/ space", "http://h/%zz", "ok:/x"}) {
            tv("create " + s, () -> URI.create(s).toString());
            tv("create-cause " + s, () -> {
                try { URI.create(s); return "no throw"; }
                catch (IllegalArgumentException e) {
                    Throwable c = e.getCause();
                    return e.getClass().getName() + " msg=" + esc(e.getMessage())
                         + " cause=" + (c == null ? "null" : c.getClass().getName());
                }
            });
        }

        // ---- the multi-argument constructors, which QUOTE rather than reject
        tv("ctor 3 quotes", () -> new URI("http", "h/a b", "frag ment").toString());
        tv("ctor 4", () -> new URI("http", "ex.com", "/a b", "f g").toString());
        tv("ctor 5", () -> new URI("http", "ex.com", "/a b", "q=1 2", "f g").toString());
        tv("ctor 7", () -> new URI("http", "u i", "ex.com", 8080, "/a b", "q= 1", "f g").toString());
        tv("ctor 7 rawQuery", () -> new URI("http", "u i", "ex.com", 8080, "/a b", "q= 1", "f g").getRawQuery());
        tv("ctor 7 query", () -> new URI("http", "u i", "ex.com", 8080, "/a b", "q= 1", "f g").getQuery());
        tv("ctor 3 nullSsp", () -> new URI("http", null, null).toString());
        tv("ctor 5 relPathNoAuth", () -> new URI(null, null, "a/b", null, null).toString());
        tv("ctor 5 pathNoSlash+auth", () -> new URI("http", "h", "noslash", null, null).toString());
        tv("ctor 7 negPort", () -> new URI("http", null, "h", -1, "/p", null, null).toString());

        // ---- resolve / relativize, including the identities the spec fixes
        String[][] res = {
            {"http://h/a/b/c", "d"}, {"http://h/a/b/c", "/d"},
            {"http://h/a/b/c", "../d"}, {"http://h/a/b/c", "?q"},
            {"http://h/a/b/c", "#f"}, {"http://h/a/b/c", ""},
            {"http://h/a/b/c", "//other/x"}, {"http://h/a/b/c", "s:opaque"},
            {"http://h/a/b/c#f", "d"}, {"mailto:a@b", "c"},
            {"http://h", "d"}, {"http://h/", "d"},
            {"a/b/c", "d"}, {"http://h/a/b/c", "../../../../x"},
        };
        for (String[] r : res) {
            tv("resolve " + r[0] + " + " + r[1], () -> new URI(r[0]).resolve(r[1]).toString());
            tv("resolveU " + r[0] + " + " + r[1], () -> new URI(r[0]).resolve(new URI(r[1])).toString());
        }
        String[][] rel = {
            {"http://h/a/", "http://h/a/b/c"}, {"http://h/a/", "http://h/x/y"},
            {"http://h/a/", "http://other/a/b"}, {"http://h/a/", "http://h/a/"},
            {"http://h/a", "http://h/a/b"}, {"mailto:a@b", "http://h/x"},
            {"http://h/a/", "b/c"},
        };
        for (String[] r : rel)
            tv("relativize " + r[0] + " -> " + r[1],
               () -> new URI(r[0]).relativize(new URI(r[1])).toString());

        // ---- normalize: the `..`-above-root rule, and the "leading segment
        // that would otherwise read as a scheme" rule, which is the subtle one
        for (String s : new String[] {
                "http://h/a/../../b", "a/../../b", "./a/b", "a/b/..",
                "a/b/../..", "/../a", "a/./b/./c", "//h/a/../b",
                "x/../y:z", "a/b/../c:d", "http://h", "mailto:a@b"})
            tv("normalize " + s, () -> new URI(s).normalize().toString());

        // ---- equality and ordering: scheme and host fold case, path does not
        String[][] pairs = {
            {"http://h/p", "HTTP://h/p"}, {"http://H/p", "http://h/p"},
            {"http://h/P", "http://h/p"}, {"http://h/%41", "http://h/A"},
            {"http://h/%41", "http://h/%41"}, {"http://h/%41", "http://h/%61"},
            {"mailto:A@b", "mailto:a@b"}, {"http://u@h/p", "http://U@h/p"},
            {"http://h:80/p", "http://h/p"}, {"http://h/p#F", "http://h/p#f"},
        };
        for (String[] q : pairs) {
            tv("equals " + q[0] + " " + q[1], () -> new URI(q[0]).equals(new URI(q[1])));
            tv("hashEq " + q[0] + " " + q[1], () -> new URI(q[0]).hashCode() == new URI(q[1]).hashCode());
            tv("cmp " + q[0] + " " + q[1], () -> Integer.signum(new URI(q[0]).compareTo(new URI(q[1]))));
            tv("cmpRev " + q[0] + " " + q[1], () -> Integer.signum(new URI(q[1]).compareTo(new URI(q[0]))));
        }
        tv("equals null", () -> new URI("http://h/p").equals(null));
        tv("equals other type", () -> new URI("http://h/p").equals("http://h/p"));
        tv("compareTo self 0", () -> new URI("http://h/p").compareTo(new URI("http://h/p")));
        tv("compareTo null NPE", () -> new URI("http://h/p").compareTo((URI) null));

        // ---- parseServerAuthority: promotes a registry-based authority to a
        // server-based one, or explains why it cannot
        for (String s : new String[] {
                "http://h:8080/p", "http://us_er@h/p", "s://a[b]c/p",
                "http://h:x/p", "mailto:a@b", "//h/p", "http://h/p"})
            tv("parseServerAuthority " + s, () -> new URI(s).parseServerAuthority().toString());

        // ---- toURL: only absolute URIs, and only for known protocols
        for (String s : new String[] {
                "http://h/p", "a/b", "mailto:a@b", "unknownproto://h/p", "file:/tmp/x"})
            tv("toURL " + s, () -> {
                try { return new URI(s).toURL().toString(); }
                catch (Throwable e) { return "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()); }
            });

        // ---- shape questions, none of them an identity the VMs may choose
        p("URI class", URI.create("http://h/p").getClass().getName());
        tv("normalize returns receiver when nothing to do", () -> {
            URI u = new URI("http://h/p");
            return u.normalize() == u;
        });
        tv("resolve empty equals receiver", () -> {
            URI u = new URI("http://h/a/b");
            return u.resolve("").equals(u) + " " + u.resolve("").toString();
        });
        tv("toString stable across calls", () -> {
            URI u = new URI("http://h/a%20b?q#f");
            return u.toString().equals(u.toString());
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L6UriSweep");
    }
}
