import jdk.jfr.*;
public class Sbr5JfrMod2 {
    @Name("probe.E2") static class Ev extends Event { int n; }
    public static void main(String[] a) throws Exception {
        try (Recording r = new Recording()) {
            r.enable("probe.E2");
            r.start();
            Module jfr = FlightRecorder.class.getModule();
            Module me = Ev.class.getModule();
            Ev e = new Ev(); e.n = 1;
            System.out.println("exported=" + jfr.isExported("jdk.jfr.internal.event", me));
            try { e.commit(); System.out.println("commit ok"); } catch (Throwable t) { System.out.println("commit " + t); }
            try { e.commit(); System.out.println("commit2 ok"); } catch (Throwable t) { System.out.println("commit2 " + t); }
        }
    }
}
