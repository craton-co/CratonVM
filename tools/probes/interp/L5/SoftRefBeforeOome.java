// Lane L5 probe: every SoftReference must be cleared before OutOfMemoryError.
//
// java.lang.ref.SoftReference: "All soft references to softly-reachable objects
// are guaranteed to have been cleared before the virtual machine throws an
// OutOfMemoryError." The GC-overhead-limit early exit in CratonVM's allocation
// paths (alloc_object_shared / gc_alloc_array / create_exception_object_for_class)
// used to throw OOME without running the soft-reference rung.
//
// Run with a small heap, e.g. -Xmx64m. HotSpot 25 prints:
//   oome true
//   cleared true
// (`cleared` must be true: by the time OOME is caught, the 16 MiB soft-held
// block must have been released.) The exact point OOME is thrown is
// timing-dependent, so nothing else is printed.
import java.lang.ref.SoftReference;
import java.util.ArrayList;
import java.util.List;

public class SoftRefBeforeOome {
    static long touched;

    public static void main(String[] args) {
        SoftReference<byte[]> soft = new SoftReference<>(new byte[16 << 20]);
        List<long[]> hold = new ArrayList<>();
        boolean oome = false;
        try {
            // Retain everything: the heap fills with LIVE data, which is the
            // shape that trips the GC-overhead limit rather than a single
            // failed allocation.
            while (true) {
                hold.add(new long[1024]);
                // Keep touching the soft reference so an LRU policy alone
                // would never consider it idle.
                if (soft.get() != null) {
                    touched++;
                }
            }
        } catch (OutOfMemoryError e) {
            hold = null;
            oome = true;
        }
        System.out.println("oome " + oome);
        System.out.println("cleared " + (soft.get() == null));
    }
}
