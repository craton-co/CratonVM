// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane NATMISC): field-helper half of
// docs/feature-designs/jit-ir-optimizations.md
//
// A single-pass caller splices a tiny static body whose getstatic / putstatic
// first touches a class whose <clinit> throws. The helper runs <clinit> and
// the ExceptionInInitializerError (then NoClassDefFoundError) is built inside
// it; before the fix that helper call filed no inline-frame row, so the trace
// lost the spliced callee's frame. The Boom classes are loaded (class
// literals) but not initialised before the callers warm up, so the field rows
// resolve and the bodies can be spliced.
//
// Run: single-pass only
//   CRATONVM_TIER_C2_THRESHOLD=1000000000 CRATONVM_JIT_OSR_OPTIMIZING=0 cratonvm --real-jdk -cp out J27SpliceClinitTrace
// and once with defaults. Expected (HotSpot 25, both runs):
//   fact get-eiie-frames read,viaRead
//   fact get-eiie-cause boom-get
//   fact get-ncdfe-frames read,viaRead
//   fact put-eiie-frames write,viaWrite
//   fact put-eiie-cause boom-put
//   fact put-ncdfe-frames write,viaWrite
public class J27SpliceClinitTrace {
    static class BoomGet {
        static int V;
        static {
            if (Boolean.TRUE) throw new RuntimeException("boom-get");
        }
    }

    static class BoomPut {
        static int V;
        static {
            if (Boolean.TRUE) throw new RuntimeException("boom-put");
        }
    }

    static int read(boolean go) { return go ? BoomGet.V : 1; }
    static int viaRead(boolean go) { return read(go); }
    static void write(boolean go) { if (go) BoomPut.V = 1; }
    static void viaWrite(boolean go) { write(go); }

    static String frames(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : t.getStackTrace()) {
            if (!e.getClassName().equals("J27SpliceClinitTrace")) continue;
            String m = e.getMethodName();
            if (m.equals("main") || m.equals("tryGet") || m.equals("tryPut")) break;
            if (sb.length() > 0) sb.append(',');
            sb.append(m);
        }
        return sb.toString();
    }

    static Throwable tryGet() {
        try { viaRead(true); return null; } catch (Throwable t) { return t; }
    }

    static Throwable tryPut() {
        try { viaWrite(true); return null; } catch (Throwable t) { return t; }
    }

    public static void main(String[] args) {
        Class<?> g = BoomGet.class; // loaded, not initialised
        Class<?> p = BoomPut.class;
        long sink = 0;
        for (int i = 0; i < 300_000; i++) {
            sink += viaRead(false);
            viaWrite(false);
        }
        Throwable e1 = tryGet();
        System.out.println("fact get-eiie-frames " + frames(e1));
        System.out.println("fact get-eiie-cause "
                + (e1 instanceof ExceptionInInitializerError ? e1.getCause().getMessage() : String.valueOf(e1)));
        Throwable e2 = tryGet();
        System.out.println("fact get-ncdfe-frames "
                + (e2 instanceof NoClassDefFoundError ? frames(e2) : String.valueOf(e2)));
        Throwable e3 = tryPut();
        System.out.println("fact put-eiie-frames " + frames(e3));
        System.out.println("fact put-eiie-cause "
                + (e3 instanceof ExceptionInInitializerError ? e3.getCause().getMessage() : String.valueOf(e3)));
        Throwable e4 = tryPut();
        System.out.println("fact put-ncdfe-frames "
                + (e4 instanceof NoClassDefFoundError ? frames(e4) : String.valueOf(e4)));
        if (sink != 300_000 || g == p) System.out.println("fact sink-bad " + sink);
    }
}
