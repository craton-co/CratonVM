import java.lang.reflect.*;
import java.util.concurrent.atomic.*;
import java.util.function.*;

/** Why does `AtomicIntegerFieldUpdater$RustJvmImpl.updateAndGet` fail?
 *
 *  `AtomicUpdaterSweep` shows compatible mode dying with
 *
 *    NoSuchMethodError: AtomicIntegerFieldUpdater$RustJvmImpl
 *                       .updateAndGet(Object, IntUnaryOperator)
 *
 *  while `--jdk-only` runs all 87 rows clean. `class_manager.rs` DOES declare
 *  the synthetic impl as a direct subclass of the real abstract base, and the
 *  base's `updateAndGet` is ordinary JDK bytecode written in terms of `get` and
 *  `compareAndSet` -- both of which ARE registered on the impl. So inheritance
 *  ought to carry it.
 *
 *  Every base method the registrar does NOT register happens to be one of the
 *  four lambda-taking ones, so the sweep cannot separate two hypotheses:
 *
 *    H1  method resolution on a SYNTHETIC class does not walk to the real
 *        superclass at all -- nothing inherited works.
 *    H2  inheritance works, and something specific to these four fails.
 *
 *  This probe separates them. It asks for inherited members that are NOT
 *  lambda-taking and NOT registered: `Object`'s methods through the chain, the
 *  reflected superclass, and a reflective invoke of `updateAndGet` declared on
 *  the BASE (which resolves against the declaring class rather than the
 *  receiver's). Run it in COMPATIBLE mode -- strict never mints the class.
 */
public class RustJvmImplReach {
    static void p(String tag, Object v) { System.out.println(tag + " |" + v + "|"); }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) {
            Throwable c = (e instanceof InvocationTargetException && e.getCause() != null)
                ? e.getCause() : e;
            p(tag, "THREW " + c.getClass().getName());
        }
    }
    interface ThrowingRun { void run() throws Throwable; }

    public static class Box { volatile int i = 5; }

    public static void main(String[] a) throws Exception {
        AtomicIntegerFieldUpdater<Box> u =
            AtomicIntegerFieldUpdater.newUpdater(Box.class, "i");
        Box b = new Box();

        p("impl class", u.getClass().getName());
        p("superclass", u.getClass().getSuperclass() == null
            ? "null" : u.getClass().getSuperclass().getName());
        p("is an AtomicIntegerFieldUpdater", u instanceof AtomicIntegerFieldUpdater);

        // ---- H1's discriminators: INHERITED, not registered, no lambda ----
        // Object's own methods travel the same superclass chain the base's
        // bytecode would. If these fail, nothing inherited resolves.
        t("inherited toString", () -> u.toString());
        t("inherited hashCode", () -> u.hashCode());
        t("inherited equals", () -> u.equals(u));
        t("inherited getClass", () -> u.getClass());

        // ---- the registered ones, as the positive control ----------------
        t("registered get", () -> u.get(b));
        t("registered compareAndSet", () -> u.compareAndSet(b, 5, 6));

        // ---- the four that failed ----------------------------------------
        t("updateAndGet", () -> u.updateAndGet(b, x -> x + 1));
        t("getAndUpdate", () -> u.getAndUpdate(b, x -> x + 1));
        t("accumulateAndGet", () -> u.accumulateAndGet(b, 1, Integer::sum));
        t("getAndAccumulate", () -> u.getAndAccumulate(b, 1, Integer::sum));

        // ---- reflective invoke, declared on the BASE ----------------------
        // Reflection resolves against the DECLARING class, so if the base's
        // bytecode is reachable at all this is the shortest route to it.
        Method m = AtomicIntegerFieldUpdater.class
            .getMethod("updateAndGet", Object.class, IntUnaryOperator.class);
        p("reflected declaring class", m.getDeclaringClass().getName());
        t("reflective updateAndGet", () -> m.invoke(u, b, (IntUnaryOperator) x -> x + 1));

        // ---- does the base declare it as bytecode in THIS image? ---------
        // If the method has no Code the failure is a class-file gap, not a
        // dispatch one -- a different repair entirely.
        p("base declares updateAndGet", m != null);
        p("base updateAndGet is abstract", Modifier.isAbstract(m.getModifiers()));
        p("base updateAndGet modifiers", Modifier.toString(m.getModifiers()));

        // ---- the same question one class over, as a cross-check ----------
        AtomicLongFieldUpdater<LBox> lu = AtomicLongFieldUpdater.newUpdater(LBox.class, "l");
        LBox lb = new LBox();
        p("long impl class", lu.getClass().getName());
        t("long updateAndGet", () -> lu.updateAndGet(lb, x -> x + 1));
        AtomicReferenceFieldUpdater<RBox, String> ru =
            AtomicReferenceFieldUpdater.newUpdater(RBox.class, String.class, "s");
        RBox rb = new RBox();
        p("ref impl class", ru.getClass().getName());
        t("ref updateAndGet", () -> ru.updateAndGet(rb, x -> x + "!"));

        System.out.println("DONE RustJvmImplReach");
    }
    public static class LBox { volatile long l = 5; }
    public static class RBox { volatile String s = "a"; }
}
