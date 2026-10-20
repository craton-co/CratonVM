// zr z7/sp probe (b): the last-ditch soft clear must never clear the
// WeakReference of a strongly reachable referent nor run its Cleaner, and (as
// HotSpot) keeps its SoftReference too.
//
// Shape: SoftRefBeforeOome's (tools/probes/interp/L5). The OSR-compiled loop in
// `main` allocates until the heap is full while a LIVE local (`live`, read
// after the loop) holds the referent; no static, no heap path. A conservative
// word of the compiled frame names the referent (engagement:
// `CRATONVM_DBG=weakref` prints `precise-first ... named=1`). A WeakReference
// and a Cleaner are registered on the referent. Two shapes:
//   entry  `live` is assigned before the loop;
//   late   `live` is first assigned INSIDE the compiled loop (iteration
//          LATE_AT).
// In both, the live local is ALSO a precise shadow-stack root (collect_roots
// step 14b); that is what `CRATONVM_ZGC_SOFT_CLEAR_PRECISE_ROOT_VALUES` reads.
//
// HotSpot 25 (`java -XX:+UseZGC -Xmx64m Zr7SpLiveLocalWeakCleaner entry`,
// same for `late`):
//   oome true
//   live true
//   weak_kept true
//   cleaned false
//   soft_cleared false
// Pass (CratonVM, both modes, both shapes): all five lines equal HotSpot.
// Controls:
//   `CRATONVM_ZGC_SOFT_CLEAR_PRECISE_ROOT_VALUES=0`: `soft_cleared true` (cp's
//   trade: the referent is in the forced-dead set), the other four = HotSpot
//   (rv5's soft-phase-only fix);
//   `..._PRECISE_ROOT_VALUES=0` + `CRATONVM_ZGC_SOFT_CLEAR_SOFT_PHASE_ONLY=0`:
//   `weak_kept false` and `cleaned true` (the folded predicate reaches the weak
//   and phantom phases: rv5's positive control);
//   `CRATONVM_ZGC_SOFT_CLEAR_CONSERVATIVE=0`: all five lines = HotSpot.
// On a binary without PRECISE_ROOT_VALUES (dev 11db85e72e) the default arm is
// the `=0` arm above and `SOFT_PHASE_ONLY=0` alone separates.
import java.lang.ref.Cleaner;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

public class Zr7SpLiveLocalWeakCleaner {
    static final int LATE_AT = 20000;
    static volatile boolean cleaned;
    static long touched;

    public static void main(String[] args) throws Exception {
        boolean late = args.length > 0 && args[0].equals("late");
        byte[] block = new byte[16 << 20];
        SoftReference<byte[]> soft = new SoftReference<>(block);
        WeakReference<byte[]> weak = new WeakReference<>(block);
        Cleaner.create().register(block, () -> cleaned = true);
        byte[] live = late ? null : block;
        block = null;
        List<long[]> hold = new ArrayList<>();
        boolean oome = false;
        int n = 0;
        try {
            while (true) {
                hold.add(new long[128]);
                if (live == null && ++n >= LATE_AT) {
                    live = soft.get();
                }
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
        System.out.println("live " + (live != null && live.length == (16 << 20)));
        System.out.println("weak_kept " + (live != null && weak.get() == live));
        System.out.println("cleaned " + cleaned);
        System.out.println("soft_cleared " + (soft.get() == null));
    }
}
