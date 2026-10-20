import jdk.jfr.*;
import java.lang.reflect.*;
public class Sbr5JfrM3 {
    @Name("probe.Ev") static class Ev extends Event { int n; }
    static Object cfg() throws Exception { Field f = Ev.class.getDeclaredField("eventConfiguration"); f.setAccessible(true); return f.get(null); }
    public static void main(String[] a) throws Exception {
        System.out.println("cfg0=" + cfg());
        FlightRecorder fr = FlightRecorder.getFlightRecorder();
        System.out.println("cfg1=" + cfg());
        fr.getEventTypes();
        System.out.println("cfg2=" + cfg());
    }
}
