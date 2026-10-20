// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.net.URI;
import java.net.URL;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

/**
 * SBR wave 1, lane {@code uri}: the retired {@code java/net/URI} surface, and
 * the producers/readers around it that the retirement depends on.
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value -- a native
 * that wins from the first call shows here), then re-evaluated inside a hot
 * loop and compared with that first value; {@code drift} counts mismatches, so
 * cold == warm is asserted rather than assumed. Diff the whole output against
 * HotSpot 25.
 *
 * <pre>
 *   java SbrW1Uri                       (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW1Uri
 * </pre>
 */
public class SbrW1Uri {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    static void row(String label, Supplier<String> s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    interface U {
        URI get() throws Exception;
    }

    static URI u(U f) {
        try {
            return f.get();
        } catch (Exception e) {
            throw new RuntimeException(e.getClass().getName() + ": " + e.getMessage());
        }
    }

    static String all(URI x) {
        return units(allRaw(x));
    }

    static String allRaw(URI x) {
        return "str=" + x + " scheme=" + x.getScheme() + " ssp=" + x.getSchemeSpecificPart()
                + " rawSsp=" + x.getRawSchemeSpecificPart() + " auth=" + x.getAuthority()
                + " rawAuth=" + x.getRawAuthority() + " ui=" + x.getUserInfo()
                + " rawUi=" + x.getRawUserInfo() + " host=" + x.getHost() + " port=" + x.getPort()
                + " path=" + x.getPath() + " rawPath=" + x.getRawPath() + " q=" + x.getQuery()
                + " rawQ=" + x.getRawQuery() + " frag=" + x.getFragment()
                + " rawFrag=" + x.getRawFragment() + " abs=" + x.isAbsolute()
                + " opaque=" + x.isOpaque() + " hash=" + x.hashCode()
                + " ascii=" + x.toASCIIString();
    }

    static String units(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }

    public static void main(String[] args) throws Exception {
        String[] texts = {
            "http://user:pw@example.com:8080/a/b/../c?q=1&r=%20#frag",
            "HTTP://Example.COM/a%2Fb",
            "https://[::1]:443/x",
            "https://[fe80::1%25eth0]/y",
            "mailto:someone@example.com",
            "urn:isbn:0451450523",
            "file:///tmp/sbr%20uri/a%20b.txt",
            "file:/C:/dir/x.jar",
            "jar:file:/tmp/app.jar!/BOOT-INF/classes/",
            "jar:nested:/tmp/app.jar/!BOOT-INF/lib/x.jar",
            "/redirect:account?q=1",
            "docProps/core.xml",
            "../WEB-INF/test.tld",
            "",
            "#only-frag",
            "?q",
            "//host.only",
            "http://h_x/p",
            "http://h:+80/p",
            "http://h:x/a",
            "a b",
            "http://h/a%zz",
            "s://h/%E2%82%AC",
        };
        for (String t : texts) {
            row("new[" + t + "]", () -> all(u(() -> new URI(t))));
            row("create[" + t + "]", () -> all(URI.create(t)));
            row("parseServer[" + t + "]", () -> all(u(() -> new URI(t).parseServerAuthority())));
            row("normalize[" + t + "]", () -> all(u(() -> new URI(t).normalize())));
            row("toURL[" + t + "]", () -> {
                try {
                    return new URI(t).toURL().toString();
                } catch (Exception e) {
                    return "!" + e.getClass().getName() + ": " + e.getMessage();
                }
            });
        }
        row("create(null)", () -> String.valueOf(URI.create(null)));
        row("new(null)", () -> String.valueOf(u(() -> new URI(null))));

        // Multi-argument constructors (always real bytecode) meeting the
        // retired accessors.
        row("ctor5", () -> all(u(() -> new URI("http", "u i", "h.example", 81, "/a b", "q=" + (char) 0xE4, "f g"))));
        row("ctor4", () -> all(u(() -> new URI("http", "h.example", "/p q", "fr"))));
        row("ctor3", () -> all(u(() -> new URI("mailto", "a@b c", "x"))));
        row("ctor5b", () -> all(u(() -> new URI("s", "auth:9", "/p", "q", "f"))));

        URI base = URI.create("http://a/b/c/d;p?q");
        String[] refs = {"g:h", "g", "./g", "g/", "/g", "//g", "?y", "g?y", "#s", "g#s",
            "g?y#s", ";x", "g;x", "", ".", "./", "..", "../", "../g", "../..", "../../",
            "../../g", "../../../g", "/./g", "/../g", "g.", ".g", "g..", "..g",
            "./../g", "./g/.", "g/./h", "g/../h", "g;x=1/./y", "g;x=1/../y"};
        for (String r : refs) {
            row("resolveS[" + r + "]", () -> all(base.resolve(r)));
            row("resolveU[" + r + "]", () -> all(base.resolve(URI.create(r))));
        }
        row("resolve(null)", () -> String.valueOf(base.resolve((URI) null)));
        row("relativize1", () -> all(URI.create("http://h/a/b/").relativize(URI.create("http://h/a/b/c/d?x#y"))));
        row("relativize2", () -> all(URI.create("http://h/a/b").relativize(URI.create("http://h/a/bc"))));
        row("relativize3", () -> all(URI.create("http://h/a/").relativize(URI.create("http://H/a/x"))));
        row("relativize4", () -> all(URI.create("mailto:a").relativize(URI.create("mailto:a/b"))));
        row("relativize5", () -> all(URI.create("/a/").relativize(URI.create("/a/"))));

        // equals / hashCode / compareTo, including the case-folded pieces.
        String[][] pairs = {
            {"http://Example.com/a", "HTTP://example.COM/a"},
            {"http://h/a%2f", "http://h/a%2F"},
            {"http://h/A", "http://h/a"},
            {"mailto:A", "mailto:a"},
            {"http://h:80/", "http://h/"},
            {"a/b", "a/b#"},
            {"x:y", "x:y"},
            {"http://u@h/", "http://U@h/"},
        };
        for (String[] p : pairs) {
            row("eq[" + p[0] + "|" + p[1] + "]", () -> {
                URI a = URI.create(p[0]), b = URI.create(p[1]);
                return a.equals(b) + " " + (a.hashCode() == b.hashCode()) + " "
                        + Integer.signum(a.compareTo(b)) + " " + Integer.signum(b.compareTo(a));
            });
        }
        row("eq(null)", () -> String.valueOf(URI.create("x:y").equals(null)));
        row("eq(str)", () -> String.valueOf(URI.create("x:y").equals("x:y")));
        row("cmp(null)", () -> String.valueOf(URI.create("x:y").compareTo(null)));
        row("cmpObj", () -> {
            Comparable<Object> c = (Comparable<Object>) (Comparable<?>) URI.create("a:b");
            return String.valueOf(Integer.signum(c.compareTo(URI.create("a:c"))));
        });

        // Lone surrogate through every producer door (RJdkBridge1's `surrog`).
        String lone = "a" + (char) 0xD800 + "b";
        row("lone.new", () -> units(u(() -> new URI("http://h.example/" + lone)).getPath()));
        row("lone.ctor5", () -> units(u(() -> new URI("http", "h.example", "/" + lone, lone, lone)).getPath()));
        row("lone.urlToUri", () -> units(u(() -> new URL("http://h.example/" + lone).toURI()).getPath()));
        row("lone.relativize", () -> units(URI.create("http://h/a/").relativize(
                u(() -> new URI("http", "h", "/a/" + lone, null))).getPath()));

        // Native PRODUCERS still in front of real readers.
        row("path.toUri", () -> all(Path.of("/tmp/sbr uri/a#b?.txt").toUri()));
        row("path.toUri.dir", () -> all(Path.of("/tmp").toUri()));
        row("file.toURI", () -> all(new File("/tmp/sbr uri/x y").toURI()));
        row("url.toURI.http", () -> all(u(() -> new URL("http://u@h.example:81/a/b?q#f").toURI())));
        row("url.toURI.file", () -> all(u(() -> new URL("file:/tmp/x.jar").toURI())));
        row("url.toURI.jar", () -> all(u(() -> new URL("jar:file:/tmp/x.jar!/a/b").toURI())));
        row("url.toURI.bad", () -> {
            try {
                return all(new URL("http://h/a b").toURI());
            } catch (Exception e) {
                return "!" + e.getClass().getName() + ": " + e.getMessage();
            }
        });

        // Native READERS of a URI real bytecode assembled (string cache null).
        row("pathOf.resolved", () -> Path.of(URI.create("file:///tmp/a/b/").resolve("../c.txt")).toString());
        row("pathOf.normalized", () -> Path.of(URI.create("file:///tmp/a/./b/../c").normalize()).toString());
        row("fileOf.resolved", () -> new File(URI.create("file:///tmp/a/").resolve("x y")).getPath());
        row("pathOf.ctor4", () -> Path.of(u(() -> new URI("file", null, "/tmp/p q", null))).toString());
        row("roundTrip", () -> all(Path.of(URI.create("file:///tmp/z/").resolve("w")).toUri()));

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + units(first[i]));
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + units(v));
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
