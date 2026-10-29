// One row of JtsIsolate: an AssertionError escapes throwAe's catch(Exception) into escape()'s
// handler, which reads getMessage() (arg "msg") or not (arg "nomsg").
public class JtsOne {
    static int throwAe(int i) {
        try { if (i >= 0) throw new AssertionError("a"); return -1; } catch (Exception e) { return 6000; }
    }
    static int escapeMsg(int i) {
        try { return throwAe(i); } catch (AssertionError e) { return e.getMessage().length(); }
    }
    static int escapeNoMsg(int i) {
        try { return throwAe(i); } catch (AssertionError e) { return 2; }
    }
    public static void main(String[] a) {
        boolean msg = a.length == 0 || a[0].equals("msg");
        long acc = 0;
        for (int i = 0; i < 300_000; i++) acc += msg ? escapeMsg(i) : escapeNoMsg(i);
        long t0 = System.nanoTime();
        for (int i = 0; i < 100_000; i++) acc += msg ? escapeMsg(i) : escapeNoMsg(i);
        System.out.println("fact acc " + acc);
        System.out.println("time " + (msg ? "msg" : "nomsg") + " " + (System.nanoTime() - t0) / 100_000 + " ns");
    }
}
