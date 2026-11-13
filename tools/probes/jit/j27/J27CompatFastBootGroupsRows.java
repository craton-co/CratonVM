// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 9 (lane W9RPL): the natives `--compatible` promotes to Intrinsic through
// native-api `retired_shadow::is_compat_fast_boot_intrinsic`. Group 2 is
// `Method`/`Constructor.getParameterAnnotations`. Group 4 is `ResourceBundle`, `LocaleResources`
// and `Bundles`. Page: docs/feature-designs/native-dispatch-architecture.md.
//
// Every line is deterministic: the default locale is pinned to en-US before anything else runs.
//   javac -d <out> J27CompatFastBootGroupsRows.java
//   java -cp <out> J27CompatFastBootGroupsRows > hs.txt                    # HotSpot 25
//   cratonvm {--real-jdk,--compatible} -cp <out> J27CompatFastBootGroupsRows | diff - hs.txt
// Expected: identical output in both modes (HotSpot's lines are the reference).
import java.lang.annotation.*;
import java.lang.reflect.*;
import java.text.BreakIterator;
import java.text.DateFormat;
import java.text.DateFormatSymbols;
import java.text.DecimalFormat;
import java.text.DecimalFormatSymbols;
import java.text.NumberFormat;
import java.text.SimpleDateFormat;
import java.time.format.*;
import java.util.*;

public class J27CompatFastBootGroupsRows {
    @Retention(RetentionPolicy.RUNTIME) @Target(ElementType.PARAMETER) @interface Vis { String value() default "v"; }
    @Retention(RetentionPolicy.CLASS) @Target(ElementType.PARAMETER) @interface Invis { }
    @Retention(RetentionPolicy.RUNTIME) @Target(ElementType.PARAMETER) @Repeatable(Reps.class) @interface Rep { int value(); }
    @Retention(RetentionPolicy.RUNTIME) @Target(ElementType.PARAMETER) @interface Reps { Rep[] value(); }

    static void fact(String k, Object v) { System.out.println("fact " + k + " " + v); }

    // ---- group 2: parameter annotations ----
    public void plain(@Vis("a") int a, @Invis String b, @Vis("c") @Rep(1) @Rep(2) Object c) { }
    public static void none() { }
    public static <T> void generic(@Vis("g") T t, @Vis("h") List<? extends T> l) { }

    class Inner { Inner(@Vis("inner") int x, String y) { } }
    static class Nested { Nested(@Vis("nested") int x) { } }
    enum E { A(1, "x"); E(@Vis("enum") int i, @Invis String s) { } }
    record R(@Vis("rec") int a, String b) { }
    interface Base<T> { void take(@Vis("base") T t); }
    static class Impl implements Base<String> { public void take(@Vis("impl") String s) { } }
    interface WithDefault { default void d(@Vis("dflt") int x) { } }

    static String ann(Annotation[][] a) {
        StringBuilder sb = new StringBuilder("[").append(a.length).append("]");
        for (Annotation[] row : a) {
            List<String> names = new ArrayList<>();
            for (Annotation x : row) {
                String s = x.annotationType().getSimpleName();
                if (x instanceof Vis v) s += "=" + v.value();
                if (x instanceof Reps r) {
                    StringBuilder rs = new StringBuilder();
                    for (Rep rep : r.value()) rs.append(rep.value());
                    s += "=" + rs;
                }
                names.add(s);
            }
            sb.append(names);
        }
        return sb.toString();
    }

    static String tryAnn(Executable e) {
        try {
            return ann(e.getParameterAnnotations());
        } catch (Throwable t) {
            return "THROW " + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String params(Executable e) {
        StringBuilder sb = new StringBuilder();
        for (Parameter p : e.getParameters()) {
            sb.append(p.getType().getSimpleName()).append(p.isSynthetic() ? "/syn" : "").append(p.isImplicit() ? "/impl" : "")
              .append(Arrays.toString(p.getAnnotations().length == 0 ? new String[0]
                  : Arrays.stream(p.getAnnotations()).map(x -> x.annotationType().getSimpleName()).toArray(String[]::new)))
              .append(';');
        }
        return sb.toString();
    }

    static void annotations() throws Exception {
        Class<?> c = J27CompatFastBootGroupsRows.class;
        Method plain = c.getMethod("plain", int.class, String.class, Object.class);
        fact("pa-plain", tryAnn(plain));
        fact("pa-plain-params", params(plain));
        fact("pa-none", tryAnn(c.getMethod("none")));
        Method gen = null;
        for (Method m : c.getDeclaredMethods()) if (m.getName().equals("generic")) gen = m;
        fact("pa-generic", tryAnn(gen));
        Constructor<?> inner = Inner.class.getDeclaredConstructors()[0];
        fact("pa-inner", tryAnn(inner));
        fact("pa-inner-params", params(inner));
        fact("pa-nested", tryAnn(Nested.class.getDeclaredConstructors()[0]));
        Constructor<?> ec = E.class.getDeclaredConstructors()[0];
        fact("pa-enum", tryAnn(ec));
        fact("pa-enum-params", params(ec));
        Constructor<?> rc = R.class.getDeclaredConstructors()[0];
        fact("pa-record", tryAnn(rc));
        List<String> impl = new ArrayList<>();
        for (Method m : Impl.class.getDeclaredMethods()) {
            impl.add(m.getName() + (m.isBridge() ? "/bridge" : "") + "(" + m.getParameterTypes()[0].getSimpleName() + ")" + tryAnn(m));
        }
        Collections.sort(impl);
        fact("pa-impl", impl);
        fact("pa-base", tryAnn(Base.class.getMethod("take", Object.class)));
        fact("pa-default", tryAnn(WithDefault.class.getMethod("d", int.class)));
        int base = 10;
        Object local = new Object() {
            @Override public String toString() { return "x" + base; }
        };
        Constructor<?> anon = local.getClass().getDeclaredConstructors()[0];
        fact("pa-anon", tryAnn(anon));
        Runnable r = () -> System.out.print("");
        List<String> lambdas = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            if (m.isSynthetic() && m.getName().startsWith("lambda$")) lambdas.add(m.getParameterCount() + tryAnn(m));
        }
        Collections.sort(lambdas);
        fact("pa-lambdas", lambdas);
        r.run();
        // A fresh array each call: mutating one must not leak into the next.
        Annotation[][] first = plain.getParameterAnnotations();
        first[0] = new Annotation[0];
        fact("pa-copy", first != plain.getParameterAnnotations() && plain.getParameterAnnotations()[0].length == 1);
        fact("pa-type", plain.getParameterAnnotations().getClass().getName() + " "
            + plain.getParameterAnnotations()[0].getClass().getName());
        fact("pa-jdk", tryAnn(String.class.getMethod("substring", int.class, int.class)));
        fact("pa-object-ctor", tryAnn(Object.class.getConstructor()));
    }

    // ---- group 4: ResourceBundle and the locale data behind java.text ----
    public static class Data extends ListResourceBundle {
        protected Object[][] getContents() {
            return new Object[][] { {"hello", "Hello"}, {"only.root", "root"}, {"arr", new String[] {"a", "b"}}, {"num", 7} };
        }
    }
    public static class Data_fr extends ListResourceBundle {
        protected Object[][] getContents() { return new Object[][] { {"hello", "Bonjour"} }; }
    }
    public static class Data_fr_CA extends ListResourceBundle {
        protected Object[][] getContents() { return new Object[][] { {"hello", "Allo"}, {"ca", "oui"} }; }
    }

    static final String BASE = "J27CompatFastBootGroupsRows$Data";

    static String describe(ResourceBundle b) {
        if (b == null) return "null";
        List<String> keys = new ArrayList<>(b.keySet());
        Collections.sort(keys);
        List<String> en = new ArrayList<>(Collections.list(b.getKeys()));
        Collections.sort(en);
        return b.getClass().getSimpleName() + " locale=[" + b.getLocale() + "] base=" + b.getBaseBundleName()
            + " keys=" + keys + " enum=" + en + " hello=" + b.getString("hello");
    }

    interface Call { Object run() throws Throwable; }

    static String t(Call c) {
        try {
            Object o = c.run();
            if (o instanceof Object[] a) return Arrays.toString(a);
            if (o instanceof ResourceBundle b) return describe(b);
            return String.valueOf(o);
        } catch (MissingResourceException e) {
            return "THROW MRE " + e.getMessage() + " class=" + e.getClassName() + " key=" + e.getKey();
        } catch (Throwable e) {
            return "THROW " + e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static void bundles() {
        ClassLoader cl = J27CompatFastBootGroupsRows.class.getClassLoader();
        fact("rb-root", t(() -> ResourceBundle.getBundle(BASE, Locale.ROOT)));
        fact("rb-fr", t(() -> ResourceBundle.getBundle(BASE, Locale.FRENCH)));
        fact("rb-fr-CA", t(() -> ResourceBundle.getBundle(BASE, Locale.CANADA_FRENCH)));
        fact("rb-de-fallback", t(() -> ResourceBundle.getBundle(BASE, Locale.GERMAN)));
        fact("rb-default", t(() -> ResourceBundle.getBundle(BASE)));
        fact("rb-cached", ResourceBundle.getBundle(BASE, Locale.FRENCH) == ResourceBundle.getBundle(BASE, Locale.FRENCH));
        ResourceBundle ca = ResourceBundle.getBundle(BASE, Locale.CANADA_FRENCH);
        fact("rb-inherited", t(() -> ca.getString("only.root")));
        fact("rb-object", t(() -> ca.getObject("num")));
        fact("rb-array", t(() -> ca.getStringArray("arr")));
        fact("rb-array-cce", t(() -> ca.getStringArray("hello")));
        fact("rb-string-cce", t(() -> ca.getString("num")));
        fact("rb-missing", t(() -> ca.getString("nope")));
        fact("rb-missing-obj", t(() -> ca.getObject("nope")));
        fact("rb-null-key", t(() -> ca.getString(null)));
        fact("rb-contains", ca.containsKey("only.root") + " " + ca.containsKey("ca") + " " + ca.containsKey("nope"));
        fact("rb-contains-null", t(() -> ca.containsKey(null)));
        fact("rb-handle", t(() -> ((ListResourceBundle) ca).handleGetObject("only.root")));
        fact("rb-no-bundle", t(() -> ResourceBundle.getBundle("no.such.Bundle", Locale.FRENCH)));
        fact("rb-null-base", t(() -> ResourceBundle.getBundle(null, Locale.FRENCH)));
        fact("rb-null-locale", t(() -> ResourceBundle.getBundle(BASE, (Locale) null)));
        ResourceBundle.Control noFallback = ResourceBundle.Control.getNoFallbackControl(ResourceBundle.Control.FORMAT_CLASS);
        fact("rb-ctl-nofallback-de", t(() -> ResourceBundle.getBundle(BASE, Locale.GERMAN, noFallback)));
        fact("rb-ctl-nofallback-fr", t(() -> ResourceBundle.getBundle(BASE, Locale.FRENCH, cl, noFallback)));
        ResourceBundle.Control onlyRoot = new ResourceBundle.Control() {
            @Override public List<Locale> getCandidateLocales(String b, Locale l) { return List.of(Locale.ROOT); }
        };
        fact("rb-ctl-candidates", t(() -> ResourceBundle.getBundle(BASE, Locale.CANADA_FRENCH, onlyRoot)));
        ResourceBundle.Control props = new ResourceBundle.Control() {
            @Override public List<String> getFormats(String b) { return List.of("java.properties"); }
            @Override public ResourceBundle newBundle(String b, Locale l, String f, ClassLoader c, boolean r) throws java.io.IOException {
                if (!l.equals(Locale.ROOT)) return null;
                return new PropertyResourceBundle(new java.io.StringReader("hello=Props\nx=1\n"));
            }
        };
        fact("rb-ctl-props", t(() -> ResourceBundle.getBundle("virtual.Props", Locale.ITALIAN, props)));
        fact("rb-ctl-format-default", t(() -> ResourceBundle.getBundle(BASE, Locale.FRENCH,
            ResourceBundle.Control.getControl(ResourceBundle.Control.FORMAT_DEFAULT))));
        ResourceBundle.clearCache();
        fact("rb-after-clear", t(() -> ResourceBundle.getBundle(BASE, Locale.FRENCH)));
        // A user subclass overriding handleGetObject only.
        ResourceBundle custom = new ResourceBundle() {
            @Override protected Object handleGetObject(String k) { return k.equals("k") ? "v" : null; }
            @Override public Enumeration<String> getKeys() { return Collections.enumeration(List.of("k")); }
        };
        fact("rb-custom", t(() -> custom.getString("k") + " " + custom.containsKey("k") + " " + custom.keySet()
            + " " + custom.getLocale() + " " + custom.getBaseBundleName()));
        fact("rb-custom-missing", t(() -> custom.getString("z")));
    }

    static void localeData() {
        for (Locale l : new Locale[] {Locale.US, Locale.FRANCE, Locale.GERMANY, Locale.JAPAN, Locale.ROOT, Locale.forLanguageTag("ar-EG"), Locale.forLanguageTag("hi-IN")}) {
            String tag = l.toLanguageTag();
            DecimalFormatSymbols s = DecimalFormatSymbols.getInstance(l);
            fact("ld-dfs-" + tag, s.getDecimalSeparator() + "|" + s.getGroupingSeparator() + "|" + s.getMinusSign() + "|"
                + s.getZeroDigit() + "|" + s.getInfinity() + "|" + s.getNaN() + "|" + s.getExponentSeparator() + "|" + s.getPercent());
            fact("ld-num-" + tag, ((DecimalFormat) NumberFormat.getInstance(l)).toPattern() + " | "
                + ((DecimalFormat) NumberFormat.getPercentInstance(l)).toPattern() + " | "
                + ((DecimalFormat) NumberFormat.getCurrencyInstance(l)).toPattern() + " | " + NumberFormat.getInstance(l).format(-1234567.891));
            fact("ld-date-" + tag, ((SimpleDateFormat) DateFormat.getDateTimeInstance(DateFormat.FULL, DateFormat.SHORT, l)).toPattern()
                + " | " + ((SimpleDateFormat) DateFormat.getDateInstance(DateFormat.MEDIUM, l)).toPattern());
            fact("ld-jt-" + tag, DateTimeFormatterBuilder.getLocalizedDateTimePattern(FormatStyle.LONG, FormatStyle.MEDIUM,
                java.time.chrono.IsoChronology.INSTANCE, l));
            DateFormatSymbols d = DateFormatSymbols.getInstance(l);
            fact("ld-dfsym-" + tag, d.getMonths()[0] + "|" + d.getShortWeekdays()[1] + "|" + d.getAmPmStrings()[1] + "|" + d.getEras()[1]);
            BreakIterator bi = BreakIterator.getWordInstance(l);
            bi.setText("Hello, world! It's 3.5 o'clock.");
            List<Integer> b = new ArrayList<>();
            for (int i = bi.first(); i != BreakIterator.DONE; i = bi.next()) b.add(i);
            fact("ld-break-" + tag, b);
        }
    }

    public static void main(String[] args) throws Exception {
        Locale.setDefault(Locale.US);
        annotations();
        bundles();
        localeData();
    }
}
