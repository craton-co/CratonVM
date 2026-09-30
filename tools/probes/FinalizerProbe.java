// Is the Finalizer thread alive after N quiet seconds, and does finalize() still run?
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D4)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] FinalizerProbe <quiet-seconds>
// Compare with the same command on HotSpot (java -cp ...).
import java.util.concurrent.*;

public class FinalizerProbe {
    static final CountDownLatch RAN = new CountDownLatch(1);
    static class F { @SuppressWarnings("removal") @Override protected void finalize() { RAN.countDown(); } }
    public static void main(String[] a) throws Exception {
        int quiet = Integer.parseInt(a[0]);
        Thread.sleep(quiet * 1000L);
        Thread fin = null;
        for (Thread t : Thread.getAllStackTraces().keySet()) if (t.getName().equals("Finalizer")) fin = t;
        System.out.println("after " + quiet + "s quiet: Finalizer thread " + (fin == null ? "ABSENT" : ("alive=" + fin.isAlive() + " state=" + fin.getState())));
        for (int i = 0; i < 50 && RAN.getCount() > 0; i++) {
            new F();
            System.gc();
            RAN.await(100, TimeUnit.MILLISECONDS);
        }
        System.out.println("finalize ran=" + (RAN.getCount() == 0));
    }
}
