import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

// Regression probe for the meta-aware VarHandle atomic ops. A VarHandle built
// by a genuine `findVarHandle` does NOT carry CratonVM's synthetic 6-field
// layout, so the native ops must resolve kind/field via the meta side-table.
// Covers: getAndBitwiseOr/And/Xor (delivered by d6cefc7), getAndAdd (the
// continue_prompt_varhandle_getandadd_meta fix), and compareAndSet. JDK-only,
// gate-independent, deterministic.
public class VarHandleBitwiseProbe {
    volatile int n;
    static final VarHandle N;
    static {
        try {
            N = MethodHandles.lookup().findVarHandle(VarHandleBitwiseProbe.class, "n", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    public static void main(String[] args) {
        VarHandleBitwiseProbe o = new VarHandleBitwiseProbe();
        o.n = 0b1100; // 12

        int orOld = (int) N.getAndBitwiseOr(o, 0b0011);
        System.out.println("or old=" + orOld + " now=" + o.n);   // old=12 now=15

        int andOld = (int) N.getAndBitwiseAnd(o, 0b0110);
        System.out.println("and old=" + andOld + " now=" + o.n); // old=15 now=6

        int xorOld = (int) N.getAndBitwiseXor(o, 0b0101);
        System.out.println("xor old=" + xorOld + " now=" + o.n); // old=6 now=3

        int addOld = (int) N.getAndAdd(o, 5);
        System.out.println("add old=" + addOld + " now=" + o.n); // old=3 now=8

        boolean cas1 = N.compareAndSet(o, 8, 100);
        System.out.println("cas1=" + cas1 + " now=" + o.n);       // true now=100

        boolean cas2 = N.compareAndSet(o, 8, 200);
        System.out.println("cas2=" + cas2 + " now=" + o.n);       // false now=100

        System.out.println("OK");
    }
}
