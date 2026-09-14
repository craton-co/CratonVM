package cratonvm;
// Synthetic-mode (no real JDK) reproducer for the JIT null-receiver swallow:
// a JIT-compiled method that invokes an instance method on a null receiver
// must throw NullPointerException, exactly like the interpreter.
public class JitNull {
    static final class Box { int v; int get() { return v; } }

    // Warmed callsite -> monomorphic inline cache (jit_invoke_virtual_mic).
    static int warmed(Box b) { return b.get(); }
    // Second-call callsite is cold in JIT'd code -> jit_invoke_dispatch.
    static int coldDispatch(Box b) { return b.get(); }

    public static void main(String[] args) {
        Box real = new Box(); real.v = 7;
        long acc = 0;
        for (int i = 0; i < 300; i++) acc += warmed(real); // warm -> JIT + MIC
        String r1;
        try { warmed(null); r1 = "RET"; } catch (NullPointerException e) { r1 = "NPE"; }

        coldDispatch(real);                                 // 1st call: schedules JIT
        String r2;
        try { coldDispatch(null); r2 = "RET"; } catch (NullPointerException e) { r2 = "NPE"; }

        System.out.println("warmed-null=" + r1);
        System.out.println("cold-null=" + r2);
        System.out.println("acc=" + acc);
        System.out.println("JIT_NULLRECV_OK");
    }
}
