// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.io.FileOutputStream;
import java.io.OutputStreamWriter;
import java.io.Writer;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.text.BreakIterator;
import java.text.DateFormat;
import java.text.DateFormatSymbols;
import java.text.DecimalFormatSymbols;
import java.text.MessageFormat;
import java.text.NumberFormat;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.format.DateTimeFormatter;
import java.time.format.FormatStyle;
import java.time.format.TextStyle;
import java.time.temporal.WeekFields;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Calendar;
import java.util.Collections;
import java.util.Currency;
import java.util.Date;
import java.util.Enumeration;
import java.util.List;
import java.util.ListResourceBundle;
import java.util.Locale;
import java.util.ResourceBundle;
import java.util.TimeZone;
import java.util.TreeSet;
import java.util.logging.Level;

/**
 * SBR wave 4, lane LOCALE2: the ResourceBundle / Bundles.of / LocaleResources
 * half of the locale family wave 1 held.
 *
 * Every row goes through a real {@code ResourceBundle}: the public API over a
 * user {@code ListResourceBundle} and a {@code .properties} family, and the
 * JDK's own locale-data bundles as {@code java.text} / {@code java.time} /
 * {@code Calendar} / JUL read them through {@code LocaleResources}. Fifteen
 * locales, so a reader that only knows en_US shows up as a diff.
 *
 * Prints every row's FIRST-run value, then recomputes all rows inside a hot
 * loop and counts rows whose value changed ({@code drift}).
 *
 * Run: {@code java SbrW4Locale2 [iters]} (HotSpot oracle) vs
 * {@code cratonvm --jdk-only -cp . SbrW4Locale2 [iters]}.
 */
public class SbrW4Locale2 {

    public static class Res extends ListResourceBundle {
        @Override
        protected Object[][] getContents() {
            return new Object[][] {{"greeting", "hello"}, {"arr", new String[] {"a", "b"}}, {"only.base", "base"}};
        }
    }

    public static class Res_de extends ListResourceBundle {
        @Override
        protected Object[][] getContents() {
            return new Object[][] {{"greeting", "hallo"}};
        }
    }

    public static class Res_de_CH extends ListResourceBundle {
        @Override
        protected Object[][] getContents() {
            return new Object[][] {{"greeting", "gruezi"}};
        }
    }

    static final Locale[] LOCALES = {
        Locale.US, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN, Locale.of("ru", "RU"), Locale.of("ar", "EG"),
        Locale.of("hi", "IN"), Locale.of("th", "TH"), Locale.CHINA, Locale.of("pt", "BR"), Locale.UK,
        Locale.of("es", "MX"), Locale.of("sv", "SE"), Locale.ROOT, Locale.forLanguageTag("de-CH"),
    };

    static ClassLoader propsLoader;

    static String safe(java.util.function.Supplier<Object> s) {
        try {
            Object o = s.get();
            if (o instanceof Object[] a) {
                return Arrays.toString(a);
            }
            return String.valueOf(o);
        } catch (Throwable t) {
            String m = t.getMessage();
            return "THROW " + t.getClass().getName() + (m == null ? "" : ": " + m);
        }
    }

    static String keys(ResourceBundle b) {
        TreeSet<String> ks = new TreeSet<>();
        for (Enumeration<String> e = b.getKeys(); e.hasMoreElements();) {
            ks.add(e.nextElement());
        }
        return ks.toString();
    }

    static void localeRows(List<String> out, Locale l) {
        String t = "[" + l.toLanguageTag() + "]";
        Date when = new Date(1700000000000L);
        TimeZone utc = TimeZone.getTimeZone("UTC");
        // DecimalFormatSymbols: LocaleResources.getDecimalFormatSymbolsData.
        out.add(t + "dfs=" + safe(() -> {
            DecimalFormatSymbols d = DecimalFormatSymbols.getInstance(l);
            return "" + d.getDecimalSeparator() + d.getGroupingSeparator() + d.getMinusSign() + d.getPercent()
                    + d.getZeroDigit() + "|" + d.getExponentSeparator() + "|" + d.getInfinity() + "|" + d.getNaN()
                    + "|" + d.getPerMill() + "|" + d.getMonetaryDecimalSeparator() + "|" + d.getCurrencySymbol()
                    + "|" + d.getInternationalCurrencySymbol();
        }));
        // NumberFormat patterns: LocaleResources.getNumberPatterns.
        out.add(t + "nf=" + safe(() -> NumberFormat.getInstance(l).format(1234567.891)));
        out.add(t + "nf.cur=" + safe(() -> NumberFormat.getCurrencyInstance(l).format(-1234.5)));
        out.add(t + "nf.pct=" + safe(() -> NumberFormat.getPercentInstance(l).format(0.256)));
        out.add(t + "nf.int=" + safe(() -> NumberFormat.getIntegerInstance(l).format(-9876543.5)));
        out.add(t + "nf.compact=" + safe(() -> NumberFormat.getCompactNumberInstance(l, NumberFormat.Style.SHORT)
                .format(1234567)));
        out.add(t + "nf.parse=" + safe(() -> {
            try {
                return NumberFormat.getInstance(l).parse(NumberFormat.getInstance(l).format(-4321.25));
            } catch (java.text.ParseException e) {
                return "PARSE " + e.getErrorOffset();
            }
        }));
        out.add(t + "fmt=" + safe(() -> String.format(l, "%,.2f|%e|%tB %<tA", 1234567.891, 12345.678,
                when)));
        // DateFormat: LocaleResources.getDateTimePattern.
        for (int style : new int[] {DateFormat.FULL, DateFormat.LONG, DateFormat.MEDIUM, DateFormat.SHORT}) {
            out.add(t + "df" + style + "=" + safe(() -> {
                DateFormat df = DateFormat.getDateTimeInstance(style, style, l);
                df.setTimeZone(utc);
                return df.format(when);
            }));
        }
        out.add(t + "dfs.sym=" + safe(() -> {
            DateFormatSymbols s = DateFormatSymbols.getInstance(l);
            return s.getMonths()[0] + "|" + s.getShortMonths()[1] + "|" + s.getWeekdays()[1] + "|"
                    + s.getShortWeekdays()[2] + "|" + Arrays.toString(s.getAmPmStrings()) + "|"
                    + Arrays.toString(s.getEras());
        }));
        // Calendar week data: CalendarData (region table).
        out.add(t + "cal=" + safe(() -> {
            Calendar c = Calendar.getInstance(utc, l);
            return c.getFirstDayOfWeek() + "|" + c.getMinimalDaysInFirstWeek() + "|" + c.getCalendarType();
        }));
        out.add(t + "weekfields=" + safe(() -> WeekFields.of(l).getFirstDayOfWeek() + "|"
                + WeekFields.of(l).getMinimalDaysInFirstWeek()));
        // java.time: LocaleResources.getJavaTimeDateTimePattern.
        LocalDateTime ldt = LocalDateTime.of(2024, 3, 5, 14, 7, 9);
        for (FormatStyle fs : new FormatStyle[] {FormatStyle.FULL, FormatStyle.MEDIUM, FormatStyle.SHORT}) {
            out.add(t + "jt.date." + fs + "=" + safe(() -> DateTimeFormatter.ofLocalizedDate(fs).withLocale(l)
                    .format(ldt)));
        }
        out.add(t + "jt.dt.medium=" + safe(() -> DateTimeFormatter.ofLocalizedDateTime(FormatStyle.MEDIUM)
                .withLocale(l).format(ldt)));
        out.add(t + "jt.pattern=" + safe(() -> DateTimeFormatter.ofPattern("EEEE, d MMMM uuuu a", l).format(ldt)));
        out.add(t + "jt.month=" + safe(() -> ldt.getMonth().getDisplayName(TextStyle.FULL, l) + "|"
                + ldt.getDayOfWeek().getDisplayName(TextStyle.SHORT, l)));
        // Display names: LocaleNames / TimeZoneNames / CurrencyNames bundles.
        out.add(t + "disp=" + safe(() -> l.getDisplayName(l) + "|" + l.getDisplayLanguage(Locale.GERMAN) + "|"
                + l.getDisplayCountry(Locale.FRENCH)));
        out.add(t + "tzname=" + safe(() -> TimeZone.getTimeZone("Europe/Berlin").getDisplayName(false,
                TimeZone.LONG, l) + "|" + TimeZone.getTimeZone("America/New_York").getDisplayName(true,
                TimeZone.SHORT, l)));
        out.add(t + "cur=" + safe(() -> {
            if (l.getCountry().isEmpty()) {
                return "-";
            }
            Currency c = Currency.getInstance(l);
            return c.getCurrencyCode() + "|" + c.getSymbol(l) + "|" + c.getDisplayName(l);
        }));
        // BreakIterator: LocaleResources.getBreakIteratorInfo/Resources.
        out.add(t + "bi=" + safe(() -> {
            BreakIterator bi = BreakIterator.getWordInstance(l);
            String s = "The quick brown fox. It jumped! Over 3.5 dogs?";
            bi.setText(s);
            int n = 0;
            for (int e = bi.next(); e != BreakIterator.DONE; e = bi.next()) {
                n++;
            }
            BreakIterator sb = BreakIterator.getSentenceInstance(l);
            sb.setText(s);
            int m = 0;
            for (int e = sb.next(); e != BreakIterator.DONE; e = sb.next()) {
                m++;
            }
            return n + "|" + m;
        }));
        out.add(t + "mf=" + safe(() -> {
            MessageFormat mf = new MessageFormat("{0,number,#,##0.0} on {1,date,long} at {1,time,short}", l);
            mf.getFormats();
            for (java.text.Format f : mf.getFormatsByArgumentIndex()) {
                if (f instanceof DateFormat df) {
                    df.setTimeZone(utc);
                }
            }
            return mf.format(new Object[] {12345.67, when});
        }));
    }

    static List<String> rows() {
        List<String> out = new ArrayList<>();
        out.add("default=" + safe(() -> Locale.getDefault()));
        out.add("default.props=" + System.getProperty("user.language") + "|" + System.getProperty("user.country")
                + "|" + System.getProperty("user.region") + "|" + System.getProperty("user.variant"));
        for (Locale l : LOCALES) {
            localeRows(out, l);
        }

        // ResourceBundle public API over a user ListResourceBundle family.
        String base = "SbrW4Locale2$Res";
        out.add("rb.root=" + safe(() -> ResourceBundle.getBundle(base, Locale.ROOT).getString("greeting")));
        out.add("rb.de=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN).getString("greeting")));
        out.add("rb.deCH=" + safe(() -> ResourceBundle.getBundle(base, Locale.forLanguageTag("de-CH"))
                .getString("greeting")));
        out.add("rb.deCH.inherit=" + safe(() -> ResourceBundle.getBundle(base, Locale.forLanguageTag("de-CH"))
                .getString("only.base")));
        out.add("rb.de.locale=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN).getLocale()));
        out.add("rb.fr.locale=" + safe(() -> {
            ResourceBundle b = ResourceBundle.getBundle(base, Locale.FRENCH);
            return b.getLocale() + "|" + b.getClass().getName() + "|" + b.getString("greeting");
        }));
        out.add("rb.de.arr=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN).getStringArray("arr")));
        out.add("rb.de.obj=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN).getObject("arr")
                .getClass().getSimpleName()));
        out.add("rb.de.has=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN).containsKey("arr") + "|"
                + ResourceBundle.getBundle(base, Locale.GERMAN).containsKey("zzz")));
        out.add("rb.de.keys=" + safe(() -> keys(ResourceBundle.getBundle(base, Locale.GERMAN))));
        out.add("rb.de.keySet=" + safe(() -> new TreeSet<>(ResourceBundle.getBundle(base, Locale.GERMAN).keySet())));
        out.add("rb.de.class=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN).getClass().getName()));
        out.add("rb.de.basename=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN).getBaseBundleName()));
        out.add("rb.same=" + safe(() -> ResourceBundle.getBundle(base, Locale.GERMAN)
                == ResourceBundle.getBundle(base, Locale.GERMAN)));
        out.add("rb.missing=" + safe(() -> ResourceBundle.getBundle("no.such.Bundle")));
        out.add("rb.missingkey=" + safe(() -> ResourceBundle.getBundle(base, Locale.ROOT).getString("nope")));
        out.add("rb.nullkey=" + safe(() -> ResourceBundle.getBundle(base, Locale.ROOT).getString(null)));
        out.add("rb.nofallback=" + safe(() -> ResourceBundle.getBundle(base, Locale.JAPANESE,
                ResourceBundle.Control.getNoFallbackControl(ResourceBundle.Control.FORMAT_DEFAULT)).getLocale()));

        // .properties family through a loader: PropertyResourceBundle.
        out.add("props.en=" + safe(() -> {
            ResourceBundle b = ResourceBundle.getBundle("sbrw4.Msg", Locale.ENGLISH, propsLoader);
            return b.getClass().getName() + "|" + b.getString("k") + "|" + b.getString("only") + "|" + b.getLocale();
        }));
        out.add("props.de=" + safe(() -> {
            ResourceBundle b = ResourceBundle.getBundle("sbrw4.Msg", Locale.GERMANY, propsLoader);
            return b.getClass().getName() + "|" + b.getString("k") + "|" + b.getString("only") + "|" + b.getLocale()
                    + "|" + keys(b) + "|" + new TreeSet<>(b.keySet());
        }));
        out.add("props.ctl=" + safe(() -> {
            ResourceBundle b = ResourceBundle.getBundle("sbrw4.Msg", Locale.GERMANY, propsLoader,
                    ResourceBundle.Control.getNoFallbackControl(ResourceBundle.Control.FORMAT_PROPERTIES));
            return b.getString("k") + "|" + b.getLocale();
        }));

        // JDK-internal bundles through the public API and through JUL.
        out.add("jdk.logging=" + safe(() -> ResourceBundle.getBundle("sun.util.logging.resources.logging",
                Locale.GERMAN).getString("WARNING")));
        out.add("jul.level=" + safe(() -> Level.WARNING.getLocalizedName() + "|" + Level.INFO.getResourceBundleName()));
        out.add("jul.logger=" + safe(() -> {
            java.util.logging.Logger lg = java.util.logging.Logger.getLogger("sbrw4.rb", base);
            ResourceBundle b = lg.getResourceBundle();
            return lg.getResourceBundleName() + "|" + (b == null ? "null" : b.getString("greeting"));
        }));
        out.add("jul.format=" + safe(() -> {
            java.util.logging.LogRecord r = new java.util.logging.LogRecord(Level.INFO, "greeting");
            r.setResourceBundle(ResourceBundle.getBundle(base, Locale.GERMAN));
            return new java.util.logging.SimpleFormatter().formatMessage(r);
        }));
        out.add("avail.dfs=" + safe(() -> DecimalFormatSymbols.getAvailableLocales().length > 100));
        out.add("clearCache=" + safe(() -> {
            ResourceBundle.clearCache();
            return ResourceBundle.getBundle(base, Locale.GERMAN).getString("greeting");
        }));
        return out;
    }

    static void writeProps(File dir, String name, String body) throws Exception {
        try (Writer w = new OutputStreamWriter(new FileOutputStream(new File(dir, name)), StandardCharsets.UTF_8)) {
            w.write(body);
        }
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        File dir = new File(System.getProperty("java.io.tmpdir"), "sbrw4-" + ProcessHandle.current().pid());
        File pkg = new File(dir, "sbrw4");
        pkg.mkdirs();
        writeProps(pkg, "Msg.properties", "k=base\nonly=from-base\n");
        writeProps(pkg, "Msg_de.properties", "k=deutsch \\u00e4\n");
        propsLoader = new URLClassLoader(new URL[] {dir.toURI().toURL()}, SbrW4Locale2.class.getClassLoader());
        List<String> first = rows();
        for (String r : first) {
            System.out.println(r);
        }
        int drift = 0;
        StringBuilder firstDrift = new StringBuilder();
        for (int i = 0; i < iters; i++) {
            List<String> again = rows();
            for (int k = 0; k < first.size(); k++) {
                String a = k < again.size() ? again.get(k) : "<missing>";
                if (!first.get(k).equals(a)) {
                    drift++;
                    if (firstDrift.length() == 0) {
                        firstDrift.append(" first: iter ").append(i).append(' ').append(first.get(k))
                                .append(" -> ").append(a);
                    }
                }
            }
        }
        for (File f : Collections.singletonList(pkg)) {
            for (File c : f.listFiles()) {
                c.delete();
            }
            f.delete();
        }
        dir.delete();
        System.out.println("rows " + first.size() + " iters " + iters + " drift " + drift + firstDrift);
    }
}
