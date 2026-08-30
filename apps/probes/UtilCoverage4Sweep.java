import java.io.*;
import java.util.*;

/** L3 coverage sweep, round 4 — the families no round reached, and one round-3
 *  bug that explains why.
 *
 *  Rounds 1-3 took the corpus from 445 owning registrations to 546 of 591. The
 *  45 left are not a long tail of singletons: they are five CLUSTERS, and four
 *  of them were never asked at all.
 *
 *    java.util.Date, deprecated instance surface   19 rows
 *    serialization: writeReplace / read+writeObject 11 rows
 *    the views' own toArray(T[]) and forEach         8 rows
 *    OptionalInt / OptionalLong / OptionalDouble     7 rows
 *    Locale / TimeZone / ResourceBundle display     20 rows
 *
 *  THE VIEW CLUSTER IS A PROBE BUG, not a gap. `UtilCoverage3Sweep` asks for
 *  `toArray(T[])` like this:
 *
 *      new TreeSet<>(m.keySet()).toArray(new String[0])
 *
 *  -- which reaches `TreeSet.toArray`, never `HashMap$KeySet.toArray`. The
 *  copy was there to make the order deterministic, and it silently retargeted
 *  every row in the block. Here the view is asked DIRECTLY and the order is
 *  made deterministic by sorting the RESULT instead.
 *
 *  DETERMINISM. `Date`'s deprecated getters read the default time zone, so this
 *  pins it to UTC first and restores it at the end; without that the rows are a
 *  claim about the host. `getAvailableLocales` is asked for its shape, never its
 *  contents. Nothing prints a clock reading.
 */
public class UtilCoverage4Sweep {
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
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    static String sorted(Object[] a) {
        String[] s = new String[a.length];
        for (int i = 0; i < a.length; i++) s[i] = String.valueOf(a[i]);
        Arrays.sort(s, Comparator.nullsFirst(Comparator.naturalOrder()));
        return Arrays.toString(s);
    }

    // ---- 1. The views' OWN typed toArray and forEach. -----------------------
    static void views(String tag, Map<String, Integer> m) {
        p(tag + " ks toArray typed", sorted(m.keySet().toArray(new String[0])));
        p(tag + " ks toArray typed big", m.keySet().toArray(new String[6]).length);
        p(tag + " ks toArray typed big tail", m.keySet().toArray(new String[6])[5]);
        p(tag + " ks toArray typed exact", sorted(m.keySet().toArray(new String[3])));
        p(tag + " vs toArray typed", sorted(m.values().toArray(new Integer[0])));
        p(tag + " es toArray typed len", m.entrySet().toArray(new Map.Entry[0]).length);

        // forEach on each view, accumulated into a sorted bag so a hash order
        // cannot make the row a coin flip.
        List<String> ks = new ArrayList<>();
        m.keySet().forEach(k -> ks.add(k));
        Collections.sort(ks);
        p(tag + " ks forEach", ks.toString());

        List<String> vs = new ArrayList<>();
        m.values().forEach(v -> vs.add(String.valueOf(v)));
        Collections.sort(vs);
        p(tag + " vs forEach", vs.toString());

        List<String> es = new ArrayList<>();
        m.entrySet().forEach(e -> es.add(e.getKey() + "=" + e.getValue()));
        Collections.sort(es);
        p(tag + " es forEach", es.toString());

        List<String> me = new ArrayList<>();
        m.forEach((k, v) -> me.add(k + "=" + v));
        Collections.sort(me);
        p(tag + " map forEach", me.toString());

        // The INTERFACE-typed receiver, which is a different dispatch door.
        Map<String, Integer> asMap = m;
        List<String> mi = new ArrayList<>();
        asMap.forEach((k, v) -> mi.add(k + "=" + v));
        Collections.sort(mi);
        p(tag + " Map-typed forEach", mi.toString());

        Collection<String> asColl = m.keySet();
        p(tag + " Collection.stream count", asColl.stream().count());
        p(tag + " Collection.toArray(IntFunction)", sorted(asColl.toArray(String[]::new)));
        p(tag + " AbstractCollection.toArray", sorted(m.values().toArray()));
        p(tag + " keySet hashCode == sum", asColl.hashCode() == sum(asColl));
    }

    static int sum(Collection<String> c) {
        int h = 0;
        for (String s : c) h += s == null ? 0 : s.hashCode();
        return h;
    }

    static Map<String, Integer> seed(Map<String, Integer> m) {
        m.put("a", 1); m.put("b", 2); m.put("c", 3);
        return m;
    }

    // ---- 2. Serialization, which is where writeReplace lives. ---------------
    static byte[] ser(Object o) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        try (ObjectOutputStream os = new ObjectOutputStream(b)) { os.writeObject(o); }
        return b.toByteArray();
    }

    static Object deser(byte[] b) throws Exception {
        try (ObjectInputStream is = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return is.readObject();
        }
    }

    /** Round trip and report the CONTENT and the class. A round trip that
     *  silently loses elements reads as a pass if only the class is printed. */
    static void trip(String tag, Object o) {
        try {
            Object back = deser(ser(o));
            String content = back instanceof Map
                    ? new TreeMap<>((Map<?, ?>) back).toString()
                    : back instanceof Collection
                            ? sorted(((Collection<?>) back).toArray())
                            : String.valueOf(back);
            p(tag, back.getClass().getName() + " " + content);
        } catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        TimeZone savedZone = TimeZone.getDefault();
        Locale savedLocale = Locale.getDefault();
        TimeZone.setDefault(TimeZone.getTimeZone("UTC"));

        views("hm", seed(new HashMap<>()));
        views("lhm", seed(new LinkedHashMap<>()));
        views("ht", seed(new Hashtable<>()));
        p("hs toArray typed", sorted(new HashSet<>(List.of("a", "b")).toArray(new String[0])));
        p("AbstractSet hashCode", new HashSet<>(List.of("a", "b")).hashCode()
                == ("a".hashCode() + "b".hashCode()));

        // ---- serialization
        trip("ser List.of(1)", List.of("a"));
        trip("ser List.of(3)", List.of("a", "b", "c"));
        trip("ser Set.of(1)", Set.of("a"));
        trip("ser Set.of(3)", Set.of("a", "b", "c"));
        trip("ser Map.of(1)", Map.of("a", 1));
        trip("ser Map.of(3)", Map.of("a", 1, "b", 2, "c", 3));
        trip("ser unmodifiableList", Collections.unmodifiableList(new ArrayList<>(List.of("a", "b"))));
        trip("ser HashMap", seed(new HashMap<>()));
        trip("ser LinkedHashMap", seed(new LinkedHashMap<>()));
        trip("ser TreeSet", new TreeSet<>(List.of("b", "a", "c")));
        trip("ser TreeMap", new TreeMap<>(seed(new HashMap<>())));
        trip("ser ArrayList", new ArrayList<>(List.of("a", "b")));
        trip("ser Arrays.asList", Arrays.asList("a", "b"));

        // ---- the primitive Optionals
        OptionalInt oi = OptionalInt.of(7);
        OptionalInt oie = OptionalInt.empty();
        p("OptionalInt isPresent", oi.isPresent());
        p("OptionalInt empty isPresent", oie.isPresent());
        p("OptionalInt orElse", oi.orElse(9));
        p("OptionalInt empty orElse", oie.orElse(9));
        StringBuilder ib = new StringBuilder();
        oi.ifPresent(v -> ib.append(v));
        oie.ifPresent(v -> ib.append("NO"));
        p("OptionalInt ifPresent", ib.toString());

        OptionalLong ol = OptionalLong.of(7L);
        p("OptionalLong isPresent", ol.isPresent());
        p("OptionalLong orElse", ol.orElse(9L));
        p("OptionalLong empty orElse", OptionalLong.empty().orElse(9L));
        StringBuilder lb = new StringBuilder();
        ol.ifPresent(v -> lb.append(v));
        p("OptionalLong ifPresent", lb.toString());

        OptionalDouble od = OptionalDouble.of(1.5);
        p("OptionalDouble isPresent", od.isPresent());
        p("OptionalDouble orElse", od.orElse(9.5));
        p("OptionalDouble empty orElse", OptionalDouble.empty().orElse(9.5));
        StringBuilder db = new StringBuilder();
        od.ifPresent(v -> db.append(v));
        p("OptionalDouble ifPresent", db.toString());

        // ---- java.util.Date, the deprecated surface, pinned to UTC.
        // 1000000000000L is 2001-09-09T01:46:40Z: a fixed instant, so every
        // getter below is a constant.
        Date d = new Date(1000000000000L);
        p("Date getYear", d.getYear());
        p("Date getMonth", d.getMonth());
        p("Date getDate", d.getDate());
        p("Date getDay", d.getDay());
        p("Date getHours", d.getHours());
        p("Date getMinutes", d.getMinutes());
        p("Date getSeconds", d.getSeconds());
        p("Date getTimezoneOffset", d.getTimezoneOffset());
        tv("Date toGMTString", () -> d.toGMTString());

        Date c1 = new Date(101, 8, 9);
        p("Date(y,m,d) getTime", c1.getTime());
        p("Date(y,m,d) toString-ish", c1.getYear() + "/" + c1.getMonth() + "/" + c1.getDate());
        Date c2 = new Date(101, 8, 9, 1, 46);
        p("Date(y,m,d,h,mi) getTime", c2.getTime());
        Date c3 = new Date(101, 8, 9, 1, 46, 40);
        p("Date(y,m,d,h,mi,s) getTime", c3.getTime());
        tv("Date(String)", () -> new Date("Sun, 09 Sep 2001 01:46:40 GMT").getTime());

        Date m = new Date(1000000000000L);
        m.setYear(102); p("Date setYear", m.getYear() + " " + m.getTime());
        m.setMonth(0); p("Date setMonth", m.getMonth() + " " + m.getTime());
        m.setDate(15); p("Date setDate", m.getDate() + " " + m.getTime());
        m.setHours(3); p("Date setHours", m.getHours() + " " + m.getTime());
        m.setMinutes(4); p("Date setMinutes", m.getMinutes() + " " + m.getTime());
        m.setSeconds(5); p("Date setSeconds", m.getSeconds() + " " + m.getTime());

        // ---- TimeZone: the offset pair and the no-arg display name.
        TimeZone ny = TimeZone.getTimeZone("America/New_York");
        p("TZ getOffset(J) winter", ny.getOffset(1000000000000L));
        p("TZ getOffset(J) epoch", ny.getOffset(0L));
        p("TZ utc getOffset(J)", TimeZone.getTimeZone("UTC").getOffset(0L));
        // `getOffsets` is package-private in TimeZone, so no application can
        // call it -- a registration nothing outside java.base can reach.
        tv("TZ getDisplayName()", () -> ny.getDisplayName());
        tv("TZ getDisplayName(Locale)", () -> ny.getDisplayName(Locale.ENGLISH));
        p("TZ default is UTC", TimeZone.getDefault().getID());

        // ---- Locale: the no-arg display family and the deprecated ctors.
        Locale.setDefault(Locale.ENGLISH);
        Locale fr = Locale.forLanguageTag("fr-CA");
        tv("Locale getDisplayName()", () -> fr.getDisplayName());
        tv("Locale getDisplayLanguage()", () -> fr.getDisplayLanguage());
        tv("Locale getDisplayCountry()", () -> fr.getDisplayCountry());
        tv("Locale(String)", () -> new Locale("de").toLanguageTag());
        tv("Locale(String,String)", () -> new Locale("de", "AT").toLanguageTag());
        tv("Locale(String,String,String)", () -> new Locale("de", "AT", "x").toLanguageTag());
        // Which variants BCP-47 can carry as subtags, and where the rest go.
        // `x` is 1 char: ill-formed, so HotSpot moves it into the private-use
        // `lvariant` sequence. `POSIX` is 5: well-formed, emitted as-is.
        for (String[] v : new String[][] {
                {"de", "AT", "x"},
                {"de", "AT", "POSIX"},
                {"de", "AT", "1234"},
                {"de", "AT", "123"},
                {"de", "AT", "abcdefgh"},
                {"de", "AT", "abcdefghi"},
                {"de", "AT", "POSIX_WIN"},
                {"de", "AT", "POSIX_x"},
                {"de", "AT", "x_POSIX"},
                {"de", "", "x"},
                {"de", "AT", ""},
        }) {
            final String[] f = v;
            tv("Locale variant [" + f[0] + "/" + f[1] + "/" + f[2] + "]",
                    () -> new Locale(f[0], f[1], f[2]).toLanguageTag());
            tv("Locale getVariant [" + f[0] + "/" + f[1] + "/" + f[2] + "]",
                    () -> new Locale(f[0], f[1], f[2]).getVariant());
            tv("Locale toString [" + f[0] + "/" + f[1] + "/" + f[2] + "]",
                    () -> new Locale(f[0], f[1], f[2]).toString());
        }
        // And the round trip, which is the property that actually matters.
        tv("Locale variant round trip", () ->
                Locale.forLanguageTag(new Locale("de", "AT", "x").toLanguageTag()).getVariant());

        p("Locale getAvailableLocales non-empty", Locale.getAvailableLocales().length > 0);
        p("Locale getAvailableLocales has en",
                Arrays.stream(Locale.getAvailableLocales()).anyMatch(l -> "en".equals(l.getLanguage())));
        p("Locale default after setDefault", Locale.getDefault().toLanguageTag());

        // ---- ResourceBundle, whose absence is itself the observable.
        tv("RB getBundle missing", () -> ResourceBundle.getBundle("no.such.Bundle").getClass().getName());
        tv("RB getBundle missing w/ locale",
                () -> ResourceBundle.getBundle("no.such.Bundle", Locale.ENGLISH).getClass().getName());

        // ---- Properties.replaceAll and the SequencedMap polls.
        Properties pr = new Properties();
        pr.setProperty("a", "1");
        pr.setProperty("b", "2");
        tv("Properties replaceAll", () -> {
            pr.replaceAll((k, v) -> String.valueOf(v) + "!");
            return new TreeMap<>(pr).toString();
        });

        LinkedHashMap<String, Integer> sq = new LinkedHashMap<>();
        sq.put("a", 1); sq.put("b", 2); sq.put("c", 3);
        tv("SequencedMap pollFirstEntry", () -> String.valueOf(sq.pollFirstEntry()) + " " + sq);
        tv("SequencedMap pollLastEntry", () -> String.valueOf(sq.pollLastEntry()) + " " + sq);
        tv("SequencedMap pollFirst empty", () -> String.valueOf(new LinkedHashMap<>().pollFirstEntry()));

        // ---- Arrays.asList's own list class.
        List<String> al = Arrays.asList("a", "b", "c");
        p("Arrays.asList class", al.getClass().getName());
        p("Arrays.asList toArray", sorted(al.toArray()));
        tv("Arrays.asList set", () -> { al.set(0, "z"); return al.toString(); });
        tv("Arrays.asList add", () -> { al.add("q"); return "no-throw"; });

        TimeZone.setDefault(savedZone);
        Locale.setDefault(savedLocale);
        System.out.println("DONE UtilCoverage4Sweep");
    }
}
