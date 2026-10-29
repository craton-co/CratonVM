package cratonvm;
import java.time.*;
public class TckInstant {
    public static int instant_now() { return Instant.now() != null ? 1 : 0; }
    public static int instant_epoch() { return Instant.EPOCH.getEpochSecond() == 0 ? 1 : 0; }
    public static int instant_ofEpochSecond() { return Instant.ofEpochSecond(100).getEpochSecond() == 100 ? 1 : 0; }
    public static int instant_plusSeconds() { return Instant.ofEpochSecond(10).plusSeconds(5).getEpochSecond() == 15 ? 1 : 0; }
    public static int instant_minusSeconds() { return Instant.ofEpochSecond(10).minusSeconds(3).getEpochSecond() == 7 ? 1 : 0; }
    public static int instant_compareTo() { Instant a = Instant.ofEpochSecond(1); Instant b = Instant.ofEpochSecond(2); return a.compareTo(b) < 0 ? 1 : 0; }
    public static int instant_isBefore() { return Instant.ofEpochSecond(1).isBefore(Instant.ofEpochSecond(2)) ? 1 : 0; }
    public static int instant_isAfter() { return Instant.ofEpochSecond(2).isAfter(Instant.ofEpochSecond(1)) ? 1 : 0; }
    public static int instant_toEpochMilli() { return Instant.ofEpochSecond(1).toEpochMilli() == 1000 ? 1 : 0; }
    public static int instant_toString() { return Instant.EPOCH.toString() != null ? 1 : 0; }
    public static int instant_equals() { return Instant.ofEpochSecond(42).equals(Instant.ofEpochSecond(42)) ? 1 : 0; }
}
