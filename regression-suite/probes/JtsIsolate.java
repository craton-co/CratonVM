// Isolates the --compatible error-escape residue: construction vs escape vs handler.
public class JtsIsolate {
    static final int N = 200_000;
    static volatile Object sink;

    static int concatOnly(int i) { sink = "a" + (i & 3); return 1; }
    static int aeConstLocal(int i) {
        try { if (i >= 0) throw new AssertionError("a"); return -1; } catch (AssertionError e) { return e.getMessage().length(); }
    }
    static int aeConcatLocal(int i) {
        try { if (i >= 0) throw new AssertionError("a" + (i & 3)); return -1; } catch (AssertionError e) { return e.getMessage().length(); }
    }
    static int iseConcatLocal(int i) {
        try { if (i >= 0) throw new IllegalStateException("a" + (i & 3)); return -1; } catch (IllegalStateException e) { return e.getMessage().length(); }
    }
    static int throwAeConst(int i) {
        try { if (i >= 0) throw new AssertionError("a"); return -1; } catch (Exception e) { return 6000; }
    }
    static int aeConstEscape(int i) {
        try { return throwAeConst(i); } catch (AssertionError e) { return e.getMessage().length(); }
    }
    static int throwAeConstNoHandler(int i) {
        if (i >= 0) throw new AssertionError("a");
        return -1;
    }
    static int aeConstEscapeNoHandler(int i) {
        try { return throwAeConstNoHandler(i); } catch (AssertionError e) { return e.getMessage().length(); }
    }
    static int aeConstEscapeNoMsg(int i) {
        try { return throwAeConst(i); } catch (AssertionError e) { return 2; }
    }

    interface K { int run(int i); }

    static void row(String label, K k) {
        long acc = 0;
        for (int i = 0; i < N; i++) acc += k.run(i);
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < N / 4; i++) acc += k.run(i);
            best = Math.min(best, (System.nanoTime() - t0) / (N / 4));
        }
        System.out.println("fact " + label + "-sum " + (acc % 1000003));
        System.out.println("time " + label + " " + best + " ns");
    }

    public static void main(String[] a) {
        row("concat-only", JtsIsolate::concatOnly);
        row("ae-const-local", JtsIsolate::aeConstLocal);
        row("ae-concat-local", JtsIsolate::aeConcatLocal);
        row("ise-concat-local", JtsIsolate::iseConcatLocal);
        row("ae-const-escape", JtsIsolate::aeConstEscape);
        row("ae-const-escape-nohandler", JtsIsolate::aeConstEscapeNoHandler);
        row("ae-const-escape-nomsg", JtsIsolate::aeConstEscapeNoMsg);
    }
}
