import java.time.ZoneId;
import java.util.Locale;
import java.util.TimeZone;

/**
 * What the VM reports as the platform's default time zone and locale.
 *
 * Written 2026-08-17 for the hibernate-reactive residual seven. Two of those
 * classes fail on values that never reach a test assertion directly:
 *
 *   * `ORMReactivePersistenceTest` dies with
 *     `FATAL: invalid value for parameter "TimeZone": "America/Buenos_Aires"` —
 *     the PostgreSQL JDBC driver sends `TimeZone.getDefault().getID()` to the
 *     server in its startup packet, so an ID the server's tzdata does not know
 *     kills the connection before any query runs. `America/Buenos_Aires` is the
 *     pre-2009 name; current tzdata has it only as a backward-compatibility
 *     LINK, and PostgreSQL builds without `--with-system-tzdata` do not carry
 *     the backward file.
 *
 *   * `DatabaseHibernateReactiveTest` asserts on an English Bean Validation
 *     message and gets a localized one, which is `Locale.getDefault()`.
 *
 * Run under both VMs on the same box; only the DIFFERENCE is a defect.
 */
public class DefaultLocaleTimeZoneProbe {
    public static void main(String[] args) {
        TimeZone tz = TimeZone.getDefault();
        System.out.println("timezone.id=" + tz.getID());
        System.out.println("timezone.rawoffset.ms=" + tz.getRawOffset());
        System.out.println("timezone.available.has.id=" + java.util.Arrays
                .asList(TimeZone.getAvailableIDs()).contains(tz.getID()));
        System.out.println("zoneid.systemDefault=" + ZoneId.systemDefault().getId());
        System.out.println("prop.user.timezone=" + System.getProperty("user.timezone"));

        Locale def = Locale.getDefault();
        System.out.println("locale.default=" + def);
        System.out.println("locale.display=" + Locale.getDefault(Locale.Category.DISPLAY));
        System.out.println("locale.format=" + Locale.getDefault(Locale.Category.FORMAT));
        System.out.println("prop.user.language=" + System.getProperty("user.language"));
        System.out.println("prop.user.country=" + System.getProperty("user.country"));
        System.out.println("PROBE-DONE");
    }
}
