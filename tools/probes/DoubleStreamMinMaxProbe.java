import java.util.stream.DoubleStream;

/** Residual check for docs/known-issues/jdk-only/W7-2-primitive-stream-terminal-surface.md
 *  section 9.1's third bullet: DoubleStream.min()/max() folding with Rust's
 *  f64::min/f64::max instead of Java's NaN-propagating Math.min/Math.max rule.
 *
 *  Uses DoubleStream.range-style construction (rangeClosed-derived, via
 *  .map on an IntStream converted asDoubleStream) so the receiver is the
 *  synthetic interface-stamped stream this defect lives on, not a real
 *  DoublePipeline$Head from DoubleStream.of(...).
 */
public class DoubleStreamMinMaxProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        System.out.println(++rows + " " + tag + " |" + v + "|");
    }

    public static void main(String[] a) {
        DoubleStream s1 = java.util.stream.IntStream.rangeClosed(1, 3).asDoubleStream().map(x -> x == 2 ? Double.NaN : x);
        p("min(1.0,NaN,3.0)", s1.min());

        DoubleStream s2 = java.util.stream.IntStream.rangeClosed(1, 3).asDoubleStream().map(x -> x == 2 ? Double.NaN : x);
        p("max(1.0,NaN,3.0)", s2.max());

        DoubleStream s3 = java.util.stream.IntStream.rangeClosed(1, 2).asDoubleStream().map(x -> x == 1 ? -0.0 : 0.0);
        p("min(-0.0,0.0)", s3.min());

        DoubleStream s4 = java.util.stream.IntStream.rangeClosed(1, 3).asDoubleStream();
        p("min(1.0,2.0,3.0)", s4.min());

        DoubleStream s5 = java.util.stream.IntStream.rangeClosed(1, 3).asDoubleStream();
        p("max(1.0,2.0,3.0)", s5.max());
    }
}
