import java.util.concurrent.Callable;

/**
 * Lane 7, target 2: the null java.lang.Module family.
 *
 * The lane page records ClassLoader.getUnnamedModule() answering null under
 * CRATONVM_ENFORCE_NATIVE_SHADOW=all, surfacing through
 * ClassLoader.postDefineClass -&gt; NamedPackage.&lt;init&gt;. The native returns
 * a canonical mirror; the real bytecode is `return this.unnamedModule`, so the
 * two disagree exactly when the field was never published.
 *
 * Every receiver shape whose rules differ is covered, plus every accessor
 * derived from the one under test, per the lane-0 reviewed-Intrinsic protocol
 * step 3 — a fix to one that breaks another cannot hide here:
 *
 *   - the built-in application loader (one canonical unnamed module)
 *   - a user-defined loader (HotSpot gives every loader its OWN unnamed module)
 *   - a named module (java.base) as the control that must NOT read as unnamed
 *   - the identity contract Class.getModule() == loader.getUnnamedModule()
 *   - Module.isNamed()/getName()/getClassLoader(), which the JDK derives from
 *     the same object
 *
 * No identity hash codes and no addresses are printed: both VMs choose those
 * independently, and a probe that prints them diffs on every run.
 *
 * MEASURED 2026-09-10 (JDK 25.0.4+7, azure vm1, dev tip 7a8b79526): 10 of 10
 * rows identical across HotSpot, --jdk-only unarmed and --jdk-only armed with
 * CRATONVM_ENFORCE_NATIVE_SHADOW=all.
 */
public class L7UnnamedModuleSweep {

    static void row(String label, Callable<Object> c) {
        try {
            System.out.println(label + "=" + c.call());
        } catch (Throwable t) {
            System.out.println(label + " THREW " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) {
        ClassLoader scl = ClassLoader.getSystemClassLoader();
        row("1 scl.getUnnamedModule()==null", () -> scl.getUnnamedModule() == null);
        row("2 thisClass.getModule()==null", () -> L7UnnamedModuleSweep.class.getModule() == null);
        row("3 identity class==loader", () -> L7UnnamedModuleSweep.class.getModule() == scl.getUnnamedModule());
        row("4 unnamed.isNamed()", () -> scl.getUnnamedModule().isNamed());
        row("5 unnamed.getName()", () -> scl.getUnnamedModule().getName());
        row("6 unnamed.getClassLoader()==scl", () -> scl.getUnnamedModule().getClassLoader() == scl);
        row("7 named control java.base", () -> Object.class.getModule().getName());
        ClassLoader custom = new ClassLoader(scl) {
        };
        row("8 custom.getUnnamedModule()==null", () -> custom.getUnnamedModule() == null);
        row("9 custom unnamed != scl unnamed", () -> custom.getUnnamedModule() != scl.getUnnamedModule());
        row("10 custom unnamed loader==custom", () -> custom.getUnnamedModule().getClassLoader() == custom);
    }
}
