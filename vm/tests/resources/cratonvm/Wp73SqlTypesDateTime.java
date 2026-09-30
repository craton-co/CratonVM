package cratonvm;

import java.sql.Date;
import java.sql.Time;
import java.sql.Timestamp;
import java.sql.Types;
import java.time.Instant;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.LocalTime;

/**
 * WP7.3 — JDBC SQL date/time interop conformance.
 *
 * Verifies that the legacy SQL date types ({@link java.sql.Date},
 * {@link java.sql.Time}, {@link java.sql.Timestamp}) and the modern
 * {@link java.time.*} driver paths interoperate correctly on cratonvm.
 *
 * <h2>Two-tier acceptance</h2>
 * The acceptance from {@code docs/wildfly-ejbca-roadmap.md} §10 WP7.3 has
 * two tiers given the open-source baseline state:
 *
 * <ol>
 *   <li><b>Tier-1 (must pass today)</b>: spec-fixed {@link Types} integer
 *       constants accessible via direct field reads, and the legacy SQL
 *       date/time classes constructible via their {@code (long millis)}
 *       constructor. This mirrors what {@code TckSql.java} pins in the
 *       JCK harness and is the strict JVM-side gate for WP7.3.</li>
 *   <li><b>Tier-2 (documented gaps)</b>: {@code java.time.*} factory
 *       methods, reflection paths ({@code Class.forName}, {@code
 *       Class.getField}), and the legacy/modern conversion bridge
 *       methods ({@code Timestamp.toLocalDateTime}, {@code
 *       Date.valueOf(LocalDate)}). These all return 0 / throw
 *       {@code NoSuchMethodError} on the open-sourced revision (see
 *       {@code TckLocalDate} / {@code TckInstant} JCK floors at 0 in
 *       {@code docs/jdk-regression-baseline.md}). The corresponding
 *       Rust tests are {@code #[ignore = "..."]}'d with a forward
 *       pointer to the upstream WP that owns the fix.</li>
 * </ol>
 *
 * <h2>Why no reflection?</h2>
 * Reflection ({@code Class.forName}, {@code Class.getField}) is an
 * orthogonal gap owned by WP2.1 (see {@code TckReflect} JCK floor at 5).
 * To keep WP7.3 evidence focused on the SQL types surface, the constant
 * checks read fields directly instead of via {@code getField} — this is
 * what TckSql does and what the JCK harness already validates.
 *
 * Each method returns 1 on pass, 0 on fail.
 */
public class Wp73SqlTypesDateTime {

    // ---------------------------------------------------------------------
    // Tier-1: java.sql.Types constants — direct field reads. Mirrors the
    // TckSql.types_* family which passes on the JCK harness.
    // ---------------------------------------------------------------------

    public static int types_varchar_is_12() {
        return Types.VARCHAR == 12 ? 1 : 0;
    }

    public static int types_integer_is_4() {
        return Types.INTEGER == 4 ? 1 : 0;
    }

    public static int types_timestamp_is_93() {
        return Types.TIMESTAMP == 93 ? 1 : 0;
    }

    public static int types_date_is_91() {
        return Types.DATE == 91 ? 1 : 0;
    }

    public static int types_time_is_92() {
        return Types.TIME == 92 ? 1 : 0;
    }

    // ---------------------------------------------------------------------
    // Tier-1: legacy SQL date/time classes — load via plain constructor
    // and check toString() returns a non-null formatted string. Mirrors
    // the way TckJdbc materializes Timestamp objects.
    // ---------------------------------------------------------------------

    public static int reach_sqlDate() {
        try {
            Date d = new Date(0L);
            return d.toString() != null ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int reach_sqlTime() {
        try {
            Time t = new Time(0L);
            return t.toString() != null ? 1 : 0;
        } catch (Throwable e) { return 0; }
    }

    public static int reach_sqlTimestamp() {
        try {
            Timestamp ts = new Timestamp(0L);
            return ts.toString() != null ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    /**
     * Diagnostic: returns the result of {@code Timestamp.getTime()}
     * directly so the Rust harness can see what value was actually
     * round-tripped. Lets us assert at the bytes level without a
     * boolean compare in Java.
     */
    public static long sqlTimestamp_getTime() {
        try {
            long millis = 1_745_700_896_000L; // 2025-04-26 ish
            Timestamp ts = new Timestamp(millis);
            return ts.getTime();
        } catch (Throwable t) { return Long.MIN_VALUE; }
    }

    /**
     * Diagnostic: returns the result of {@code Date.getTime()}
     * (inherited from {@code java.util.Date}).
     */
    public static long sqlDate_getTime() {
        try {
            long millis = 1_745_700_896_000L;
            Date d = new Date(millis);
            return d.getTime();
        } catch (Throwable t) { return Long.MIN_VALUE; }
    }

    /**
     * Diagnostic: returns the result of {@code Time.getTime()}
     * (inherited from {@code java.util.Date}).
     */
    public static long sqlTime_getTime() {
        try {
            long millis = 45_296_000L; // 12:34:56 since epoch
            Time t = new Time(millis);
            return t.getTime();
        } catch (Throwable e) { return Long.MIN_VALUE; }
    }

    // ---------------------------------------------------------------------
    // Tier-2: java.time.* reachability + legacy/modern conversion. These
    // currently return 0 / throw NoSuchMethodError on the open-sourced
    // revision; corresponding Rust tests are ignored with a clear note.
    // Kept here so a future WP can flip the ignore off without rewriting
    // the fixture.
    // ---------------------------------------------------------------------

    public static int reach_localDate() {
        try {
            LocalDate ld = LocalDate.of(2026, 4, 26);
            return ld != null && "2026-04-26".equals(ld.toString()) ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int reach_localTime() {
        try {
            LocalTime lt = LocalTime.of(12, 34, 56);
            return lt != null && "12:34:56".equals(lt.toString()) ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int reach_localDateTime() {
        try {
            LocalDateTime ldt = LocalDateTime.of(2026, 4, 26, 12, 34, 56);
            String s = ldt != null ? ldt.toString() : null;
            return s != null && s.startsWith("2026-04-26") ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int reach_instant() {
        try {
            Instant i = Instant.ofEpochMilli(0L);
            return i != null && i.toEpochMilli() == 0L ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int sqlDate_valueOf_localDate_roundtrip() {
        try {
            LocalDate ld = LocalDate.of(2026, 4, 26);
            Date d = Date.valueOf(ld);
            if (d == null) return 0;
            LocalDate back = d.toLocalDate();
            if (back == null) return 0;
            return "2026-04-26".equals(back.toString()) ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int sqlTime_valueOf_localTime_roundtrip() {
        try {
            LocalTime lt = LocalTime.of(12, 34, 56);
            Time t = Time.valueOf(lt);
            if (t == null) return 0;
            LocalTime back = t.toLocalTime();
            if (back == null) return 0;
            return "12:34:56".equals(back.toString()) ? 1 : 0;
        } catch (Throwable e) { return 0; }
    }

    public static int sqlTimestamp_toLocalDateTime_roundtrip() {
        try {
            long millis = 1_745_700_896_000L;
            Timestamp ts = new Timestamp(millis);
            LocalDateTime ldt = ts.toLocalDateTime();
            if (ldt == null) return 0;
            Timestamp ts2 = Timestamp.valueOf(ldt);
            if (ts2 == null) return 0;
            return ts2.getTime() == millis ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int instant_timestamp_roundtrip() {
        try {
            long millis = 1_745_700_896_000L;
            Instant i = Instant.ofEpochMilli(millis);
            Timestamp ts = Timestamp.from(i);
            if (ts == null) return 0;
            Instant back = ts.toInstant();
            if (back == null) return 0;
            return back.toEpochMilli() == millis ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int sqlDate_valueOf_string() {
        try {
            Date d = Date.valueOf("2026-04-26");
            return d != null && "2026-04-26".equals(d.toString()) ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }

    public static int sqlTime_valueOf_string() {
        try {
            Time t = Time.valueOf("12:34:56");
            return t != null && "12:34:56".equals(t.toString()) ? 1 : 0;
        } catch (Throwable e) { return 0; }
    }

    public static int sqlTimestamp_valueOf_string() {
        try {
            Timestamp ts = Timestamp.valueOf("2026-04-26 12:34:56");
            if (ts == null) return 0;
            String s = ts.toString();
            return s != null && s.startsWith("2026-04-26") ? 1 : 0;
        } catch (Throwable t) { return 0; }
    }
}
