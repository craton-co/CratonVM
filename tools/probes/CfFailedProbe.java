import java.util.concurrent.CompletableFuture;

/** Is CompletableFuture.failedFuture reachable on this VM? */
public class CfFailedProbe {
    public static void main(String[] a) {
        try {
            CompletableFuture<String> f = CompletableFuture.failedFuture(new java.io.IOException("boom"));
            System.out.println("failedFuture.class = " + f.getClass().getName());
            System.out.println("isDone             = " + f.isDone());
            System.out.println("isCompletedExc     = " + f.isCompletedExceptionally());
            try { f.join(); System.out.println("join               = returned"); }
            catch (Throwable t) { System.out.println("join               = " + t.getClass().getName()
                    + " cause=" + (t.getCause() == null ? "null" : t.getCause().getClass().getName())); }
        } catch (Throwable e) {
            System.out.println("failedFuture.class = " + e.getClass().getName() + ": " + e.getMessage());
        }
        System.out.println("RESULT done");
    }
}
