package cratonvm;
import java.time.*;
public class TckZonedDateTime {
    public static int zdt_now() { return ZonedDateTime.now() != null ? 1 : 0; }
    public static int zdt_of() { ZonedDateTime z = ZonedDateTime.of(2024, 6, 15, 10, 30, 0, 0, ZoneId.of("UTC")); return z.getYear() == 2024 ? 1 : 0; }
    public static int zdt_getMonth() { ZonedDateTime z = ZonedDateTime.of(2024, 3, 1, 0, 0, 0, 0, ZoneId.of("UTC")); return z.getMonthValue() == 3 ? 1 : 0; }
    public static int zdt_getDayOfMonth() { ZonedDateTime z = ZonedDateTime.of(2024, 1, 15, 0, 0, 0, 0, ZoneId.of("UTC")); return z.getDayOfMonth() == 15 ? 1 : 0; }
    public static int zdt_getHour() { ZonedDateTime z = ZonedDateTime.of(2024, 1, 1, 14, 0, 0, 0, ZoneId.of("UTC")); return z.getHour() == 14 ? 1 : 0; }
    public static int zdt_getZone() { ZonedDateTime z = ZonedDateTime.of(2024, 1, 1, 0, 0, 0, 0, ZoneId.of("UTC")); return "UTC".equals(z.getZone().getId()) ? 1 : 0; }
    public static int zdt_toInstant() { ZonedDateTime z = ZonedDateTime.of(2024, 1, 1, 0, 0, 0, 0, ZoneId.of("UTC")); return z.toInstant() != null ? 1 : 0; }
    public static int zdt_plusDays() { ZonedDateTime z = ZonedDateTime.of(2024, 1, 1, 0, 0, 0, 0, ZoneId.of("UTC")); return z.plusDays(5).getDayOfMonth() == 6 ? 1 : 0; }
    public static int zdt_minusHours() { ZonedDateTime z = ZonedDateTime.of(2024, 1, 1, 10, 0, 0, 0, ZoneId.of("UTC")); return z.minusHours(3).getHour() == 7 ? 1 : 0; }
    public static int zdt_withZoneSameInstant() { ZonedDateTime z = ZonedDateTime.of(2024, 1, 1, 12, 0, 0, 0, ZoneId.of("UTC")); ZonedDateTime z2 = z.withZoneSameInstant(ZoneId.of("UTC")); return z.toInstant().equals(z2.toInstant()) ? 1 : 0; }
}
