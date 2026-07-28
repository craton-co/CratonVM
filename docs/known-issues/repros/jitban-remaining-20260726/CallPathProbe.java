public class CallPathProbe {
    static int n = 0;
    static long sink = 0;
    static class Boom extends RuntimeException { Boom() { super(null, null, false, false); } }
    static void thrower(int i) { sink += i; if ((i % 7) == 3) throw new Boom(); }
    static void guarded(int i) { try { n++; thrower(i); } finally { n--; } }

    interface Shape { void run(int i); }
    static class AnonImpl implements Shape { public void run(int i) { guarded(i); } }
    static class DirectImpl implements Shape {
        public void run(int i) { try { n++; thrower(i); } finally { n--; } }
    }

    static void drive(String name, Shape s, int iters) {
        n = 0;
        for (int i = 0; i < iters; i++) { try { s.run(i); } catch (Boom e) { } }
        System.out.println(String.format("%-22s final=%-8d %s", name, n, (n == 0 ? "OK" : "LEAK")));
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        n = 0;
        for (int i = 0; i < iters; i++) { try { guarded(i); } catch (Boom e) { } }
        System.out.println(String.format("%-22s final=%-8d %s", "STATIC-direct", n, (n == 0 ? "OK" : "LEAK")));
        drive("IFACE-class-delegating", new AnonImpl(), iters);
        drive("IFACE-class-inline", new DirectImpl(), iters);
        drive("LAMBDA-methodref", CallPathProbe::guarded, iters);
        drive("LAMBDA-body", i -> { try { n++; thrower(i); } finally { n--; } }, iters);
        System.out.println("sink=" + sink);
    }
}
