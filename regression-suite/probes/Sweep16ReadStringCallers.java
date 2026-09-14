import java.io.File;
import java.net.*;
import java.nio.file.*;
import java.text.SimpleDateFormat;
import java.util.*;

/**
 * Sweep 16: the read_string caller audit (G70-1 N1), probed rather than grepped.
 *
 * The audit set is not "every read_string call site" (2904 of those). It is the
 * 18 sites that BOTH round-trip a value back to Java AND are actually invoked
 * under --jdk-only, found by joining a static scan to `--dump-native-registry`
 * invocation counts. This probe asks each of those 18 the one question the grep
 * cannot answer: is the value merely INSPECTED, or handed BACK?
 *
 * A lone surrogate is the instrument. A Rust `str` cannot hold one, so any value
 * that survives it round-trip was never routed through `read_string`; any value
 * that comes back as fffd was. Every row below is a pure string operation — no
 * row requires the filesystem to hold such a name, because `File` is a string
 * wrapper until something touches the disk.
 */
public class Sweep16ReadStringCallers {
    interface C { Object g() throws Exception; }
    static void t(String l, C c) {
        try { System.out.println("S " + l + " = " + c.g()); }
        catch (Throwable x) {
            System.out.println("S " + l + " = " + x.getClass().getName() + " | " + x.getMessage());
        }
    }
    /** Code units in hex — the only rendering that shows a substitution. */
    static String u(String s) {
        if (s == null) return "<null>";
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) b.append(',');
            b.append(Integer.toHexString(s.charAt(i)));
        }
        return b.toString();
    }

    static final String LONE = "\uD83D";        // a high surrogate, unpaired
    static final String LONE_LO = "\uDC00";     // a low surrogate, unpaired
    static final String PAIR = "😀";  // a well-formed pair (control)
    static final String HI = "é中";    // non-ASCII but well-formed (control)

    public static void main(String[] a) {
        // ---- File: the dominant family, 148 of the 18 sites' invocations ------
        t("file_ctor_lone_getPath",   () -> u(new File("d/" + LONE + "x").getPath()));
        t("file_ctor_lone_toString",  () -> u(new File("d/" + LONE + "x").toString()));
        t("file_ctor_lone_getName",   () -> u(new File("d/" + LONE + "x").getName()));
        t("file_ctor_lone_getParent", () -> u(new File(LONE + "p/" + LONE + "x").getParent()));
        t("file_ctor_lo_getPath",     () -> u(new File("d/" + LONE_LO + "x").getPath()));
        t("file_ctor_pair_getPath",   () -> u(new File("d/" + PAIR + "x").getPath()));
        t("file_ctor_hi_getPath",     () -> u(new File("d/" + HI + "x").getPath()));
        t("file_ctor_ascii_getPath",  () -> u(new File("d/plain.txt").getPath()));

        t("file_pc_lone_getPath",     () -> u(new File("p" + LONE, "c" + LONE).getPath()));
        t("file_pc_lone_getName",     () -> u(new File("p" + LONE, "c" + LONE).getName()));
        t("file_fc_lone_getPath",     () -> u(new File(new File("p" + LONE), "c" + LONE).getPath()));
        t("file_pc_ascii_getPath",    () -> u(new File("p", "c").getPath()));

        t("file_lone_length",         () -> new File("d/" + LONE + "x").getPath().length());
        t("file_lone_equals",         () -> new File("d/" + LONE + "x").equals(new File("d/" + LONE + "x")));
        t("file_lone_vs_fffd",        () -> new File("d/" + LONE + "x").equals(new File("d/�x")));
        t("file_lone_hash_differs",   () -> new File("d/" + LONE + "x").hashCode()
                                            != new File("d/�x").hashCode());
        t("file_lone_compareTo",      () -> Integer.signum(new File("d/" + LONE + "x")
                                            .compareTo(new File("d/�x"))));
        t("file_lone_isAbsolute",     () -> new File("d/" + LONE + "x").isAbsolute());
        t("file_lone_separatorChar",  () -> u(new File("d/" + LONE + "x").getPath()).contains("2f")
                                            || File.separatorChar == '\\');

        // ---- Path / FileSystem -------------------------------------------------
        t("path_get_lone_toString",   () -> u(Paths.get("d", LONE + "x").toString()));
        t("path_get_lone_getFileName",() -> u(Paths.get("d", LONE + "x").getFileName().toString()));
        t("path_get_pair_toString",   () -> u(Paths.get("d", PAIR + "x").toString()));
        t("path_matcher_lone",        () -> FileSystems.getDefault()
                                            .getPathMatcher("glob:*" + LONE + "*")
                                            .matches(Paths.get(LONE + "y")));
        t("path_matcher_ascii",       () -> FileSystems.getDefault()
                                            .getPathMatcher("glob:*.txt")
                                            .matches(Paths.get("a.txt")));

        // ---- DateFormat: 48 invocations, literal text in the pattern -----------
        t("sdf_lone_literal", () -> {
            SimpleDateFormat f = new SimpleDateFormat("'" + LONE + "'yyyy", Locale.ROOT);
            f.setTimeZone(TimeZone.getTimeZone("UTC"));
            return u(f.format(new Date(0L)));
        });
        t("sdf_pair_literal", () -> {
            SimpleDateFormat f = new SimpleDateFormat("'" + PAIR + "'yyyy", Locale.ROOT);
            f.setTimeZone(TimeZone.getTimeZone("UTC"));
            return u(f.format(new Date(0L)));
        });
        t("sdf_ascii_literal", () -> {
            SimpleDateFormat f = new SimpleDateFormat("'T'yyyy-MM-dd", Locale.ROOT);
            f.setTimeZone(TimeZone.getTimeZone("UTC"));
            return u(f.format(new Date(0L)));
        });

        // ---- URI / URL: mostly ASCII by grammar, kept as CONTROLS --------------
        t("uri_scheme_ascii", () -> {
            try { return new URI("http://h/p?q#f").getScheme(); }
            catch (URISyntaxException e) { return "USE|" + e.getMessage(); }
        });
        t("uri_host_ascii", () -> {
            try { return new URI("http://h.example/p").getHost(); }
            catch (URISyntaxException e) { return "USE|" + e.getMessage(); }
        });
        t("uri_lone_rejected", () -> {
            try { return u(new URI("http://h/" + LONE).getPath()); }
            catch (URISyntaxException e) { return "USE"; }
        });
        t("url_protocol_ascii", () -> {
            try { return new URL("http://h/p").getProtocol(); }
            catch (MalformedURLException e) { return "MUE"; }
        });
        t("isa_tostring_ascii", () -> new InetSocketAddress("127.0.0.1", 80).toString());

        // ---- the inspected side: values that SHOULD go through read_string -----
        // A class name, a charset name and a descriptor cannot hold a lone
        // surrogate, so `read_string` is the right reader there. These rows exist
        // to prove the audit did not simply convert everything in sight.
        t("classname_inspected", () -> String.class.getName());
        t("charset_inspected",   () -> "x".getBytes(java.nio.charset.StandardCharsets.UTF_8).length);
        t("descriptor_inspected",() -> int[].class.getName());


        // ---- hashCode/compareTo are computed FROM the path: check the value,
        // ---- not just that two paths differ (see G70-1 N1 addendum).
        t("file_hash_ascii",   () -> new File("AB").hashCode());
        t("file_hash_oracle",  () -> new File("AB").hashCode() == ("AB".hashCode() ^ 1234321));
        t("file_hash_nonascii",() -> new File(String.valueOf((char) 0xE9) + "B").hashCode());
        t("file_hash_case",    () -> new File("ab").hashCode() == new File("AB").hashCode());
        t("file_cmp_case",     () -> Integer.signum(new File("ab").compareTo(new File("AB"))));
        t("file_eq_case",      () -> new File("ab").equals(new File("AB")));


        // The fourth File constructor. It was NOT in the live set (zero
        // invocations across the corpus), so the audit never reached it; this
        // row asks whether the sibling is fixed rather than assuming it
        // (G70-1 N2).
        t("file_uri_ctor_getPath", () -> {
            try { return u(new File(new URI("file:///d/" + LONE + "x")).getPath()); }
            catch (Exception e) { return e.getClass().getSimpleName(); }
        });
        t("file_uri_ctor_ascii", () -> {
            try { return u(new File(new URI("file:///d/p.txt")).getPath()); }
            catch (Exception e) { return e.getClass().getSimpleName(); }
        });

        System.out.println("S done = 1");
    }
}
