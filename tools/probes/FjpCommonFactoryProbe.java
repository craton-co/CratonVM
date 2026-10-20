import java.util.concurrent.ForkJoinPool;

/**
 * W7-14's Falsifier, run as written. Three rows, and the third is the one that
 * decides whether the fix stays: "If Compatible moves, this lane's ordering
 * argument is wrong and the change should be reverted, not patched."
 */
public class FjpCommonFactoryProbe {
    public static void main(String[] a) {
        try {
            ForkJoinPool.ForkJoinWorkerThreadFactory f = ForkJoinPool.commonPool().getFactory();
            System.out.println("factory.class = " + f.getClass().getName());
            System.out.println("isDefaultSingleton = "
                    + (f == ForkJoinPool.defaultForkJoinWorkerThreadFactory));
            System.out.println("factory.isNull = " + (f == null));
        } catch (Throwable t) {
            System.out.println("factory.class = " + t.getClass().getName() + ": " + t.getMessage());
            System.out.println("isDefaultSingleton = unreached");
            System.out.println("factory.isNull = unreached");
        }
        System.out.println("RESULT done");
    }
}
