// Minimal repro of the WildFly JUnit-platform ThrowableCollector NPE:
//   "Cannot read field 'throwable' because the object is null"
// Mirrors org.junit.platform.engine.support.hierarchical.ThrowableCollector:
//   execute(): try { e.run(); } catch (Throwable t) { add(t); }  // aload_0;aload t;invokespecial add
//   add():     if (this.throwable == null) ...                   // aload_0;getfield throwable
// The catch block in a JIT-compiled instance method reaches add() with `this`.
// If the JIT exception-handler entry fails to restore local-0 (this), add()
// sees this==null and the getfield throws NPE.
public class TCRepro {
    Throwable throwable;

    interface Exec { void run() throws Throwable; }

    void execute(Exec e) {
        try {
            e.run();
        } catch (Throwable t) {
            add(t);
        }
    }

    void add(Throwable t) {
        // aload_0; getfield throwable  -> NPE here if `this` is null
        if (this.throwable == null) {
            this.throwable = t;
        } else if (t != this.throwable) {
            this.throwable.addSuppressed(t);
        }
    }

    // Deeper throw site, closer to the real (many-frame) stack.
    static void deepThrow(int n) {
        if (n <= 0) throw new RuntimeException("boom");
        deepThrow(n - 1);
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        TCRepro c = new TCRepro();
        long ok = 0, bad = 0;
        for (int i = 0; i < iters; i++) {
            c.throwable = null;
            try {
                c.execute(() -> deepThrow(3));
                if (c.throwable != null) ok++; else bad++;
            } catch (Throwable t) {
                bad++;
                System.out.println("ITER " + i + " EXC " + t.getClass().getName()
                        + ": " + t.getMessage());
                if (bad <= 3) t.printStackTrace(System.out);
            }
        }
        System.out.println("done iters=" + iters + " ok=" + ok + " bad=" + bad);
    }
}
