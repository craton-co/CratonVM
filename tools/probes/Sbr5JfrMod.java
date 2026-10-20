import jdk.jfr.*;
public class Sbr5JfrMod {
    static class Ev extends Event { int n; }
    public static void main(String[] a) throws Exception {
        Module jfr = FlightRecorder.class.getModule();
        Module me = Ev.class.getModule();
        System.out.println("before exported=" + jfr.isExported("jdk.jfr.internal.event", me));
        EventType.getEventType(Ev.class);
        System.out.println("after exported=" + jfr.isExported("jdk.jfr.internal.event", me) + " unnamed=" + !me.isNamed());
        System.out.println("exportedToObject=" + jfr.isExported("jdk.jfr.internal.event", Object.class.getModule()));
        Ev e = new Ev(); e.n = 1;
        try { e.commit(); System.out.println("commit ok"); } catch (Throwable t) { System.out.println("commit " + t); }
    }
}
