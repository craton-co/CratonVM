import java.lang.instrument.ClassDefinition;
import java.nio.file.Files;
import java.nio.file.Paths;

// Does a promoted invoke resolution survive a class redefinition?
//
// The point of the SECOND thread: the warm-up thread's own `invoke_cache`
// would answer the post-redefinition call without ever consulting the
// cross-thread promoted map, so a single-threaded probe cannot see the
// window this is about. A freshly started thread has an empty local cache
// and therefore reads `get_promoted_invoke` on its first dispatch.
public class RedefMain {
    static String callImpl(Impl x) { return x.tag(); }
    static String callSup(Sup x)   { return x.tag(); }

    static volatile String fromNewThread;

    static void inNewThread(final java.util.concurrent.Callable<String> c) throws Exception {
        Thread t = new Thread(new Runnable() {
            public void run() {
                try { fromNewThread = c.call(); }
                catch (Exception e) { fromNewThread = "EX:" + e; }
            }
        });
        t.start();
        t.join();
    }

    public static void main(String[] args) throws Exception {
        final int reps = Integer.parseInt(args[0]);
        final String implV2 = args[1];
        final String supV2  = args[2];

        // ---- case A: the redefined class IS the receiver ----
        final Impl x = new Impl();
        String before = null;
        for (int i = 0; i < reps; i++) before = callImpl(x);
        System.out.println("A.warm            = " + before);
        inNewThread(new java.util.concurrent.Callable<String>() {
            public String call() { return callImpl(x); }
        });
        System.out.println("A.otherThreadPre  = " + fromNewThread);

        RedefAgent.inst.redefineClasses(
            new ClassDefinition(Impl.class, Files.readAllBytes(Paths.get(implV2))));

        System.out.println("A.sameThreadPost  = " + callImpl(x));
        inNewThread(new java.util.concurrent.Callable<String>() {
            public String call() { return callImpl(x); }
        });
        System.out.println("A.otherThreadPost = " + fromNewThread + "   (want NEW)");

        // ---- case B: the redefined class DECLARES the method, the receiver
        // is a subclass that does not override it ----
        final Sub y = new Sub();
        String b2 = null;
        for (int i = 0; i < reps; i++) b2 = callSup(y);
        System.out.println("B.warm            = " + b2);
        inNewThread(new java.util.concurrent.Callable<String>() {
            public String call() { return callSup(y); }
        });
        System.out.println("B.otherThreadPre  = " + fromNewThread);

        RedefAgent.inst.redefineClasses(
            new ClassDefinition(Sup.class, Files.readAllBytes(Paths.get(supV2))));

        System.out.println("B.sameThreadPost  = " + callSup(y));
        inNewThread(new java.util.concurrent.Callable<String>() {
            public String call() { return callSup(y); }
        });
        System.out.println("B.otherThreadPost = " + fromNewThread + "   (want SUP-NEW)");
    }
}
