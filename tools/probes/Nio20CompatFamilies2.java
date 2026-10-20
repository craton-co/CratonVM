import java.lang.reflect.*;
import java.util.*;

/**
 * nio20 wave 2 lane L5: conformance facts for the `--compatible` families this lane adds
 * (`jdk-array-list`, `jdk-locale`, `jdk-date`, `jdk-object-identity-bodies` with the
 * `ParameterizedTypeImpl` reifier change), plus an optional timing block for the three
 * `R16Fieldsite2Layout`-shaped rows they target. Without `-Dnio20f2.bench=1` every line is a fact
 * (no timings, no identity hashes), so the output diffs against HotSpot 25 as is.
 *
 * Run (the default TimeZone must be the same on both VMs for the `date-*` rows; UTC is simplest):
 *   javac -d out tools/probes/Nio20CompatFamilies2.java
 *   TZ=UTC java -cp out Nio20CompatFamilies2                     > hs.txt
 *   TZ=UTC cratonvm --compatible -cp out Nio20CompatFamilies2    > compat.txt
 *   TZ=UTC cratonvm --jdk-only -cp out Nio20CompatFamilies2      > strict.txt
 *   diff hs.txt compat.txt; diff hs.txt strict.txt
 * A/B one family: CRATONVM_COMPAT_ALIGN=-jdk-array-list (or -jdk-locale, -jdk-date,
 * -jdk-object-identity-bodies); the generic rows also depend on
 * CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS (default on; =0 brings the carriers back).
 * -Dnio20f2.rows=a,b selects rows; -Dnio20f2.bench=1 appends `<row> <N> ns/op` lines
 * (rows bench-sublist, bench-locale, bench-date; -Dnio20f2.iters=N, default 200000).
 */
public class Nio20CompatFamilies2 {
    static String rows = System.getProperty("nio20f2.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }

    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    interface Fact { Object get() throws Throwable; }
    static String f(Fact x) { try { return String.valueOf(x.get()); } catch (Throwable t) { return "!" + ex(t); } }

    public static void main(String[] a) throws Throwable {
        String[] all = {"sublist", "sublistcme", "listitr", "itrvector", "locale", "localedefault", "date",
                "datestale", "generic", "objidentity"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio20CompatFamilies2.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                System.out.println(r + " THREW " + ex(e.getCause()));
            }
        }
        if (Boolean.getBoolean("nio20f2.bench")) bench();
        System.out.println("done");
    }

    // ---- jdk-array-list ----------------------------------------------------------------

    static void sublist() {
        List<String> base = new ArrayList<>(List.of("a", "b", "c", "d", "e"));
        List<String> s = base.subList(1, 4);
        System.out.println("sublist-class " + s.getClass().getName() + " " + s.iterator().getClass().getName()
                + " " + s.listIterator().getClass().getName());
        System.out.println("sublist-read " + s + " " + s.size() + " " + s.get(0) + " " + s.indexOf("d")
                + " " + s.contains("e") + " " + Arrays.toString(s.toArray()) + " " + s.hashCode()
                + " " + s.equals(List.of("b", "c", "d")));
        s.set(0, "B");
        s.add("x");
        s.remove("c");
        System.out.println("sublist-write " + s + " " + base);
        List<String> n = s.subList(1, 3);
        n.clear();
        System.out.println("sublist-nested " + n + " " + s + " " + base);
        System.out.println("sublist-bounds " + f(() -> base.subList(-1, 2)) + " | " + f(() -> base.subList(0, 9))
                + " | " + f(() -> base.subList(2, 1)));
        ListIterator<String> it = s.listIterator();
        it.next();
        it.set("Z");
        it.add("y");
        System.out.println("sublist-listitr " + s + " " + base + " " + it.nextIndex() + " " + it.hasPrevious());
        System.out.println("sublist-reversed " + f(() -> s.reversed()) + " " + f(() -> s.getFirst()));
    }

    static void sublistcme() {
        List<Integer> base = new ArrayList<>(List.of(1, 2, 3, 4));
        List<Integer> s = base.subList(0, 2);
        base.add(5);
        System.out.println("sublistcme-get " + f(() -> s.get(0)) + " | " + f(() -> s.size()));
        List<Integer> l = new ArrayList<>(List.of(1, 2, 3));
        System.out.println("sublistcme-itr " + f(() -> { for (Integer x : l) if (x == 2) l.remove(x); return l; }));
    }

    static void listitr() {
        List<String> l = new ArrayList<>(List.of("a", "b", "c"));
        ListIterator<String> it = l.listIterator(1);
        System.out.println("listitr-class " + it.getClass().getName() + " " + l.iterator().getClass().getName());
        System.out.println("listitr-walk " + it.previous() + " " + it.next() + " " + it.next() + " "
                + it.nextIndex() + " " + it.previousIndex());
        it.remove();
        it.add("q");
        System.out.println("listitr-mut " + l + " " + f(() -> { it.remove(); return "no"; }));
        Iterator<String> i2 = l.iterator();
        i2.next();
        i2.remove();
        System.out.println("listitr-itr-remove " + l + " " + f(() -> { i2.remove(); return "no"; }));
        System.out.println("listitr-bounds " + f(() -> l.listIterator(9)));
    }

    /** `ArrayList$Itr` is excepted from the family: the VM mints it for Vector backings too. */
    static void itrvector() {
        Vector<String> v = new Vector<>(List.of("a", "b", "c"));
        Iterator<String> it = v.iterator();
        it.next();
        it.remove();
        System.out.println("itrvector " + v + " " + it.next());
        Stack<Integer> st = new Stack<>();
        st.push(1);
        st.push(2);
        System.out.println("itrvector-stack " + st + " " + st.pop() + " " + st);
    }

    // ---- jdk-locale --------------------------------------------------------------------

    static void locale() {
        Locale fr = new Locale("fr", "ca");
        Locale tag = Locale.forLanguageTag("de-CH-u-nu-latn");
        Locale old = new Locale("iw");
        System.out.println("locale-ctor " + fr + " " + fr.getLanguage() + " " + fr.getCountry() + " "
                + fr.toLanguageTag() + " " + old.getLanguage() + " " + old.toLanguageTag());
        System.out.println("locale-tag " + tag + " " + tag.getExtension('u') + " " + tag.getUnicodeLocaleType("nu")
                + " " + tag.stripExtensions() + " " + tag.hasExtensions());
        System.out.println("locale-eq " + fr.equals(Locale.CANADA_FRENCH) + " " + (fr.hashCode() == Locale.CANADA_FRENCH.hashCode())
                + " " + Locale.of("en", "US").equals(Locale.US) + " " + (Locale.of("en", "US") == Locale.US));
        System.out.println("locale-script " + Locale.forLanguageTag("zh-Hant-TW").getScript() + " "
                + new Locale("en", "US", "POSIX").getVariant() + " " + f(() -> Locale.ROOT.getISO3Language()));
        System.out.println("locale-builder " + new Locale.Builder().setLanguage("sr").setScript("Latn").setRegion("RS").build());
        System.out.println("locale-available " + (Locale.getAvailableLocales().length > 100));
    }

    static void localedefault() {
        Locale d = Locale.getDefault();
        System.out.println("localedefault " + d + " " + (Locale.getDefault() == d) + " "
                + (Locale.getDefault(Locale.Category.FORMAT) != null) + " " + d.equals(Locale.getDefault()));
        Locale saved = Locale.getDefault();
        Locale.setDefault(Locale.forLanguageTag("tr-TR"));
        System.out.println("localedefault-set " + Locale.getDefault() + " " + "TITLE".toLowerCase() + " "
                + Locale.getDefault(Locale.Category.DISPLAY));
        Locale.setDefault(saved);
        System.out.println("localedefault-restored " + (Locale.getDefault() == saved) + " " + "TITLE".toLowerCase());
    }

    // ---- jdk-date ----------------------------------------------------------------------

    @SuppressWarnings("deprecation")
    static void date() {
        Date d = new Date(99, 11, 31, 23, 59, 58);
        System.out.println("date-fields " + d.getYear() + " " + d.getMonth() + " " + d.getDate() + " " + d.getHours()
                + " " + d.getMinutes() + " " + d.getSeconds() + " " + d.getDay() + " " + d.getTime());
        d.setMonth(13);
        d.setDate(0);
        System.out.println("date-normalise " + d.getYear() + " " + d.getMonth() + " " + d.getDate() + " " + d.getTime());
        System.out.println("date-gmt " + new Date(0L).toGMTString() + " | " + new Date(1234567890123L).toGMTString());
        System.out.println("date-parse " + f(() -> new Date("Sat, 12 Aug 1995 13:30:00 GMT").getTime()));
    }

    /** A native setter under a JDK-normalised `cdate`: HotSpot prints the new date. */
    @SuppressWarnings("deprecation")
    static void datestale() {
        Date d = new Date(0L);
        String before = d.toString();
        d.setYear(120);
        d.setHours(5);
        System.out.println("datestale " + before.length() + " " + d.getYear() + " " + d.getHours() + " " + d.toString()
                + " " + d.getTime());
    }

    // ---- jdk-object-identity-bodies + the ParameterizedTypeImpl reifier ------------------

    static class Holder<T> {
        public List<? extends Number> wild;
        public Map<String, List<Integer>> nested;
        public List<String>[] arr;
        public T tv;
    }

    static void generic() throws Exception {
        for (String name : new String[] {"wild", "nested", "arr", "tv"}) {
            Field fld = Holder.class.getField(name);
            Type t = fld.getGenericType();
            Type again = Holder.class.getField(name).getGenericType();
            String args = t instanceof ParameterizedType p ? Arrays.toString(p.getActualTypeArguments()) : "-";
            String argClasses = t instanceof ParameterizedType p
                    ? Arrays.stream(p.getActualTypeArguments()).map(x -> x.getClass().getName()).toList().toString() : "-";
            System.out.println("generic-" + name + " " + t + " " + t.getTypeName() + " " + t.getClass().getName()
                    + " " + t.equals(again) + " " + (t.hashCode() == again.hashCode()) + " " + args + " " + argClasses);
        }
    }

    static void objidentity() {
        Object o = new Object();
        Object[] arr = new Object[0];
        Runnable lam = () -> {};
        System.out.println("objidentity " + o.equals(o) + " " + o.equals(new Object()) + " " + o.equals(null) + " "
                + o.toString().startsWith("java.lang.Object@") + " " + arr.toString().startsWith("[Ljava.lang.Object;@")
                + " " + lam.toString().contains("$$Lambda") + " "
                + new Object() { }.toString().startsWith("Nio20CompatFamilies2$")
                + " " + (o.toString().equals("java.lang.Object@" + Integer.toHexString(o.hashCode()))));
    }

    // ---- optional timings ----------------------------------------------------------------

    static volatile Object sink;

    static void bench() {
        int iters = Integer.getInteger("nio20f2.iters", 200_000);
        for (int round = 0; round < 3; round++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                ArrayList<Integer> l = new ArrayList<>(16);
                for (int k = 0; k < 16; k++) l.add(k);
                List<Integer> s = l.subList(2, 12);
                int acc = 0;
                for (int k = 0; k < s.size(); k++) acc += s.get(k);
                for (Integer x : s) acc += x;
                sink = s.toArray();
                sink = acc;
            }
            long t1 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                Locale loc = Locale.forLanguageTag(i % 2 == 0 ? "en-US" : "fr-CA");
                sink = loc.getLanguage() + loc.getCountry() + loc.toLanguageTag() + loc;
                sink = Locale.getDefault();
            }
            long t2 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                @SuppressWarnings("deprecation")
                int y = new Date(1_000_000_000_000L + i).getYear();
                sink = y + new Date(i).getTime();
            }
            long t3 = System.nanoTime();
            if (round == 2) {
                System.out.println("bench-sublist " + (t1 - t0) / iters + " ns/op");
                System.out.println("bench-locale " + (t2 - t1) / iters + " ns/op");
                System.out.println("bench-date " + (t3 - t2) / iters + " ns/op");
            }
        }
    }
}
