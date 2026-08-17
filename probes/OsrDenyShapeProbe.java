/**
 * Which method SHAPE makes the OSR door refuse outright?
 *
 * `compile_osr_artifact` returns `None` from ~30 `?`/`return None` points before
 * it ever reaches the backend, and every one of them is silent — a refused OSR
 * compile prints "OSR-compile FAILED ... OSR-denied" with no reason, and no
 * `codegen-bail` line, because codegen never ran. This narrows it by shape:
 * every arm is a once-called method with a hot loop, differing in one feature.
 *
 *   cratonvm --java-home <jdk> -cp . OsrDenyShapeProbe 200000
 *   (with CRATONVM_DBG_JITC=1, then grep 'OSR-compile FAILED')
 */
public final class OsrDenyShapeProbe {
    static long sink;
    static final RuntimeException E = new RuntimeException() {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static int leaf(int i) { return i + 1; }
    static void thrower(int i) { if ((i & 0xFFFF) == 0) { throw E; } }

    static void plainLoop(int n)      { long a = 0; for (int i = 0; i < n; i++) { a += leaf(i); } sink += a; }
    static void tryCatchLoop(int n)   { long a = 0; for (int i = 0; i < n; i++) { try { thrower(i); } catch (RuntimeException e) { a++; } } sink += a; }
    static void newBeforeLoop(int n)  { byte[] b = new byte[4]; long a = 0; for (int i = 0; i < n; i++) { a += b.length + leaf(i); } sink += a; }
    static void anonBeforeLoop(int n) {
        final Runnable r = new Runnable() { @Override public void run() { sink++; } };
        long a = 0;
        for (int i = 0; i < n; i++) { a += leaf(i); }
        if (a == Long.MIN_VALUE) { r.run(); }
        sink += a;
    }
    static void doWhileLoop(int n)    { long a = 0; int i = 0; do { a += leaf(i); i++; } while (i != n); sink += a; }
    static void tryCatchDoWhile(int n){ long a = 0; int i = 0; do { try { thrower(i); } catch (RuntimeException e) { a++; } i++; } while (i != n); sink += a; }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        plainLoop(n);
        tryCatchLoop(n);
        newBeforeLoop(n);
        anonBeforeLoop(n);
        doWhileLoop(n);
        tryCatchDoWhile(n);
        System.out.println("sink=" + sink);
    }
}
