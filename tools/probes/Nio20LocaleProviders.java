import java.io.*;
import java.lang.reflect.*;
import java.net.*;
import java.nio.ByteBuffer;
import java.nio.channels.*;
import java.nio.file.*;
import java.text.*;
import java.time.*;
import java.time.format.*;
import java.util.*;

/**
 * nio20 wave 5 lane L4: conformance facts for `jdk-locale-providers` (the `--compatible`
 * locale-provider floor: `ResourceBundle`, `LocaleResources`, `LocaleData` / `Bundles`,
 * `CalendarDataUtility`, `DecimalFormatSymbols`, `BreakIterator`, the `Currency` display
 * bridges), the `TimeZone.getDefault()` clone, and the NIO failure rows of the r20
 * "nio failure exceptions residual sites" page (`reset`, `opts`). Every line is a fact (no
 * timings, no hashes), so the output diffs against HotSpot 25 as is. The default locale and
 * time zone are pinned to en-US / UTC first, so the host's settings do not enter.
 *
 * Run:
 *   javac -d out tools/probes/Nio20LocaleProviders.java
 *   java -cp out Nio20LocaleProviders > hs.txt
 *   cratonvm --jdk-only    -cp out Nio20LocaleProviders > jo.txt
 *   cratonvm --compatible  -cp out Nio20LocaleProviders > co.txt
 *   CRATONVM_COMPAT_ALIGN=-jdk-locale-providers cratonvm --compatible -cp out Nio20LocaleProviders
 * -Dnio20lp.rows=a,b selects rows. -Dnio20lp.bench=true prints `<row> <N> ns/op` lines instead
 * (bench-fmt bench-nf bench-dfs bench-df bench-dtf bench-display bench-rb bench-date
 * bench-chmobj bench-chmstr bench-currency, selectable with or without the `bench-` prefix;
 * -Dnio20lp.iters=N, default 5000, -Dnio20lp.warm=N warm passes, default 3; each line is
 * printed as soon as its row is measured), interleave `--compatible` with and without
 * `CRATONVM_COMPAT_ALIGN=-jdk-locale-providers` and `--jdk-only`.
 */
public class Nio20LocaleProviders {
    static String rows = System.getProperty("nio20lp.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }

    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    interface Fact { Object get() throws Throwable; }
    static String f(Fact x) { try { return String.valueOf(x.get()); } catch (Throwable t) { return "!" + ex(t); } }

    static final Locale[] LOCALES = {
        Locale.US, Locale.ROOT, Locale.GERMANY, Locale.FRANCE, Locale.forLanguageTag("de-CH"),
        Locale.forLanguageTag("ar-EG"), Locale.forLanguageTag("hi-IN"), Locale.JAPAN,
        Locale.forLanguageTag("sr-Latn-RS"), Locale.forLanguageTag("th-TH-u-nu-thai"),
        Locale.forLanguageTag("en-US-u-rg-dezzzz"), new Locale("ru", "RU"), Locale.forLanguageTag("es-419"),
    };

    public static void main(String[] a) throws Throwable {
        Locale.setDefault(Locale.US);
        TimeZone.setDefault(TimeZone.getTimeZone("UTC"));
        if (Boolean.getBoolean("nio20lp.bench")) { bench(); return; }
        String[] all = {"fmt", "nf", "dfs", "df", "dtf", "names", "currency", "calendar", "bi", "rb", "rbjdk",
                "tz", "reset", "opts"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio20LocaleProviders.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                System.out.println(r + " THREW " + ex(e.getCause()));
            }
        }
        System.out.println("done");
    }

    static String tag(Locale l) { return l.toLanguageTag(); }

    // ---- formatting -------------------------------------------------------------------

    static void fmt() {
        for (Locale l : LOCALES) {
            System.out.println("fmt " + tag(l) + " " + f(() -> String.format(l, "%,d|%.3f|%e|%,.2f|%(d|%08.2f",
                    1234567, Math.PI, 12345.678, -9876543.21, -42, -3.5)));
        }
    }

    static void nf() {
        for (Locale l : LOCALES) {
            System.out.println("nf " + tag(l)
                    + " num=" + f(() -> NumberFormat.getInstance(l).format(-1234567.891))
                    + " int=" + f(() -> NumberFormat.getIntegerInstance(l).format(1234567.5))
                    + " cur=" + f(() -> NumberFormat.getCurrencyInstance(l).format(-1234.5))
                    + " pct=" + f(() -> NumberFormat.getPercentInstance(l).format(0.256))
                    + " compact=" + f(() -> NumberFormat.getCompactNumberInstance(l, NumberFormat.Style.SHORT).format(1234567))
                    + " parse=" + f(() -> NumberFormat.getInstance(l).parse(NumberFormat.getInstance(l).format(1234.5))));
        }
    }

    static void dfs() {
        for (Locale l : LOCALES) {
            System.out.println("dfs " + tag(l) + " " + f(() -> {
                DecimalFormatSymbols s = DecimalFormatSymbols.getInstance(l);
                return (int) s.getDecimalSeparator() + "," + (int) s.getGroupingSeparator() + ","
                        + (int) s.getMinusSign() + "," + (int) s.getZeroDigit() + "," + (int) s.getPercent() + ","
                        + (int) s.getPerMill() + "," + s.getExponentSeparator() + "," + s.getInfinity() + ","
                        + s.getNaN() + "," + s.getCurrencySymbol() + "," + s.getInternationalCurrencySymbol() + ","
                        + (int) s.getMonetaryDecimalSeparator() + "," + (int) s.getMonetaryGroupingSeparator() + ","
                        + s.getLocale() + "," + (s.getCurrency() == null ? "null" : s.getCurrency().getCurrencyCode());
            }));
        }
        System.out.println("dfs-new " + f(() -> new DecimalFormatSymbols(Locale.GERMANY).getDecimalSeparator())
                + " " + f(() -> DecimalFormatSymbols.getInstance().getLocale())
                + " " + f(() -> DecimalFormatSymbols.getInstance(Locale.US) == DecimalFormatSymbols.getInstance(Locale.US)));
    }

    static final Date FIXED = new Date(1_700_000_000_000L);

    static void df() {
        int[] styles = {DateFormat.FULL, DateFormat.LONG, DateFormat.MEDIUM, DateFormat.SHORT};
        for (Locale l : LOCALES) {
            StringBuilder sb = new StringBuilder("df " + tag(l));
            for (int st : styles) {
                sb.append(" | ").append(f(() -> {
                    DateFormat d = DateFormat.getDateTimeInstance(st, st, l);
                    d.setTimeZone(TimeZone.getTimeZone("UTC"));
                    return d.format(FIXED);
                }));
            }
            sb.append(" | ").append(f(() -> new SimpleDateFormat("EEEE MMMM yyyy GG a zzzz", l).format(FIXED)));
            System.out.println(sb);
        }
    }

    static void dtf() {
        LocalDateTime t = LocalDateTime.of(2023, 11, 14, 22, 13, 20);
        for (Locale l : LOCALES) {
            System.out.println("dtf " + tag(l)
                    + " | " + f(() -> DateTimeFormatter.ofLocalizedDate(FormatStyle.FULL).withLocale(l).format(t))
                    + " | " + f(() -> DateTimeFormatter.ofLocalizedDateTime(FormatStyle.MEDIUM).withLocale(l).format(t))
                    + " | " + f(() -> DateTimeFormatter.ofLocalizedTime(FormatStyle.SHORT).withLocale(l).format(t))
                    + " | " + f(() -> DateTimeFormatter.ofPattern("EEEE MMMM LLL a", l).format(t))
                    + " | " + f(() -> DayOfWeek.MONDAY.getDisplayName(TextStyle.NARROW, l) + Month.MAY.getDisplayName(TextStyle.FULL_STANDALONE, l)));
        }
    }

    static void names() {
        for (Locale l : LOCALES) {
            System.out.println("names " + tag(l) + " " + f(() -> l.getDisplayName(l)) + " | "
                    + f(() -> l.getDisplayName(Locale.US)) + " | " + f(() -> Locale.GERMANY.getDisplayCountry(l)) + " | "
                    + f(() -> Locale.JAPANESE.getDisplayLanguage(l)) + " | "
                    + f(() -> TimeZone.getTimeZone("America/New_York").getDisplayName(false, TimeZone.LONG, l)) + " | "
                    + f(() -> TimeZone.getTimeZone("Europe/Berlin").getDisplayName(true, TimeZone.SHORT, l)));
        }
    }

    static void currency() {
        String[] codes = {"USD", "EUR", "JPY", "RUB", "GBP", "CHF", "INR"};
        for (Locale l : LOCALES) {
            StringBuilder sb = new StringBuilder("currency " + tag(l));
            for (String c : codes) {
                Currency cur = Currency.getInstance(c);
                sb.append(' ').append(c).append('=').append(f(() -> cur.getSymbol(l))).append('/')
                        .append(f(() -> cur.getDisplayName(l)));
            }
            sb.append(" of=").append(f(() -> Currency.getInstance(l)));
            System.out.println(sb);
        }
    }

    static void calendar() {
        for (Locale l : LOCALES) {
            System.out.println("calendar " + tag(l) + " " + f(() -> {
                Calendar c = Calendar.getInstance(TimeZone.getTimeZone("UTC"), l);
                return c.getClass().getSimpleName() + "," + c.getFirstDayOfWeek() + "," + c.getMinimalDaysInFirstWeek();
            }) + " " + f(() -> Arrays.toString(DateFormatSymbols.getInstance(l).getShortMonths()))
                    + " " + f(() -> Arrays.toString(DateFormatSymbols.getInstance(l).getAmPmStrings()))
                    + " " + f(() -> Calendar.getInstance(l).getDisplayName(Calendar.DAY_OF_WEEK, Calendar.LONG, l)));
        }
    }

    static List<Integer> walk(BreakIterator b, String s) {
        b.setText(s);
        List<Integer> out = new ArrayList<>();
        for (int p = b.first(); p != BreakIterator.DONE; p = b.next()) out.add(p);
        return out;
    }

    static void bi() {
        String text = "Hello, world! Mr. Smith's e-mail costs $3.50. Byeé́ 👍🏽 done.";
        for (Locale l : new Locale[] {Locale.US, Locale.GERMANY, Locale.forLanguageTag("th"), Locale.JAPAN}) {
            System.out.println("bi " + tag(l) + " word=" + f(() -> walk(BreakIterator.getWordInstance(l), text))
                    + " sent=" + f(() -> walk(BreakIterator.getSentenceInstance(l), text))
                    + " line=" + f(() -> walk(BreakIterator.getLineInstance(l), text))
                    + " char=" + f(() -> walk(BreakIterator.getCharacterInstance(l), text).size())
                    + " class=" + f(() -> BreakIterator.getWordInstance(l).getClass().getName()));
        }
        System.out.println("bi-thai " + f(() -> walk(BreakIterator.getWordInstance(new Locale("th")),
                "ภาษาไทยง่ายมาก")));
        System.out.println("bi-ops " + f(() -> {
            BreakIterator b = BreakIterator.getWordInstance(Locale.US);
            b.setText("one two three");
            return b.following(5) + "," + b.preceding(5) + "," + b.last() + "," + b.previous() + "," + b.current()
                    + "," + b.isBoundary(3) + "," + b.getText().getEndIndex();
        }));
    }

    // ---- bundles ------------------------------------------------------------------------

    static ClassLoader bundleLoader() throws IOException {
        Path dir = Files.createTempDirectory("nio20lp");
        Files.writeString(dir.resolve("nio20lp_msgs.properties"), "greet=Hello\nonly.root=root\nwho=World {0}\n");
        Files.writeString(dir.resolve("nio20lp_msgs_de.properties"), "greet=Hallo\n");
        Files.writeString(dir.resolve("nio20lp_msgs_de_CH.properties"), "greet=Gr\\u00fcezi\n");
        Files.writeString(dir.resolve("nio20lp_msgs_fr.properties"), "greet=Bonjour\nwho=Monde {0}\n");
        return new URLClassLoader(new URL[] {dir.toUri().toURL()}, Nio20LocaleProviders.class.getClassLoader());
    }

    static void rb() throws IOException {
        ClassLoader cl = bundleLoader();
        for (Locale l : new Locale[] {Locale.US, Locale.GERMAN, Locale.forLanguageTag("de-CH"), Locale.FRANCE,
                Locale.ROOT, Locale.JAPAN}) {
            System.out.println("rb " + tag(l) + " " + f(() -> {
                ResourceBundle b = ResourceBundle.getBundle("nio20lp_msgs", l, cl);
                return b.getString("greet") + "," + b.getString("only.root") + "," + b.getLocale() + ","
                        + b.getBaseBundleName() + "," + new TreeSet<>(b.keySet()) + "," + b.containsKey("who") + ","
                        + Collections.list(b.getKeys()).size() + "," + b.getClass().getName() + ","
                        + MessageFormat.format(b.getString("who"), 7);
            }));
        }
        System.out.println("rb-missing-key " + f(() -> ResourceBundle.getBundle("nio20lp_msgs", Locale.US, cl).getString("nope")));
        System.out.println("rb-missing-bundle " + f(() -> ResourceBundle.getBundle("nio20lp_none", Locale.GERMANY, cl)));
        System.out.println("rb-same " + f(() -> ResourceBundle.getBundle("nio20lp_msgs", Locale.FRANCE, cl)
                == ResourceBundle.getBundle("nio20lp_msgs", Locale.FRANCE, cl)));
        System.out.println("rb-nofallback " + f(() -> ResourceBundle.getBundle("nio20lp_msgs", Locale.ITALY, cl,
                ResourceBundle.Control.getNoFallbackControl(ResourceBundle.Control.FORMAT_PROPERTIES)).getLocale()));
        System.out.println("rb-candidates " + f(() -> ResourceBundle.Control.getControl(ResourceBundle.Control.FORMAT_DEFAULT)
                .getCandidateLocales("x", Locale.forLanguageTag("zh-Hant-TW"))));
        System.out.println("rb-list " + f(() -> {
            ListResourceBundle lb = new ListResourceBundle() {
                protected Object[][] getContents() { return new Object[][] {{"k", "v"}, {"n", 3}}; }
            };
            return lb.getString("k") + "," + lb.getObject("n") + "," + new TreeSet<>(lb.keySet()) + "," + lb.getLocale();
        }));
    }

    static void rbjdk() {
        // JDK-internal bundles asked from application code (module encapsulation decides).
        System.out.println("rbjdk-logging " + f(() -> ResourceBundle.getBundle("sun.util.logging.resources.logging",
                Locale.GERMANY).getString("SEVERE")));
        System.out.println("rbjdk-level " + f(() -> java.util.logging.Level.SEVERE.getLocalizedName()) + " "
                + f(() -> java.util.logging.Level.parse("WARNING")));
    }

    // ---- TimeZone.getDefault clone --------------------------------------------------------

    static void tz() {
        TimeZone a = TimeZone.getDefault();
        TimeZone b = TimeZone.getDefault();
        System.out.println("tz-identity " + (a == b) + " " + a.equals(b) + " " + a.getID() + " " + a.getClass().getName());
        a.setRawOffset(3_600_000);
        a.setID("Mine/Zone");
        TimeZone c = TimeZone.getDefault();
        System.out.println("tz-isolated " + c.getID() + " " + c.getRawOffset() + " " + a.getID() + " " + a.getRawOffset());
        System.out.println("tz-zoneid " + ZoneId.systemDefault());
    }

    // ---- NIO failure sites ----------------------------------------------------------------

    static void reset() throws Exception {
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            s.setOption(StandardSocketOptions.SO_LINGER, 0);
            s.close(); // RST
            Thread.sleep(200);
            ByteBuffer bb = ByteBuffer.allocate(16);
            System.out.println("reset-read " + f(() -> c.read(bb)));
            System.out.println("reset-read-again " + f(() -> c.read(bb)) + " open=" + c.isOpen());
            c.close();
        }
        try (ServerSocket ss = new ServerSocket(0, 50, InetAddress.getLoopbackAddress())) {
            Socket c = new Socket(InetAddress.getLoopbackAddress(), ss.getLocalPort());
            Socket s = ss.accept();
            s.setSoLinger(true, 0);
            s.close();
            Thread.sleep(200);
            System.out.println("reset-stream-read " + f(() -> c.getInputStream().read()));
            c.close();
        }
    }

    static void opts() throws Exception {
        for (InetAddress addr : new InetAddress[] {InetAddress.getByName("127.0.0.1"), InetAddress.getByName("::1")}) {
            String fam = addr instanceof Inet4Address ? "v4" : "v6";
            try (ServerSocketChannel ss = ServerSocketChannel.open()) {
                ss.bind(new InetSocketAddress(addr, 0));
                try (SocketChannel c = SocketChannel.open(ss.getLocalAddress()); SocketChannel s = ss.accept()) {
                    StringBuilder sb = new StringBuilder("opts-" + fam);
                    sb.append(" tos=").append(f(() -> { c.setOption(StandardSocketOptions.IP_TOS, 0x10); return c.getOption(StandardSocketOptions.IP_TOS); }));
                    sb.append(" nodelay=").append(f(() -> { c.setOption(StandardSocketOptions.TCP_NODELAY, true); return c.getOption(StandardSocketOptions.TCP_NODELAY); }));
                    sb.append(" keepalive=").append(f(() -> { c.setOption(StandardSocketOptions.SO_KEEPALIVE, true); return c.getOption(StandardSocketOptions.SO_KEEPALIVE); }));
                    sb.append(" linger=").append(f(() -> { c.setOption(StandardSocketOptions.SO_LINGER, 5); return c.getOption(StandardSocketOptions.SO_LINGER); }));
                    sb.append(" oob=").append(f(() -> { c.socket().setOOBInline(true); return c.socket().getOOBInline(); }));
                    sb.append(" rcvbuf>0=").append(f(() -> c.getOption(StandardSocketOptions.SO_RCVBUF) > 0));
                    System.out.println(sb);
                }
            } catch (IOException e) {
                System.out.println("opts-" + fam + " SKIP " + e.getClass().getName());
            }
        }
        try (DatagramChannel d = DatagramChannel.open()) {
            d.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
            System.out.println("opts-dgram tos=" + f(() -> { d.setOption(StandardSocketOptions.IP_TOS, 0x08); return d.getOption(StandardSocketOptions.IP_TOS); })
                    + " ttl=" + f(() -> { d.setOption(StandardSocketOptions.IP_MULTICAST_TTL, 3); return d.getOption(StandardSocketOptions.IP_MULTICAST_TTL); })
                    + " loop=" + f(() -> { d.setOption(StandardSocketOptions.IP_MULTICAST_LOOP, false); return d.getOption(StandardSocketOptions.IP_MULTICAST_LOOP); }));
        }
    }

    // ---- bench --------------------------------------------------------------------------
    //
    // nio20 wave 5 L4 fix-back: each row is warmed and measured on its own and its line is
    // printed (and flushed) the moment it is measured, so a harness timeout or one slow row
    // can no longer leave an empty table (the w5b `bench.sh` run printed nothing: the old loop
    // printed every row only after the third full pass). Rows are selected by name with or
    // without the `bench-` prefix; a row that throws prints `<row> FAILED <exception>`.
    // Rows `bench-chmobj` / `bench-chmstr` time `ConcurrentHashMap.get` with a `Locale` and a
    // `String` key alone: under `--compatible` the whole `ConcurrentHashMap` class is the VM's
    // segmented-store stand-in (72 registrations on the 25/linux kind map, none under
    // `--jdk-only`), and a non-`String` key costs a re-entrant `hashCode` / `equals` per call.

    static volatile Object sink;

    static boolean benchOn(String row) {
        return rows.equals("all") || on(row) || on(row.substring("bench-".length()));
    }

    static void bench() throws Exception {
        int iters = Integer.getInteger("nio20lp.iters", 5_000);
        int warm = Integer.getInteger("nio20lp.warm", 3);
        ClassLoader cl = bundleLoader();
        Locale[] ls = {Locale.US, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN};
        String[] keys = {"en_US", "de_DE", "fr_FR", "ja_JP"};
        java.util.concurrent.ConcurrentHashMap<Locale, Integer> byLocale = new java.util.concurrent.ConcurrentHashMap<>();
        java.util.concurrent.ConcurrentHashMap<String, Integer> byString = new java.util.concurrent.ConcurrentHashMap<>();
        for (int k = 0; k < 4; k++) { byLocale.put(ls[k], k); byString.put(keys[k], k); }
        String[] names = {"bench-fmt", "bench-nf", "bench-dfs", "bench-df", "bench-dtf", "bench-display", "bench-rb",
                "bench-date", "bench-chmobj", "bench-chmstr", "bench-currency"};
        LocalDateTime t = LocalDateTime.of(2023, 11, 14, 22, 13, 20);
        for (int k = 0; k < names.length; k++) {
            if (!benchOn(names[k])) continue;
            try {
                long ns = 0;
                for (int round = 0; round <= warm; round++) {
                    long t0 = System.nanoTime();
                    for (int i = 0; i < iters; i++) {
                        Locale l = ls[i & 3];
                        switch (k) {
                            case 0 -> sink = String.format(l, "%,d %.2f", i, i * 1.5);
                            case 1 -> sink = NumberFormat.getInstance(l).format(i * 1.25);
                            case 2 -> sink = DecimalFormatSymbols.getInstance(l);
                            case 3 -> sink = DateFormat.getDateTimeInstance(DateFormat.MEDIUM, DateFormat.SHORT, l).format(FIXED);
                            case 4 -> sink = DateTimeFormatter.ofLocalizedDateTime(FormatStyle.MEDIUM).withLocale(l).format(t);
                            case 5 -> sink = l.getDisplayName(Locale.US);
                            case 6 -> sink = ResourceBundle.getBundle("nio20lp_msgs", l, cl).getString("greet");
                            case 7 -> {
                                @SuppressWarnings("deprecation")
                                int y = new Date(1_000_000_000_000L + i).getYear();
                                sink = y + new Date(i).toString().length();
                            }
                            case 8 -> sink = byLocale.get(l);
                            case 9 -> sink = byString.get(keys[i & 3]);
                            default -> sink = Currency.getInstance(l).getSymbol(l);
                        }
                    }
                    ns = (System.nanoTime() - t0) / iters;
                }
                System.out.println(names[k] + " " + ns + " ns/op");
            } catch (Throwable e) {
                System.out.println(names[k] + " FAILED " + ex(e));
            }
            System.out.flush();
        }
    }
}
