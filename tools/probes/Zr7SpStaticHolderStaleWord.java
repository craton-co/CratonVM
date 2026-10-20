// zr z7/sp probe (a): the last-ditch soft clear must KEEP a soft reference
// whose referent is also strongly reachable from the heap, even when a stale
// conservative compiled-frame word names that referent too.
//
// Shape: SoftRefBeforeOome's (tools/probes/interp/L5). The OSR-compiled loop in
// `main` calls soft.get() and allocates in the same frame until the heap is
// full, so at the last-ditch collection one conservative word of that frame is
// the dead get() result. A STATIC holds the same referent:
//   direct  the static field IS the referent (a static-slot root with the same
//           address as the conservative word);
//   field   the static holds a holder object whose field is the referent (a
//           heap path).
//
// HotSpot 25 (`java -XX:+UseZGC -Xmx64m Zr7SpStaticHolderStaleWord direct`,
// same for `field`):
//   oome true
//   held true
//   soft_cleared false
// Pass (CratonVM, both modes, both shapes): all three lines equal HotSpot.
// Positive control `CRATONVM_ZGC_SOFT_CLEAR_PRECISE_FIRST=0`: `soft_cleared
// true` (cp's rule clears every condemned referent a conservative word names).
// `direct` only: `CRATONVM_ZGC_SOFT_CLEAR_PRECISE_ROOT_VALUES=0` prints
// `soft_cleared true` too (the collector's precise drain defers a static root
// whose VALUE a conservative word also holds; zr z7/sp). `field` keeps it at
// `=0` (the holder object is a root no word names).
// `CRATONVM_ZGC_SOFT_CLEAR_CONSERVATIVE=0`: `soft_cleared false` (no clear).
// Engagement: `CRATONVM_DBG=weakref` prints `[weakref] clear-all:
// precise-first condemned=1 named=1 ...` (`direct`: `root_kept=1`; `field`:
// `precise_kept=1`).
import java.lang.ref.SoftReference;
import java.util.ArrayList;
import java.util.List;

public class Zr7SpStaticHolderStaleWord {
    static final class Holder {
        byte[] block;
    }

    static byte[] direct;
    static Holder holder;
    static long touched;

    public static void main(String[] args) {
        String shape = args.length > 0 ? args[0] : "direct";
        byte[] block = new byte[16 << 20];
        if (shape.equals("field")) {
            holder = new Holder();
            holder.block = block;
        } else {
            direct = block;
        }
        SoftReference<byte[]> soft = new SoftReference<>(block);
        block = null;
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
        byte[] strong = shape.equals("field") ? holder.block : direct;
        System.out.println("oome " + oome);
        System.out.println("held " + (strong != null && strong.length == (16 << 20)));
        System.out.println("soft_cleared " + (soft.get() == null));
    }
}
