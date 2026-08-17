// Mirrors the JUnit Annotatable shape that miscompiles:
//   interface with an abstract method
//   an ABSTRACT class implementing the interface WITHOUT declaring the method
//   several concrete receivers, so the call site goes MIC -> PIC -> megamorphic
public class IfaceMegaProbe {
    interface Ann { int[] get(); }

    // implements the interface but does NOT declare get() -- subclasses do.
    abstract static class Member implements Ann { }

    static class M1 extends Member { public int[] get() { return new int[1]; } }
    static class M2 extends Member { public int[] get() { return new int[2]; } }
    static class Direct implements Ann { public int[] get() { return new int[3]; } }
    static class Direct2 implements Ann { public int[] get() { return new int[4]; } }
    static class Direct3 implements Ann { public int[] get() { return new int[5]; } }

    // generic, exactly like AnnotatableValidator<T extends Annotatable>:
    // erasure makes this an invokeinterface on Ann
    static <T extends Ann> int viaIface(T a) { return a.get().length; }

    public static void main(String[] args) {
        Ann[] rs = { new M1(), new M2(), new Direct(), new Direct2(), new Direct3() };
        long sum = 0;
        for (int i = 0; i < 400000; i++) {
            sum += viaIface(rs[i % rs.length]);
        }
        System.out.println("sum=" + sum + " (expect " + (400000L/5*(1+2+3+4+5)) + ")");
    }
}
