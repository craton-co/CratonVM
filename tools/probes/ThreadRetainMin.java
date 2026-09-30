import java.lang.ref.WeakReference;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Minimises `recyclertest-thread-not-collected-once-the-jit-warms-up` to its
 * smallest reproducing shape.
 *
 * <p>`ThreadRetainMatrix` showed the retention reproduces in a plain `main`,
 * with no JUnit, no netty and no `Recycler` — which contradicts that page's
 * "it does not reproduce outside JUnit, and that is the sharpest thing known".
 * So the question is no longer "what does JUnit add"; it is "which ingredient
 * of this body is required".
 *
 * <p>Each arm below drops one thing. `weakCleared=true` means the arm collects
 * (good); `false` means it retains. Run under the failing JIT dose
 * (`CRATONVM_BG_COMPILE=0`) and again with `--nojit` as the control.
 */
public class ThreadRetainMin {

    static int loopSeconds = Integer.getInteger("secs", 5);

    interface Arm {
        /** Allocate the subject, return a weak ref to it, drop every strong ref. */
        WeakReference<?> make() throws Exception;
    }

    static void run(String label, Arm arm) throws Exception {
        WeakReference<?> weak = arm.make();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(loopSeconds);
        int rounds = 0;
        while (System.nanoTime() < deadline && weak.get() != null) {
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
            rounds++;
        }
        System.out.println("CK min " + pad(label)
                + " weakCleared=" + (weak.get() == null) + " rounds=" + rounds);
    }

    static String pad(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 34) {
            b.append(' ');
        }
        return b.toString();
    }

    public static void main(String[] args) throws Exception {
        // 1. netty's exact shape: started+joined Thread subclass with finalize().
        run("thread-subclass-finalize-started", () -> {
            AtomicBoolean flag = new AtomicBoolean();
            Thread t = new Thread(() -> { }) {
                @Override protected void finalize() throws Throwable {
                    try { flag.set(true); } finally { super.finalize(); }
                }
            };
            WeakReference<Thread> w = new WeakReference<>(t);
            t.start();
            t.join();
            return w;
        });

        // 2. Same, but NO finalize() override -- is the finalizer the ingredient?
        run("thread-plain-started", () -> {
            Thread t = new Thread(() -> { });
            WeakReference<Thread> w = new WeakReference<>(t);
            t.start();
            t.join();
            return w;
        });

        // 3. Subclass with finalize(), NEVER STARTED -- is `start()` required?
        run("thread-subclass-finalize-unstarted", () -> {
            AtomicBoolean flag = new AtomicBoolean();
            Thread t = new Thread(() -> { }) {
                @Override protected void finalize() throws Throwable {
                    try { flag.set(true); } finally { super.finalize(); }
                }
            };
            return new WeakReference<>(t);
        });

        // 4. Not a Thread at all: a plain Object with finalize().
        run("object-finalize", () -> {
            AtomicBoolean flag = new AtomicBoolean();
            Object o = new Object() {
                @Override protected void finalize() throws Throwable {
                    try { flag.set(true); } finally { super.finalize(); }
                }
            };
            return new WeakReference<>(o);
        });

        // 5. The floor: a plain object, no finalizer, no thread.
        run("plain-object", () -> new WeakReference<>(new int[16]));
    }
}
