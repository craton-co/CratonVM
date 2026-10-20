/*
 * Interpreter round i1 wave 4, lane L6: a qualified enum constant label in a
 * pattern switch whose selector is not the enum type (`case E.A`, JEP 441).
 * javac links it through `SwitchBootstraps.typeSwitch` with an `EnumDesc`
 * label, a CONSTANT_Dynamic over `ConstantBootstraps.invoke(EnumDesc.of, ...)`.
 * CratonVM before wave 4 read every condy label as a primitive-class pattern,
 * so `E.A` never matched and fell to the `case E e` arm (2 instead of 1).
 * `C` has a constant body (its class is a subclass of `E`); it must still
 * match `E.C`. An `Object` selector also sees an `E[]` target.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   f(A)=1 f(B)=2 f(C)=4 f(R)=3
 *   g(A)=1 g(C)=2 g(arr)=0 g(str)=0
 */
public class TypeSwitchEnumDescProbe {
    sealed interface S permits E, R {}

    enum E implements S {
        A,
        B,
        C {
            @Override
            public String toString() {
                return "c";
            }
        }
    }

    record R() implements S {}

    static int f(S s) {
        return switch (s) {
            case E.A -> 1;
            case E.C -> 4;
            case E e -> 2;
            case R r -> 3;
        };
    }

    static int g(Object o) {
        return switch (o) {
            case E.A -> 1;
            case E.C -> 2;
            default -> 0;
        };
    }

    public static void main(String[] args) {
        System.out.println("f(A)=" + f(E.A) + " f(B)=" + f(E.B) + " f(C)=" + f(E.C)
                + " f(R)=" + f(new R()));
        System.out.println("g(A)=" + g(E.A) + " g(C)=" + g(E.C) + " g(arr)=" + g(new E[] {E.A})
                + " g(str)=" + g("A"));
    }
}
