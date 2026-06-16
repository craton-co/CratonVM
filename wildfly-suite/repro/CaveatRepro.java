// Caveat audit (bug-07 follow-up): the JIT MIC AnnotationProxy/lambda fast-paths
// return the result object as `obj.as_ptr() as i64` WITHOUT going through
// safe_native_call's native_pending_return rooting. Does the returned object
// survive a GC that fires right after the fast-path call?
//
// Hammers, under JIT + CRATONVM_DBG_GC_STRESS:
//   - annotationType() -> existing cached Class mirror (should be safe: field)
//   - value()          -> existing member String (field of the proxy)
//   - toString()       -> a FRESHLY ALLOCATED String (the real risk case)
// and consumes each result with interleaved allocation, validating integrity.
import java.lang.annotation.*;

public class CaveatRepro {
    @Retention(RetentionPolicy.RUNTIME) @interface Marker { String value(); }
    @Marker("hello-world-payload-1234567890") static class Annotated {}

    static long sink;
    static void churn() { Object s = null; for (int i = 0; i < 80; i++) s = new byte[256]; if (s == null) sink++; }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        Marker m = Annotated.class.getAnnotation(Marker.class); // AnnotationProxy
        long ok = 0, bad = 0; String firstBad = null;
        for (int i = 0; i < iters; i++) {
            // toString(): fresh String from the fast-path. Use it immediately in
            // an allocating expression (concat builds a StringBuilder/String) so
            // a GC can fire while the fast-path result is still in flight.
            String ts = m.toString();
            String combined = ts + "#" + i;            // allocates right after the call
            churn();                                    // GC while ts/combined live
            Class<?> t = m.annotationType();            // existing mirror
            String v = m.value();                       // existing member
            churn();
            boolean good = combined.startsWith(ts)
                    && ts.contains("hello-world-payload-1234567890")
                    && v.equals("hello-world-payload-1234567890")
                    && t != null && t.getName().contains("Marker");
            if (good) ok++;
            else { bad++; if (firstBad == null) firstBad = "ts=[" + ts + "] v=[" + v + "] t=" + t; }
        }
        System.out.println("done iters=" + iters + " ok=" + ok + " bad=" + bad + " sink=" + sink);
        if (firstBad != null) System.out.println("firstBad=" + firstBad);
    }
}
