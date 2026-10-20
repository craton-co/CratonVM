import jdk.jfr.*;
import java.lang.reflect.*;
public class Sbr5JfrM {
    @Name("probe.Ev") @Label("Probe") @StackTrace(false)
    static class Ev extends Event { @Label("n") int n; String s; }
    public static void main(String[] a) throws Exception {
        p("before:");
        for (Method m : Ev.class.getDeclaredMethods()) p("  " + m);
        for (Field f : Ev.class.getDeclaredFields()) p("  F " + f + " " + Modifier.toString(f.getModifiers()));
        EventType t = EventType.getEventType(Ev.class);
        p("after: " + t.getName() + " id=" + t.getId());
        for (Method m : Ev.class.getDeclaredMethods()) p("  " + m);
        for (Field f : Ev.class.getDeclaredFields()) p("  F " + f + " " + Modifier.toString(f.getModifiers()));
        p("jdkEvent types: " + FlightRecorder.getFlightRecorder().getEventTypes().stream().filter(x->x.getName().startsWith("jdk.")).count());
    }
    static void p(String s){System.out.println(s);}
}
