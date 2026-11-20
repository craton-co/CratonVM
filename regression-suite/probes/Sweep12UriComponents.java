import java.net.*;

/** G61-1 N2: do URI/URL components still carry an unpaired surrogate? */
public class Sweep12UriComponents {
    static final String LONE = "a" + '\ud800' + "b";
    static void em(String l, String s) {
        StringBuilder sb = new StringBuilder("U2 " + l + " =");
        if (s == null) { sb.append(" null"); }
        else for (int i = 0; i < s.length(); i++) sb.append(' ').append(Integer.toHexString(s.charAt(i)));
        System.out.println(sb);
    }
    static void t(String l, java.util.concurrent.Callable<String> c) {
        try { em(l, c.call()); }
        catch (Throwable x) { System.out.println("U2 " + l + " = " + x.getClass().getName()); }
    }

    public static void main(String[] a) throws Exception {
        // A URI whose PATH, QUERY and FRAGMENT each hold the lone unit.
        URI u = new URI("http", "h.example", "/" + LONE, LONE, LONE);
        t("uri_toString", () -> u.toString());
        t("uri_getPath", () -> u.getPath());
        t("uri_getRawPath", () -> u.getRawPath());
        t("uri_getQuery", () -> u.getQuery());
        t("uri_getRawQuery", () -> u.getRawQuery());
        t("uri_getFragment", () -> u.getFragment());
        t("uri_getSSP", () -> u.getSchemeSpecificPart());
        t("uri_getRawSSP", () -> u.getRawSchemeSpecificPart());
        t("uri_getAuthority", () -> u.getAuthority());
        t("uri_getHost", () -> u.getHost());
        t("uri_getScheme", () -> u.getScheme());

        // Built from a single string rather than the multi-arg constructor.
        URI u2 = new URI("http://h.example/" + LONE);
        t("uri2_toString", () -> u2.toString());
        t("uri2_getPath", () -> u2.getPath());
        t("uri2_getRawPath", () -> u2.getRawPath());
        t("uri2_normalize", () -> u2.normalize().toString());
        t("uri2_resolve", () -> u2.resolve("x").toString());
        t("uri2_relativize", () -> URI.create("http://h.example/").relativize(u2).toString());

        // URL side.
        URL url = new URL("http://h.example/" + LONE);
        t("url_toString", () -> url.toString());
        t("url_getPath", () -> url.getPath());
        t("url_getFile", () -> url.getFile());
        t("url_toExternalForm", () -> url.toExternalForm());
        t("url_toURI_path", () -> url.toURI().getPath());

        // Controls: ordinary text must be untouched.
        URI c1 = new URI("http", "h.example", "/ok", "q=1", "f");
        t("ctl_toString", () -> c1.toString());
        t("ctl_path", () -> c1.getPath());
        t("ctl_query", () -> c1.getQuery());
        // A well-formed pair must survive as two units.
        URI c2 = new URI("http", "h.example", "/😀", null, null);
        t("ctl_pair_path", () -> c2.getPath());
    }
}
