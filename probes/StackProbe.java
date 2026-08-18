/**
 * `Thread.getStackTrace()` on a RUNNING thread — fidelity check against HotSpot.
 *
 * A daemon sampler calls `main.getStackTrace()` every 2 ms while the main
 * thread spins through three named methods; a volatile `phase` records which
 * one is executing, so the histogram can be checked per phase rather than only
 * in aggregate. A correct VM names the running method on every sample and never
 * attributes one phase's samples to another method.
 *
 * Before the 2026-08-18 fix this VM returned a ZERO-LENGTH array for every
 * sample taken while the target was running (`<empty>`=1471 of 1495), because
 * the cross-thread snapshot was only ever deposited at BLOCKING points. See
 * fixed-bugs/getstacktrace-of-a-running-thread-returned-an-empty-array-FIXED-20260818.md
 *
 *   javac -d /tmp/sw probes/StackProbe.java
 *   java  -cp /tmp/sw StackProbe 40000000        # HotSpot control
 *   <cratonvm> ... -c /tmp/sw StackProbe 4000000
 */
public class StackProbe {
    static volatile long sink = 0;
    static volatile int phase = 0;

    static void alpha(long n) { phase = 1; for (long i = 0; i < n; i++) sink += i; }
    static void beta(long n)  { phase = 2; for (long i = 0; i < n; i++) sink += i * 3; }
    static void gamma(long n) { phase = 3; for (long i = 0; i < n; i++) sink += i ^ 7; }

    public static void main(String[] a) throws Exception {
        final Thread main = Thread.currentThread();
        final java.util.Map<String,Integer> hist = new java.util.LinkedHashMap<>();
        final java.util.Map<Integer,java.util.Map<String,Integer>> byPhase = new java.util.LinkedHashMap<>();
        Thread s = new Thread(() -> {
            for (;;) {
                StackTraceElement[] st = main.getStackTrace();
                String top = st.length > 0 ? st[0].getClassName()+"."+st[0].getMethodName() : "<empty>";
                int ph = phase;
                hist.merge(top, 1, Integer::sum);
                byPhase.computeIfAbsent(ph, k -> new java.util.LinkedHashMap<>()).merge(top, 1, Integer::sum);
                try { Thread.sleep(2); } catch (InterruptedException e) { return; }
            }
        });
        s.setDaemon(true);
        s.start();
        long N = Long.parseLong(a.length > 0 ? a[0] : "40000000");
        for (int r = 0; r < 3; r++) { alpha(N); beta(N); gamma(N); }
        phase = 0;
        Thread.sleep(50);
        System.out.println("STACKPROBE sink=" + sink);
        System.out.println("STACKPROBE overall histogram:");
        hist.forEach((k,v) -> System.out.println("STACKPROBE   " + v + "  " + k));
        System.out.println("STACKPROBE by phase (1=alpha 2=beta 3=gamma):");
        byPhase.forEach((p,m) -> m.forEach((k,v) -> System.out.println("STACKPROBE   phase=" + p + " " + v + "  " + k)));
    }
}
