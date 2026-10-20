// zr z5/rv5 probe: the last-ditch soft clear must not treat a STRONGLY
// reachable object as dead for its weak reference or its Cleaner.
//
// Shape: SoftRefBeforeOome's (a compiled loop that keeps calling soft.get()
// while the heap fills with live data, so a conservatively scanned
// compiled-frame word names the soft referent at the last-ditch collection),
// plus a STATIC field holding the same object, a WeakReference and a Cleaner
// registered on it. No finalize(): a finalizable referent masked the defect
// (the finalizer closure kept the soft reference too).
//
// HotSpot 25 (`java -XX:+UseZGC -Xmx64m Zr5Rv5SoftClearLiveReferent`):
//   oome true
//   weak_kept true
//   cleaned false
//   soft_cleared false
// All four lines are the pass criterion on CratonVM (both modes).
// Controls: `CRATONVM_ZGC_SOFT_CLEAR_PRECISE_FIRST=0` prints `soft_cleared
// true` (the referent a conservative word names is cleared although the
// static holds it: docs/known-issues/gc/zgc-zr5rv5-conservative-soft-clear-
// includes-strongly-reachable-referents-20261012.md);
// `CRATONVM_ZGC_SOFT_CLEAR_SOFT_PHASE_ONLY=0` together with it prints
// `weak_kept false` and/or `cleaned true`.
import java.lang.ref.Cleaner;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

public class Zr5Rv5SoftClearLiveReferent {
    static final class Payload {
        final byte[] block = new byte[16 << 20];
    }

    static volatile boolean cleaned;
    static Payload strong;
    static long touched;

    public static void main(String[] args) throws Exception {
        Payload p = new Payload();
        strong = p;
        SoftReference<Payload> soft = new SoftReference<>(p);
        WeakReference<Payload> weak = new WeakReference<>(p);
        Cleaner.create().register(p, () -> cleaned = true);
        p = null;
        List<long[]> hold = new ArrayList<>();
        boolean oome = false;
        try {
            while (true) {
                hold.add(new long[1024]);
                if (soft.get() != null) {
                    touched++;
                }
            }
        } catch (OutOfMemoryError e) {
            hold = null;
            oome = true;
        }
        // Give a wrongly fired Cleaner the chance to run.
        for (int i = 0; i < 3; i++) {
            System.gc();
            Thread.sleep(100);
        }
        System.out.println("oome " + oome);
        System.out.println("weak_kept " + (strong != null && weak.get() == strong));
        System.out.println("cleaned " + cleaned);
        System.out.println("soft_cleared " + (soft.get() == null));
    }
}
