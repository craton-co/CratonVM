import java.net.URI;
import java.util.*;

/** URI, Locale, Optional, Date, TreeSet, HashMap, Hashtable, ArrayList ---
 *  ~193 more rows off the bridge-kind retirement surface, diffed against
 *  HotSpot.
 *
 *  Determinism: the default time zone and locale are PINNED before anything is
 *  read, because Date's component getters and Locale's display names are
 *  functions of both, and this host's defaults are not a property either VM
 *  should be judged on. Hash-ordered containers are sorted before printing.
 *  Values are hex-escaped so the diff cannot depend on either VM's stdout
 *  encoding. */
public class UriLocaleSweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Exception; }

    static void uris() {
        String[] xs = {
            "http://host/path?q=1#frag", "https://user:pw@host:8443/a/b?x=y",
            "file:///C:/tmp/f.txt", "mailto:a@b.c", "urn:isbn:0451450523",
            "//host/path", "/abs/path", "rel/path", "?onlyquery", "#onlyfrag",
            "http://host", "http://host/", "http://h/a%20b", "a/b/../c",
        };
        for (String s : xs) {
            String k = "[" + s + "]";
            URI u;
            try { u = new URI(s); } catch (Exception e) { p(k + " CTOR", "THREW " + e.getClass().getName()); continue; }
            p(k + " scheme", u.getScheme());
            p(k + " authority", u.getAuthority());
            p(k + " userInfo", u.getUserInfo());
            p(k + " host", u.getHost());
            p(k + " port", u.getPort());
            p(k + " path", u.getPath());
            p(k + " query", u.getQuery());
            p(k + " fragment", u.getFragment());
            p(k + " ssp", u.getSchemeSpecificPart());
            p(k + " isAbsolute", u.isAbsolute());
            p(k + " isOpaque", u.isOpaque());
            p(k + " normalize", u.normalize());
            p(k + " toString", u.toString());
            p(k + " toASCIIString", u.toASCIIString());
            p(k + " hashCode==self", u.hashCode() == u.hashCode());
            p(k + " equals self", u.equals(u));
            p(k + " resolve(x)", u.isOpaque() ? "n/a" : String.valueOf(u.resolve("x")));
        }
        p("URI.create", URI.create("http://h/p"));
        p("relativize", URI.create("http://h/a/").relativize(URI.create("http://h/a/b")));
        p("resolve absolute", URI.create("http://h/a/b").resolve("/c"));
        p("compareTo", Integer.signum(URI.create("http://a").compareTo(URI.create("http://b"))));
        t("URI bad syntax", () -> new URI("http://host/ space"));
        t("URI.create bad", () -> URI.create(":::"));
        t("getHost on opaque", () -> { if (URI.create("mailto:a@b").getHost() != null)
                                           throw new IllegalStateException("host"); });
    }

    static void locales() {
        Locale[] ls = { Locale.ROOT, Locale.US, Locale.UK, Locale.FRANCE, Locale.JAPAN,
                        Locale.forLanguageTag("de-DE"), Locale.forLanguageTag("zh-Hans-CN") };
        for (Locale l : ls) {
            String k = "[" + l.toLanguageTag() + "]";
            p(k + " getLanguage", l.getLanguage());
            p(k + " getCountry", l.getCountry());
            p(k + " getVariant", l.getVariant());
            p(k + " getScript", l.getScript());
            p(k + " toString", l.toString());
            p(k + " toLanguageTag", l.toLanguageTag());
            p(k + " getISO3Language", safeIso(l, true));
            p(k + " getISO3Country", safeIso(l, false));
            p(k + " displayLanguage(ROOT)", l.getDisplayLanguage(Locale.ROOT));
            p(k + " displayCountry(ROOT)", l.getDisplayCountry(Locale.ROOT));
            p(k + " displayName(ROOT)", l.getDisplayName(Locale.ROOT));
            p(k + " equals self", l.equals(l));
        }
        p("Locale.getDefault pinned", Locale.getDefault());
        p("forLanguageTag garbage", Locale.forLanguageTag("!!!"));
        p("Locale.ROOT isEmpty lang", Locale.ROOT.getLanguage().isEmpty());
    }
    static String safeIso(Locale l, boolean lang) {
        try { return lang ? l.getISO3Language() : l.getISO3Country(); }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    static void optionals() {
        Optional<String> some = Optional.of("v"), none = Optional.empty();
        p("isPresent", some.isPresent() + "/" + none.isPresent());
        p("isEmpty", some.isEmpty() + "/" + none.isEmpty());
        p("get", some.get());
        p("orElse", none.orElse("d") + "/" + some.orElse("d"));
        p("orElseGet", none.orElseGet(() -> "g"));
        p("map", some.map(String::toUpperCase));
        p("map on empty", none.map(String::toUpperCase));
        p("filter keep", some.filter(s -> s.equals("v")));
        p("filter drop", some.filter(s -> s.equals("z")));
        p("flatMap", some.flatMap(s -> Optional.of(s + "!")));
        p("or", none.or(() -> Optional.of("alt")));
        p("stream count", some.stream().count() + "/" + none.stream().count());
        p("toString", some + "/" + none);
        p("equals", some.equals(Optional.of("v")) + "/" + none.equals(Optional.empty()));
        p("hashCode equal", some.hashCode() == Optional.of("v").hashCode());
        p("ofNullable null", Optional.ofNullable(null));
        p("OptionalInt", OptionalInt.of(3) + "/" + OptionalInt.empty());
        p("OptionalLong", OptionalLong.of(3L).getAsLong());
        p("OptionalDouble", OptionalDouble.of(1.5).getAsDouble());
        t("get on empty", () -> none.get());
        t("orElseThrow on empty", () -> none.orElseThrow());
        t("Optional.of(null)", () -> Optional.of(null));
    }

    static void dates() {
        Date d = new Date(1_000_000_000_000L);
        p("getTime", d.getTime());
        p("toString(UTC-pinned)", d.toString());
        p("toInstant", d.toInstant());
        p("before/after", d.before(new Date(2_000_000_000_000L)) + "/" + d.after(new Date(0)));
        p("compareTo", Integer.signum(d.compareTo(new Date(0))));
        p("equals same millis", d.equals(new Date(1_000_000_000_000L)));
        p("hashCode", d.hashCode());
        Date c = (Date) d.clone();
        c.setTime(42L);
        p("clone independent", d.getTime() + "/" + c.getTime());
        p("Date.from(Instant)", Date.from(java.time.Instant.ofEpochMilli(5L)).getTime());
    }

    static void containers() {
        TreeSet<String> ts = new TreeSet<>(Arrays.asList("d", "a", "c", "b"));
        p("ts toString", ts);
        p("ts first/last", ts.first() + "/" + ts.last());
        p("ts headSet/tailSet", ts.headSet("c") + "/" + ts.tailSet("c"));
        p("ts subSet", ts.subSet("a", "d"));
        p("ts ceiling/floor", ts.ceiling("bb") + "/" + ts.floor("bb"));
        p("ts higher/lower", ts.higher("b") + "/" + ts.lower("b"));
        p("ts descendingSet", ts.descendingSet());
        p("ts pollFirst/pollLast", ts.pollFirst() + "/" + ts.pollLast());
        p("ts after polls", ts);
        t("ts null add", () -> ts.add(null));
        t("ts first on empty", () -> new TreeSet<String>().first());

        HashMap<String, Integer> hm = new HashMap<>();
        for (int i = 0; i < 5; i++) hm.put("k" + i, i);
        p("hm size", hm.size());
        p("hm sorted", new TreeMap<>(hm));
        p("hm get/getOrDefault", hm.get("k1") + "/" + hm.getOrDefault("z", -1));
        p("hm putIfAbsent", hm.putIfAbsent("k1", 99) + "/" + hm.putIfAbsent("k9", 9));
        p("hm remove", hm.remove("k0"));
        p("hm merge", hm.merge("k1", 10, Integer::sum));
        p("hm compute", hm.compute("k2", (k, v) -> v == null ? 0 : v * 3));
        p("hm null key allowed", hm.put(null, -1) + "/" + hm.get(null));
        p("hm containsKey null", hm.containsKey(null));
        p("hm keySet sorted-nonnull", new TreeSet<>(nonNull(hm.keySet())));
        p("hm equals copy", hm.equals(new HashMap<>(hm)));
        p("hm hashCode equal", hm.hashCode() == new HashMap<>(hm).hashCode());

        Hashtable<String, Integer> ht = new Hashtable<>();
        ht.put("a", 1); ht.put("b", 2);
        p("ht sorted", new TreeMap<>(ht));
        p("ht get", ht.get("a"));
        p("ht contains(value)", ht.contains(2));
        p("ht containsKey", ht.containsKey("b"));
        p("ht keys sorted", new TreeSet<>(Collections.list(ht.keys())));
        t("ht null key", () -> ht.put(null, 1));
        t("ht null value", () -> ht.put("c", null));

        ArrayList<String> al = new ArrayList<>(Arrays.asList("a", "b", "c"));
        al.add(1, "ins");
        p("al toString", al);
        p("al indexOf/lastIndexOf", al.indexOf("b") + "/" + al.lastIndexOf("c"));
        p("al subList", al.subList(1, 3));
        p("al removeIf", al.removeIf(s -> s.equals("ins")) + " -> " + al);
        p("al set", al.set(0, "Z") + " -> " + al);
        p("al toArray", Arrays.toString(al.toArray()));
        p("al equals LinkedList", al.equals(new LinkedList<>(al)));
        t("al get oob", () -> al.get(99));
        t("al add oob index", () -> al.add(99, "x"));
    }
    static List<String> nonNull(Collection<String> c) {
        List<String> l = new ArrayList<>();
        for (String s : c) if (s != null) l.add(s);
        return l;
    }

    public static void main(String[] a) {
        TimeZone.setDefault(TimeZone.getTimeZone("UTC"));
        Locale.setDefault(Locale.US);
        uris();
        locales();
        optionals();
        dates();
        containers();
        System.out.println("DONE UriLocaleSweep");
    }
}
