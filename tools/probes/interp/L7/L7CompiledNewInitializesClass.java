// Interpreter round i1 wave 3, lane L7 -- a `new` executed by COMPILED code
// must still run the class's <clinit> first (JVMS 5.5), even when the class
// was already LOADED but not initialized when the method was compiled.
//
// `new Holder[1]` (anewarray) and `Holder2.class` (ldc of a class) both load a
// class without initializing it. `make`/`make2` run 200 000 times on the
// branch that never executes `new`, so they are compiled while their `new`
// target is loaded and uninitialized; then the `new` branch runs once. The
// compiled `new` lowerings (the single-pass inline TLAB bump, the optimizing
// tier's `Op::New` bump) carry no class-init check -- only the `jit_new_object`
// slow-path helper has one -- and the VM's `new`-site resolver answers
// `Resolved` for any LOADED class. Fixed in wave 4 (such a site now compiles
// to the initializing CP-indexed helper); see
// docs/internal/fixed-bugs/interpreter-L7-compiled-new-of-an-uninitialized-class-skips-clinit-FIXED-20260923.md.
//
// Expected (HotSpot 25, `java L7CompiledNewInitializesClass`), exactly:
//
//   before inits=0 inits2=0
//   Holder.<clinit>
//   after inits=1 v=7
//   Holder2.<clinit>
//   after2 inits2=1 w=9
//
// Compare with `cratonvm L7CompiledNewInitializesClass` (default flags) and
// with --nojit. The loop count only has to exceed the tier-up threshold.

public class L7CompiledNewInitializesClass {
    static int inits;
    static int inits2;

    static class Holder {
        static {
            inits++;
            System.out.println("Holder.<clinit>");
        }
        int v = 7;
    }

    static class Holder2 {
        static {
            inits2++;
            System.out.println("Holder2.<clinit>");
        }
        int w = 9;
    }

    static Object make(boolean really) {
        Holder[] arr = new Holder[1]; // loads Holder, does not initialize it
        if (really) {
            return new Holder(); // must initialize Holder first
        }
        return arr;
    }

    static Object make2(boolean really) {
        Class<?> c = Holder2.class; // loads Holder2, does not initialize it
        if (really) {
            return new Holder2();
        }
        return c;
    }

    public static void main(String[] args) {
        int sink = 0;
        for (int i = 0; i < 200_000; i++) {
            if (make(false) == null) sink++;
            if (make2(false) == null) sink++;
        }
        System.out.println("before inits=" + inits + " inits2=" + inits2 + (sink == 0 ? "" : " ?"));
        Holder h = (Holder) make(true);
        System.out.println("after inits=" + inits + " v=" + h.v);
        Holder2 h2 = (Holder2) make2(true);
        System.out.println("after2 inits2=" + inits2 + " w=" + h2.w);
    }
}
