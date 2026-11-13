// Interpreter round i45x, lane L5 (docs/feature-designs/class-resolution-and-caching.md, closure).
//
// Module.isExported(String, Module) and Module.isOpen(String, Module) run
// Objects.requireNonNull(pn) then Objects.requireNonNull(other) in the JDK
// (java.lang.Module, JDK 9..25): a null package or a null target module is a
// bare NullPointerException, never `false`. The package is checked first.
//
// Self-comparing: every row must print "npe" on HotSpot and in both modes of
// this VM; any other word is a divergence.
//
//   javac -d out tools/probes/interp/L5/I45xModuleQualifiedNullTarget.java
//   for MODE in --real-jdk --compatible; do cratonvm $MODE -cp out I45xModuleQualifiedNullTarget; done
//
// Expected (HotSpot, every row):
//   base isExported(pn, null) npe
//   base isOpen(pn, null) npe
//   base isExported(null, other) npe
//   base isOpen(null, other) npe
//   base isExported(null, null) npe
//   unnamed isExported(pn, null) npe
//   unnamed isOpen(pn, null) npe
//   base isExported(pn, other) false
//   base isOpen(pn, other) false
public class I45xModuleQualifiedNullTarget {
    interface Call {
        boolean run() throws Throwable;
    }

    static void row(String label, Call call) {
        String out;
        try {
            out = String.valueOf(call.run());
        } catch (NullPointerException e) {
            out = "npe";
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(label + " " + out);
    }

    public static void main(String[] args) {
        Module base = Object.class.getModule();
        Module unnamed = I45xModuleQualifiedNullTarget.class.getModule();
        Module other = unnamed;
        row("base isExported(pn, null)", () -> base.isExported("java.lang", null));
        row("base isOpen(pn, null)", () -> base.isOpen("java.lang", null));
        row("base isExported(null, other)", () -> base.isExported(null, other));
        row("base isOpen(null, other)", () -> base.isOpen(null, other));
        row("base isExported(null, null)", () -> base.isExported(null, null));
        row("unnamed isExported(pn, null)", () -> unnamed.isExported("x", null));
        row("unnamed isOpen(pn, null)", () -> unnamed.isOpen("x", null));
        // Controls: a non-null target still answers a boolean.
        row("base isExported(pn, other)", () -> base.isExported("jdk.internal.misc", other));
        row("base isOpen(pn, other)", () -> base.isOpen("java.lang", other));
    }
}
