import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

/**
 * Does touching an object through a field updater make it immortal?
 *
 * `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced` fails
 * on CratonVM for exactly the GUARDED parameterisations, and netty's guarded
 * handle (`Recycler$DefaultHandle`) is the one that carries an
 * `AtomicIntegerFieldUpdater`; the unguarded handle has no updater and those
 * parameterisations pass. If an updater roots its target, the handle is
 * immortal, and through `handle -> localPool -> owner` so is the Thread —
 * which is what that test measures.
 *
 * Each arm allocates a target, touches it the named way, drops it, and runs
 * the same `System.gc(); System.runFinalization()` loop the netty test does.
 * Arm 0 is the control: identical object, never touched.
 */
public final class UpdaterRetainProbe {

    static final int TIMEOUT_MS = 4000;

    static class Target {
        volatile int i;
        volatile long l;
        volatile Object o;
        final AtomicBoolean flag;
        Target(AtomicBoolean f) { flag = f; }
        @Override protected void finalize() { flag.set(true); }
    }

    static final AtomicIntegerFieldUpdater<Target> I =
            AtomicIntegerFieldUpdater.newUpdater(Target.class, "i");
    static final AtomicLongFieldUpdater<Target> L =
            AtomicLongFieldUpdater.newUpdater(Target.class, "l");
    static final AtomicReferenceFieldUpdater<Target, Object> O =
            AtomicReferenceFieldUpdater.newUpdater(Target.class, Object.class, "o");
    static final VarHandle VH;
    static {
        try {
            VH = MethodHandles.lookup().findVarHandle(Target.class, "i", int.class);
        } catch (Exception e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long await(AtomicBoolean flag) throws InterruptedException {
        long t0 = System.nanoTime();
        long deadline = t0 + TIMEOUT_MS * 1_000_000L;
        while (!flag.get()) {
            if (System.nanoTime() > deadline) {
                return -1;
            }
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    interface Touch { void apply(Target t); }

    static void arm(String name, Touch touch) throws InterruptedException {
        AtomicBoolean flag = new AtomicBoolean();
        Target t = new Target(flag);
        if (touch != null) {
            touch.apply(t);
        }
        t = null;
        long ms = await(flag);
        System.out.printf("%-44s %s%n", name,
                          ms < 0 ? "RETAINED (no finalize in " + TIMEOUT_MS + "ms)" : "collected in " + ms + "ms");
    }

    public static void main(String[] args) throws Exception {
        arm("0. control, never touched", null);
        arm("1. AtomicIntegerFieldUpdater.lazySet", t -> I.lazySet(t, 1));
        arm("2. AtomicIntegerFieldUpdater.compareAndSet", t -> I.compareAndSet(t, 0, 1));
        arm("3. AtomicIntegerFieldUpdater.get", t -> I.get(t));
        arm("4. AtomicLongFieldUpdater.compareAndSet", t -> L.compareAndSet(t, 0L, 1L));
        arm("5. AtomicReferenceFieldUpdater.compareAndSet", t -> O.compareAndSet(t, null, "x"));
        arm("6. VarHandle.compareAndSet", t -> VH.compareAndSet(t, 0, 1));
        arm("7. plain volatile write", t -> t.i = 1);
        System.out.println("(a RETAINED row is an object this VM cannot collect after that operation)");
        System.exit(0);
    }
}
