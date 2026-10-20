// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.text.DateFormat;
import java.text.DecimalFormatSymbols;
import java.text.NumberFormat;
import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Calendar;
import java.util.Date;
import java.util.List;
import java.util.ListResourceBundle;
import java.util.Locale;
import java.util.ResourceBundle;
import java.util.TimeZone;

/**
 * SBR wave 1, lane LOCALE: the retired surface of {@code java.util.Locale},
 * {@code java.util.TimeZone}, {@code java.util.ResourceBundle} and the locale
 * provider readers behind them.
 *
 * Prints every row's FIRST-run value, then recomputes all rows inside a hot
 * loop and counts rows whose value changed ({@code drift}). A native that wins
 * only cold or only warm shows up as drift; a native that wins at every door
 * shows up as a diff against HotSpot with drift 0.
 *
 * Run: {@code java SbrW1Locale} (HotSpot oracle) vs
 * {@code cratonvm --jdk-only -cp . SbrW1Locale}.
 */
public class SbrW1Locale {

    public static class Res extends ListResourceBundle {
        @Override
        protected Object[][] getContents() {
            return new Object[][] {{"greeting", "hello"}, {"arr", new String[] {"a", "b"}}};
        }
    }

    public static class Res_de extends ListResourceBundle {
        @Override
        protected Object[][] getContents() {
            return new Object[][] {{"greeting", "hallo"}};
        }
    }

    static String safe(java.util.function.Supplier<Object> s) {
        try {
            Object o = s.get();
            if (o instanceof Object[] a) {
                return java.util.Arrays.toString(a);
            }
            return String.valueOf(o);
        } catch (Throwable t) {
            return "THROW " + t.getClass().getName();
        }
    }

    static void locRows(List<String> out, String tag, Locale l) {
        out.add(tag + ".toString=" + safe(() -> l.toString()));
        out.add(tag + ".toLanguageTag=" + safe(() -> l.toLanguageTag()));
        out.add(tag + ".lang=" + safe(() -> l.getLanguage()));
        out.add(tag + ".country=" + safe(() -> l.getCountry()));
        out.add(tag + ".script=" + safe(() -> l.getScript()));
        out.add(tag + ".variant=" + safe(() -> l.getVariant()));
        out.add(tag + ".ext.u=" + safe(() -> l.getExtension('u')));
        out.add(tag + ".ext.x=" + safe(() -> l.getExtension('x')));
        out.add(tag + ".strip=" + safe(() -> l.stripExtensions().toString()));
        out.add(tag + ".hash=" + safe(() -> l.hashCode()));
        out.add(tag + ".iso3=" + safe(() -> l.getISO3Language()));
        out.add(tag + ".display=" + safe(() -> l.getDisplayName(Locale.ENGLISH)));
        out.add(tag + ".upper(title)=" + safe(() -> "title".toUpperCase(l)));
        out.add(tag + ".lower(TITLE)=" + safe(() -> "TITLE".toLowerCase(l)));
    }

    static List<String> rows() {
        List<String> out = new ArrayList<>();
        // Defaults and their identity.
        out.add("default=" + safe(() -> Locale.getDefault()));
        out.add("default.same=" + (Locale.getDefault() == Locale.getDefault()));
        out.add("default.fmt=" + safe(() -> Locale.getDefault(Locale.Category.FORMAT)));
        out.add("default.disp=" + safe(() -> Locale.getDefault(Locale.Category.DISPLAY)));
        out.add("default.eqprop=" + safe(() -> Locale.getDefault().getLanguage()
                .equals(System.getProperty("user.language"))));
        // setDefault round trips, restored afterwards.
        Locale saved = Locale.getDefault();
        Locale savedFmt = Locale.getDefault(Locale.Category.FORMAT);
        try {
            Locale.setDefault(Locale.Category.FORMAT, Locale.GERMANY);
            out.add("setfmt.fmt=" + Locale.getDefault(Locale.Category.FORMAT));
            out.add("setfmt.base=" + Locale.getDefault());
            out.add("setfmt.format=" + String.format("%.2f", 1234.5));
            Locale.setDefault(Locale.FRANCE);
            out.add("set.base=" + Locale.getDefault());
            out.add("set.disp=" + Locale.getDefault(Locale.Category.DISPLAY));
            out.add("set.same=" + (Locale.getDefault() == Locale.FRANCE));
            // The no-Locale case-mapping natives read the default through a
            // side channel; a real setDefault has to reach them too.
            Locale.setDefault(new Locale("tr", "TR"));
            out.add("set.tr.lower=" + "TITLE".toLowerCase());
            out.add("set.tr.upper=" + "title".toUpperCase());
            out.add("set.tr.fmt=" + String.format("%S|%.1f", "title", 2.5));
            Locale.setDefault(Locale.Category.FORMAT, Locale.of("lt"));
            out.add("set.lt.fmt=" + String.format("%S", "title"));
        } finally {
            Locale.setDefault(saved);
            Locale.setDefault(Locale.Category.FORMAT, savedFmt);
        }
        out.add("restored=" + Locale.getDefault() + "/" + Locale.getDefault(Locale.Category.FORMAT));
        out.add("restored.lower=" + "TITLE".toLowerCase() + "|" + "title".toUpperCase());

        // forLanguageTag.
        String[] tags = {"en-US", "zh-Hant-TW", "en-US-u-ca-japanese", "und-DE", "ja-JP-x-lvariant-JP",
                "iw-IL", "", "de-AT-1996", "sr-Latn-RS", "en-US-x-java"};
        for (String t : tags) {
            locRows(out, "tag[" + t + "]", Locale.forLanguageTag(t));
        }
        out.add("tag.null=" + safe(() -> Locale.forLanguageTag(null)));
        // Deprecated constructors (their natives) and Locale.of.
        locRows(out, "new(iw,IL)", new Locale("iw", "IL"));
        locRows(out, "new(ja,JP,JP)", new Locale("ja", "JP", "JP"));
        locRows(out, "new(th,TH,TH)", new Locale("th", "TH", "TH"));
        locRows(out, "new(en,US,POSIX)", new Locale("en", "US", "POSIX"));
        locRows(out, "new(TR)", new Locale("TR"));
        locRows(out, "of(de,DE)", Locale.of("de", "DE"));
        locRows(out, "US", Locale.US);
        locRows(out, "ROOT", Locale.ROOT);
        locRows(out, "CHINA", Locale.CHINA);
        out.add("eq.new.of=" + new Locale("de", "DE").equals(Locale.of("de", "DE")));
        out.add("eq.new.const=" + new Locale("de", "DE").equals(Locale.GERMANY));
        out.add("eq.tag.const=" + Locale.forLanguageTag("en-US").equals(Locale.US));
        out.add("hash.new.of=" + (new Locale("fr").hashCode() == Locale.FRENCH.hashCode()));
        out.add("avail.nonempty=" + (Locale.getAvailableLocales().length > 0));
        out.add("avail.hasUS=" + java.util.Arrays.asList(Locale.getAvailableLocales()).contains(Locale.US));
        out.add("builder=" + safe(() -> new Locale.Builder().setLanguage("fr").setRegion("CA")
                .setExtension('u', "nu-arab").build().toLanguageTag()));

        // Formatting readers that decode a Locale.
        out.add("fmt.de=" + safe(() -> String.format(Locale.GERMANY, "%,.2f", 1234567.891)));
        out.add("fmt.newde=" + safe(() -> String.format(new Locale("de", "DE"), "%,.2f", 1234567.891)));
        out.add("nf.fr=" + safe(() -> NumberFormat.getInstance(Locale.FRANCE).format(1234567.891)));
        out.add("nf.tagch=" + safe(() -> NumberFormat.getInstance(Locale.forLanguageTag("de-CH")).format(1234567.891)));
        out.add("nf.cur.us=" + safe(() -> NumberFormat.getCurrencyInstance(Locale.US).format(12.5)));
        out.add("dfs.de.dec=" + safe(() -> DecimalFormatSymbols.getInstance(new Locale("de", "DE")).getDecimalSeparator()));
        out.add("dfs.of.it=" + safe(() -> DecimalFormatSymbols.getInstance(Locale.of("it", "IT")).getGroupingSeparator()));
        TimeZone ny = TimeZone.getTimeZone("America/New_York");
        Date when = new Date(1700000000000L);
        out.add("df.us=" + safe(() -> {
            DateFormat df = DateFormat.getDateTimeInstance(DateFormat.LONG, DateFormat.LONG, Locale.US);
            df.setTimeZone(ny);
            return df.format(when);
        }));
        out.add("df.fr=" + safe(() -> {
            DateFormat df = DateFormat.getDateInstance(DateFormat.FULL, Locale.FRANCE);
            df.setTimeZone(ny);
            return df.format(when);
        }));
        out.add("sdf.z=" + safe(() -> {
            SimpleDateFormat f = new SimpleDateFormat("yyyy-MM-dd HH:mm z", Locale.US);
            f.setTimeZone(ny);
            return f.format(when);
        }));
        out.add("sdf.tag=" + safe(() -> {
            SimpleDateFormat f = new SimpleDateFormat("EEEE d MMMM yyyy", Locale.forLanguageTag("de-DE"));
            f.setTimeZone(TimeZone.getTimeZone("UTC"));
            return f.format(when);
        }));
        out.add("cal.fdow.us=" + safe(() -> Calendar.getInstance(ny, Locale.US).getFirstDayOfWeek()));
        out.add("cal.fdow.fr=" + safe(() -> Calendar.getInstance(ny, Locale.FRANCE).getFirstDayOfWeek()));
        out.add("cal.fdow.tag=" + safe(() -> Calendar.getInstance(ny, Locale.forLanguageTag("de-DE")).getFirstDayOfWeek()));

        // TimeZone.
        String[] ids = {"America/New_York", "Europe/Berlin", "UTC", "GMT", "GMT+2", "GMT-05:30", "Not/AZone", "Asia/Kolkata"};
        for (String id : ids) {
            out.add("tz[" + id + "]=" + safe(() -> {
                TimeZone z = TimeZone.getTimeZone(id);
                return z.getID() + "|" + z.getRawOffset() + "|" + z.getOffset(1700000000000L) + "|"
                        + z.useDaylightTime() + "|" + z.inDaylightTime(when) + "|" + z.getClass().getName()
                        + "|" + z.getDisplayName(false, TimeZone.SHORT, Locale.US)
                        + "|" + z.getDisplayName(true, TimeZone.LONG, Locale.US);
            }));
        }
        out.add("tz.null=" + safe(() -> TimeZone.getTimeZone((String) null)));
        out.add("tz.default=" + safe(() -> TimeZone.getDefault().getID() + "|" + TimeZone.getDefault().getClass().getName()));
        out.add("tz.default.copy=" + (TimeZone.getDefault() != TimeZone.getDefault()));
        out.add("tz.default.eqprop=" + safe(() -> TimeZone.getDefault().getID().equals(System.getProperty("user.timezone"))));
        TimeZone savedTz = TimeZone.getDefault();
        try {
            TimeZone.setDefault(TimeZone.getTimeZone("Asia/Tokyo"));
            out.add("tz.set=" + TimeZone.getDefault().getID());
            out.add("tz.set.cal=" + Calendar.getInstance().getTimeZone().getID());
        } finally {
            TimeZone.setDefault(savedTz);
        }

        // ResourceBundle.
        out.add("rb.root=" + safe(() -> ResourceBundle.getBundle("SbrW1Locale$Res", Locale.ROOT).getString("greeting")));
        out.add("rb.de=" + safe(() -> ResourceBundle.getBundle("SbrW1Locale$Res", Locale.GERMAN).getString("greeting")));
        out.add("rb.de.locale=" + safe(() -> ResourceBundle.getBundle("SbrW1Locale$Res", Locale.GERMAN).getLocale()));
        out.add("rb.de.arr=" + safe(() -> ResourceBundle.getBundle("SbrW1Locale$Res", Locale.GERMAN).getStringArray("arr")));
        out.add("rb.de.has=" + safe(() -> ResourceBundle.getBundle("SbrW1Locale$Res", Locale.GERMAN).containsKey("arr")));
        out.add("rb.de.class=" + safe(() -> ResourceBundle.getBundle("SbrW1Locale$Res", Locale.GERMAN).getClass().getName()));
        out.add("rb.missing=" + safe(() -> ResourceBundle.getBundle("no.such.Bundle")));
        out.add("rb.missingkey=" + safe(() -> ResourceBundle.getBundle("SbrW1Locale$Res", Locale.ROOT).getString("nope")));
        out.add("rb.cand=" + safe(() -> ResourceBundle.Control.getControl(ResourceBundle.Control.FORMAT_DEFAULT)
                .getCandidateLocales("x", Locale.forLanguageTag("zh-Hant-TW"))));
        return out;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300;
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
        System.out.println("rows " + first.size() + " iters " + iters + " drift " + drift + firstDrift);
    }
}
