import java.lang.ref.SoftReference;

/**
 * gci i11/rp (finding R2 of
 * docs/known-issues/gc/gci-i11rp-generational-reference-processing-review-findings-20261013.md):
 * HotSpot stamps a {@code SoftReference} with {@code timestamp = clock}, the
 * clock as of the END of the previous collection, and judges it at a
 * collection against that same clock. A reference created {@code sleepMs}
 * after collection 1 therefore carries collection 1's clock: collection 2
 * keeps it (interval 0) and collection 3 measures the whole gap between
 * collections 1 and 2, which exceeds {@code SoftRefLRUPolicyMSPerMB * free MB}
 * (run with {@code -Xmx64m -XX:SoftRefLRUPolicyMSPerMB=10}: about 630 ms
 * against a 2000 ms sleep), so collection 3 clears it. A reference created
 * just before collection 3 carries collection 2's clock and is kept.
 *
 * <p>Neither reference is read with {@code get()} (a read re-stamps it);
 * {@code refersTo(null)} reads the referent slot without stamping.
 *
 * <p>stdout (HotSpot 25 Serial, by the rule above):
 * <pre>
 * late soft cleared: true
 * fresh soft kept: true
 * </pre>
 * Optional argument: the sleep in milliseconds (default 2000).
 */
public class GciI11RpSoftClockProbe {
    public static void main(String[] args) throws Exception {
        long sleepMs = args.length > 0 ? Long.parseLong(args[0]) : 2000L;
        System.gc(); // collection 1: SoftReference.clock := c1
        Thread.sleep(sleepMs);
        SoftReference<Object> late = new SoftReference<>(new Object()); // timestamp = c1
        System.gc(); // collection 2: interval 0, kept; clock := c2 (about c1 + sleepMs)
        SoftReference<Object> fresh = new SoftReference<>(new Object()); // timestamp = c2
        System.gc(); // collection 3: late's interval c2 - c1 > the policy window; fresh's 0
        System.out.println("late soft cleared: " + late.refersTo(null));
        System.out.println("fresh soft kept: " + !fresh.refersTo(null));
    }
}
