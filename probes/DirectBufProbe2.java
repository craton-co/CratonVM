import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;

/**
 * Which half of direct-buffer reclamation is missing: the free path, or the
 * trigger that should fire it?
 */
public final class DirectBufProbe2 {

    static long reserved() {
        try {
            Class<?> vm = Class.forName("jdk.internal.misc.VM");
            Method m = vm.getMethod("maxDirectMemory");
            return (Long) m.invoke(null);
        } catch (Throwable t) {
            return -1;
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("maxDirectMemory=" + reserved());

        // 1. Does the GC clear a WeakReference at all?
        Object o = new Object();
        WeakReference<Object> w = new WeakReference<>(o);
        o = null;
        System.gc();
        Thread.sleep(200);
        System.gc();
        Thread.sleep(200);
        System.out.println("WEAK cleared=" + (w.get() == null));

        // 2. Does a PhantomReference get enqueued?
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object p = new Object();
        PhantomReference<Object> pr = new PhantomReference<>(p, q);
        p = null;
        System.gc();
        Thread.sleep(200);
        System.gc();
        Thread.sleep(300);
        Reference<?> polled = q.poll();
        System.out.println("PHANTOM enqueued=" + (polled == pr));

        // 3. Does an EXPLICIT cleaner().clean() release the reservation?
        //    If yes, the free path works and only the trigger is missing.
        ByteBuffer b = ByteBuffer.allocateDirect(64 * 1024 * 1024);
        b.putInt(0, 7);
        boolean explicitWorked = false;
        try {
            Method cleanerM = Class.forName("sun.nio.ch.DirectBuffer").getMethod("cleaner");
            Object cleaner = cleanerM.invoke(b);
            System.out.println("CLEANER present=" + (cleaner != null));
            if (cleaner != null) {
                cleaner.getClass().getMethod("clean").invoke(cleaner);
                explicitWorked = true;
            }
        } catch (Throwable t) {
            System.out.println("CLEANER lookup failed: " + t);
        }
        System.out.println("EXPLICIT-CLEAN ran=" + explicitWorked);

        // 4. After an explicit clean of a 64 MiB buffer, can we allocate 64 MiB
        //    more than the cap would otherwise allow? Allocate up to the cap in
        //    64 MiB steps and report how far we get.
        int got = 0;
        try {
            for (int i = 0; i < 64; i++) {
                ByteBuffer x = ByteBuffer.allocateDirect(64 * 1024 * 1024);
                x.putInt(0, i);
                got++;
                // dropped immediately
            }
        } catch (OutOfMemoryError e) {
            System.out.println("OOM after " + got + " x 64MiB (" + (got * 64L) + " MiB)");
        }
        System.out.println("RESULT transientBuffersBeforeOOM=" + got
                + " (HotSpot reclaims and keeps going)");
    }
}
