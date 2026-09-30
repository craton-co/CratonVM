package cratonvm;
import java.time.*;
import java.time.format.*;
public class TckLocalDate {
    public static int ld_now() { return LocalDate.now() != null ? 1 : 0; }
    public static int ld_of() { LocalDate d = LocalDate.of(2024, 6, 15); return d.getYear() == 2024 && d.getMonthValue() == 6 && d.getDayOfMonth() == 15 ? 1 : 0; }
    public static int ld_parse() { LocalDate d = LocalDate.parse("2024-03-15"); return d.getDayOfMonth() == 15 ? 1 : 0; }
    public static int ld_plusDays() { return LocalDate.of(2024, 1, 1).plusDays(10).getDayOfMonth() == 11 ? 1 : 0; }
    public static int ld_minusMonths() { return LocalDate.of(2024, 3, 15).minusMonths(1).getMonthValue() == 2 ? 1 : 0; }
    public static int ld_isLeapYear() { return LocalDate.of(2024, 1, 1).isLeapYear() ? 1 : 0; }
    public static int ld_dayOfWeek() { return LocalDate.of(2024, 1, 1).getDayOfWeek() == DayOfWeek.MONDAY ? 1 : 0; }
    public static int ld_compareTo() { return LocalDate.of(2024, 1, 1).compareTo(LocalDate.of(2024, 1, 2)) < 0 ? 1 : 0; }
    public static int ld_format() { return LocalDate.of(2024, 6, 15).format(DateTimeFormatter.ISO_LOCAL_DATE) != null ? 1 : 0; }
    public static int ld_toString() { return "2024-06-15".equals(LocalDate.of(2024, 6, 15).toString()) ? 1 : 0; }
}
