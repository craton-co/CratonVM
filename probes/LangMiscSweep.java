import java.util.*;
import java.text.*;
import java.time.*;
import java.time.format.*;

/** The boxed primitives, Objects, Integer/Long/Double parsing and formatting,
 *  Enum, StringJoiner, Random, UUID, Base64, Calendar/SimpleDateFormat and the
 *  java.time core --- the remaining mid-size families on the retirement
 *  surface.
 *
 *  Determinism: Random is SEEDED, Calendar/SimpleDateFormat are pinned to UTC
 *  and Locale.ROOT, and java.time uses fixed instants. Nothing reads the clock.
 *  Values are hex-escaped so the diff cannot depend on stdout encoding. */
public class LangMiscSweep {
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

    enum E { A, B, C }

    static void boxes() {
        p("Integer.parseInt", Integer.parseInt("-123"));
        p("Integer.parseInt radix", Integer.parseInt("ff", 16));
        p("Integer.valueOf cache identity", Integer.valueOf(100) == Integer.valueOf(100));
        p("Integer.valueOf above cache", Integer.valueOf(1000) == Integer.valueOf(1000));
        p("Integer.toString radix", Integer.toString(255, 16));
        p("Integer.toHexString neg", Integer.toHexString(-1));
        p("Integer.toBinaryString", Integer.toBinaryString(10));
        p("Integer.toOctalString", Integer.toOctalString(64));
        p("Integer.MAX/MIN", Integer.MAX_VALUE + "/" + Integer.MIN_VALUE);
        p("Integer.compare", Integer.compare(1, 2) + "/" + Integer.compare(2, 1));
        p("Integer.bitCount", Integer.bitCount(255));
        p("Integer.reverse", Integer.reverse(1));
        p("Integer.highestOneBit", Integer.highestOneBit(100));
        p("Integer.numberOfLeadingZeros", Integer.numberOfLeadingZeros(1));
        p("Integer.rotateLeft", Integer.rotateLeft(1, 1));
        p("Integer.signum", Integer.signum(-5));
        p("Integer.divideUnsigned", Integer.divideUnsigned(-1, 2));
        p("Integer.toUnsignedLong", Integer.toUnsignedLong(-1));
        p("Integer.decode", Integer.decode("0x1F"));
        t("Integer.parseInt overflow", () -> Integer.parseInt("99999999999"));
        t("Integer.parseInt empty", () -> Integer.parseInt(""));
        t("Integer.parseInt null", () -> Integer.parseInt(null));
        t("Integer.parseInt bad radix", () -> Integer.parseInt("1", 99));

        p("Long.parseLong", Long.parseLong("-9007199254740993"));
        p("Long.toHexString", Long.toHexString(-1L));
        p("Long.compare", Long.compare(1L, 2L));
        p("Long.numberOfTrailingZeros", Long.numberOfTrailingZeros(8L));
        p("Long.hashCode", Long.hashCode(0x0102030405060708L));

        p("Double.parseDouble", Double.parseDouble("1.5e3"));
        p("Double.toString", Double.toString(0.1));
        p("Double.toString small", Double.toString(1e-320));
        p("Double.toString big", Double.toString(1e300));
        p("Double.compare NaN", Double.compare(Double.NaN, Double.NaN));
        p("Double.isNaN/isInfinite", Double.isNaN(0.0 / 0.0) + "/" + Double.isInfinite(1.0 / 0.0));
        p("Double.doubleToLongBits NaN", Double.doubleToLongBits(0.0 / 0.0));
        p("Double.doubleToRawLongBits -0", Double.doubleToRawLongBits(-0.0));
        p("Double.hashCode", Double.hashCode(1.5));
        p("Float.toString", Float.toString(0.1f));
        t("Double.parseDouble junk", () -> Double.parseDouble("x"));

        p("Boolean.parseBoolean", Boolean.parseBoolean("TrUe"));
        p("Character.valueOf identity", Character.valueOf('a') == Character.valueOf('a'));
        p("Byte.parseByte", Byte.parseByte("-128"));
        p("Short.MAX", Short.MAX_VALUE);
        p("Objects.equals nulls", Objects.equals(null, null) + "/" + Objects.equals(null, "a"));
        p("Objects.hashCode null", Objects.hashCode(null));
        p("Objects.toString null default", Objects.toString(null, "d"));
        p("Objects.hash", Objects.hash(1, "a", true));
        p("Objects.requireNonNullElse", Objects.requireNonNullElse(null, "e"));
        p("Objects.isNull/nonNull", Objects.isNull(null) + "/" + Objects.nonNull(null));
        p("Objects.compare", Objects.compare(1, 2, Comparator.naturalOrder()));
        t("Objects.requireNonNull", () -> Objects.requireNonNull(null, "msg"));
        t("Objects.checkIndex", () -> Objects.checkIndex(5, 3));
    }

    static void enumsAndJoiners() {
        p("enum values", Arrays.toString(E.values()));
        p("enum valueOf", E.valueOf("B"));
        p("enum ordinal/name", E.B.ordinal() + "/" + E.B.name());
        p("enum compareTo", E.A.compareTo(E.C));
        p("enum toString", E.C.toString());
        p("enum getDeclaringClass", E.A.getDeclaringClass().getSimpleName());
        p("EnumSet.allOf", EnumSet.allOf(E.class));
        p("EnumSet.of", EnumSet.of(E.A, E.C));
        p("EnumSet.complementOf", EnumSet.complementOf(EnumSet.of(E.A)));
        p("EnumSet.noneOf isEmpty", EnumSet.noneOf(E.class).isEmpty());
        EnumMap<E, Integer> em = new EnumMap<>(E.class);
        em.put(E.C, 3); em.put(E.A, 1);
        p("EnumMap ordinal order", em);
        t("enum valueOf bad", () -> E.valueOf("Z"));

        StringJoiner sj = new StringJoiner(", ", "[", "]");
        sj.add("a").add("b");
        p("StringJoiner", sj);
        p("StringJoiner length", sj.length());
        StringJoiner empty = new StringJoiner(",", "<", ">");
        p("StringJoiner empty", empty);
        empty.setEmptyValue("EMPTY");
        p("StringJoiner emptyValue", empty);
        StringJoiner m = new StringJoiner("-");
        m.merge(new StringJoiner(",").add("x").add("y"));
        p("StringJoiner merge", m);
    }

    static void randomsAndIds() {
        Random r = new Random(42L);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 6; i++) sb.append(r.nextInt(1000)).append(',');
        p("seeded nextInt", sb);
        p("seeded nextLong", new Random(42L).nextLong());
        p("seeded nextDouble bits", Double.doubleToRawLongBits(new Random(42L).nextDouble()));
        p("seeded nextGaussian bits", Double.doubleToRawLongBits(new Random(42L).nextGaussian()));
        p("seeded nextBoolean", new Random(42L).nextBoolean());
        byte[] bs = new byte[8];
        new Random(42L).nextBytes(bs);
        p("seeded nextBytes", Arrays.toString(bs));
        p("seeded ints stream", Arrays.toString(new Random(42L).ints(5, 0, 100).toArray()));
        p("setSeed resets", firstOf(42L) == firstOf(42L));

        UUID u = UUID.fromString("123e4567-e89b-12d3-a456-426614174000");
        p("uuid toString", u);
        p("uuid most/least", u.getMostSignificantBits() + "/" + u.getLeastSignificantBits());
        p("uuid version/variant", u.version() + "/" + u.variant());
        p("uuid equals", u.equals(UUID.fromString(u.toString())));
        p("uuid hashCode stable", u.hashCode() == UUID.fromString(u.toString()).hashCode());
        p("uuid nameUUIDFromBytes", UUID.nameUUIDFromBytes("abc".getBytes()));
        t("uuid bad string", () -> UUID.fromString("nope"));

        byte[] data = "hello world".getBytes();
        String b64 = Base64.getEncoder().encodeToString(data);
        p("base64 encode", b64);
        p("base64 decode", new String(Base64.getDecoder().decode(b64)));
        p("base64 url", Base64.getUrlEncoder().encodeToString(new byte[]{-1, -2, -3}));
        p("base64 no-pad", Base64.getEncoder().withoutPadding().encodeToString(new byte[]{1}));
        p("base64 mime len", Base64.getMimeEncoder().encodeToString(new byte[80]).length());
        t("base64 decode bad", () -> Base64.getDecoder().decode("!!!"));
    }
    static int firstOf(long seed) { return new Random(seed).nextInt(); }

    static void timeAndFormat() {
        Instant i = Instant.ofEpochMilli(1_000_000_000_000L);
        p("Instant", i);
        p("Instant epochSecond/nano", i.getEpochSecond() + "/" + i.getNano());
        p("Instant plusSeconds", i.plusSeconds(3600));
        LocalDate d = LocalDate.of(2026, 8, 27);
        p("LocalDate", d);
        p("LocalDate dayOfWeek", d.getDayOfWeek());
        p("LocalDate plusDays", d.plusDays(10));
        p("LocalDate isLeapYear", d.isLeapYear());
        p("LocalDate lengthOfMonth", d.lengthOfMonth());
        LocalDateTime dt = LocalDateTime.of(2026, 8, 27, 13, 45, 30);
        p("LocalDateTime", dt);
        p("LocalDateTime format ISO", dt.format(DateTimeFormatter.ISO_LOCAL_DATE_TIME));
        p("LocalDateTime custom", dt.format(DateTimeFormatter.ofPattern("yyyy/MM/dd HH:mm:ss", Locale.ROOT)));
        p("Duration", Duration.ofMinutes(90));
        p("Duration toString", Duration.ofSeconds(3661).toString());
        p("Period", Period.of(1, 2, 3));
        p("ZonedDateTime UTC", dt.atZone(ZoneId.of("UTC")));
        p("Month.FEBRUARY", Month.FEBRUARY);
        p("DayOfWeek.of", DayOfWeek.of(3));
        t("LocalDate bad month", () -> LocalDate.of(2026, 13, 1));
        t("Instant parse junk", () -> Instant.parse("nope"));

        TimeZone.setDefault(TimeZone.getTimeZone("UTC"));
        SimpleDateFormat f = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.ROOT);
        f.setTimeZone(TimeZone.getTimeZone("UTC"));
        p("SDF format", f.format(new Date(1_000_000_000_000L)));
        p("SDF round-trip", safeParse(f, "2001-09-09 01:46:40"));
        Calendar c = Calendar.getInstance(TimeZone.getTimeZone("UTC"), Locale.ROOT);
        c.setTimeInMillis(1_000_000_000_000L);
        p("Calendar YEAR/MONTH/DAY", c.get(Calendar.YEAR) + "/" + c.get(Calendar.MONTH) + "/" + c.get(Calendar.DAY_OF_MONTH));
        p("Calendar getTimeInMillis", c.getTimeInMillis());
        c.add(Calendar.DAY_OF_MONTH, 40);
        p("Calendar add rolls month", c.get(Calendar.YEAR) + "/" + c.get(Calendar.MONTH) + "/" + c.get(Calendar.DAY_OF_MONTH));
        NumberFormat nf = NumberFormat.getInstance(Locale.ROOT);
        p("NumberFormat", nf.format(1234567.891));
        p("DecimalFormat", new DecimalFormat("#,##0.00", DecimalFormatSymbols.getInstance(Locale.ROOT)).format(1234.5));
    }
    static String safeParse(SimpleDateFormat f, String s) {
        try { return String.valueOf(f.parse(s).getTime()); }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    public static void main(String[] a) {
        boxes();
        enumsAndJoiners();
        randomsAndIds();
        timeAndFormat();
        System.out.println("DONE LangMiscSweep");
    }
}
