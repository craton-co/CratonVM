import jdk.jfr.*;
import java.lang.reflect.*;
public class Sbr5JfrM2 {
    static abstract class Base extends Event { int b; }
    static class Child extends Base { int c; }
    static class Plain { int x; }
    public static void main(String[] a) throws Exception {
        FlightRecorder.getFlightRecorder();
        for (String n : new String[]{"jdk.internal.event.FileReadEvent","jdk.jfr.events.FileReadEvent","jdk.internal.event.Event","jdk.jfr.Event","Sbr5JfrM2$Base","Sbr5JfrM2$Child","jdk.internal.event.ThreadSleepEvent","jdk.jfr.events.ExceptionStatisticsEvent","jdk.internal.event.VirtualThreadPinnedEvent"}) {
            Class<?> c = Class.forName(n, false, null == null ? Sbr5JfrM2.class.getClassLoader() : null);
            p(n + " abstract=" + Modifier.isAbstract(c.getModifiers()));
            for (Method m : c.getDeclaredMethods()) p("  M " + m.toString().replace("jdk.internal.event.","").replace("java.lang.",""));
            for (Field f : c.getDeclaredFields()) p("  F " + Modifier.toString(f.getModifiers()) + " " + f.getType().getSimpleName() + " " + f.getName());
        }
    }
    static void p(String s){System.out.println(s);}
}
