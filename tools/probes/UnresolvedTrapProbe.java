// Does the unresolved-class uncommon trap SELF-HEAL when the class later loads?
//
// `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` is opt-in and OFF, and the reason it
// is off is that nothing had ever fired one. The argument for turning it on was
// read off the source -- `Op::Guard` bakes `DeoptReason::UncommonTrap`, and
// `recommend_action` gives that reason `Reinterpret` on the first deopt and
// `RecompileAndReinterpret` on the second, so the method should be recompiled
// without the trap once the class exists. That is a claim about a policy
// function, not a measurement. This probe fires one.
//
// The shape is the awkward part. For the trap to be planted the class must be
// UNLOADED when the method compiles, but a method can only get hot by running
// -- and running the cast would load the class. So the cast sits behind a
// parameter that is false during warm-up:
//
//     hot(o, false) x N     -> compiles; Shape unloaded; trap planted
//     hot(o, true)  x M     -> Shape loads here, the trap is on a LIVE path
//
// What to look for, with CRATONVM_DBG_DEOPT=1:
//
//   self-healing : a handful of deopt lines, then silence -- the recompile
//                  came back with the class resolved and no trap.
//   NOT healing  : deopt traffic proportional to M. Correctness still holds
//                  (the interpreter finishes the bytecode), so the FAILURE
//                  MODE HERE IS THROUGHPUT, NOT A WRONG ANSWER -- which is
//                  exactly why a correctness-only probe could not settle this.
//
// The answer counter below is the correctness half; the deopt log is the half
// that decides the flag.
public class UnresolvedTrapProbe {

    interface Shape {
        int area();
    }

    static class Square implements Shape {
        private final int side;
        Square(int side) { this.side = side; }
        public int area() { return side * side; }
    }

    // The method under test. The `checkcast` at the cast is the site that gets
    // an uncommon trap when `Shape` is not yet loaded.
    static int hot(Object o, boolean doCast) {
        if (doCast) {
            Shape s = (Shape) o;
            return s.area();
        }
        return 1;
    }

    public static void main(String[] args) throws Exception {
        final int WARM = 200000;
        final int LIVE = 200000;

        // Phase 1: make `hot` hot WITHOUT ever loading Shape or Square.
        // `o` is a plain Object, and doCast is false, so the cast never runs.
        Object dummy = new Object();
        long acc = 0;
        for (int i = 0; i < WARM; i++) acc += hot(dummy, false);
        if (acc != WARM) {
            System.out.println("FAIL warm-up sum " + acc);
        }
        System.out.println("[probe] warm-up done, Shape still unloaded");

        // Phase 2: load Shape/Square and drive the trapped path hard.
        Object sq = new Square(7);
        long live = 0;
        for (int i = 0; i < LIVE; i++) live += hot(sq, true);

        boolean ok = live == 49L * LIVE;
        System.out.println("[probe] live sum " + (ok ? "OK" : "WRONG " + live));
        System.out.println(ok ? "UNRESOLVED TRAP PROBE OK" : "UNRESOLVED TRAP PROBE FAILED");
    }
}
