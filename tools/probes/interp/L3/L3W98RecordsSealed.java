import java.lang.reflect.*;
import java.util.*;

/**
 * Interpreter round i4 wave 98, lane L3: records and sealed hierarchies as
 * HotSpot 25 reports them -- getRecordComponents order and shape, isRecord of
 * a record-like non-record and of an enum, record toString/equals/hashCode of
 * primitives/arrays, getPermittedSubclasses order (PermittedSubclasses
 * attribute order) and null for a non-sealed class, isSealed of an interface,
 * a non-sealed subclass, and Class.getModifiers of sealed / non-sealed
 * (there is no flag for either).
 */
public class L3W98RecordsSealed {
    sealed interface Shape permits Circle, Square, Poly {}
    record Circle(double r) implements Shape {}
    static final class Square implements Shape {}
    static non-sealed class Poly implements Shape {}
    static class Tri extends Poly {}

    static sealed abstract class Expr permits Expr.Lit, Expr.Neg, Expr.Add {
        static final class Lit extends Expr {}
        static final class Neg extends Expr {}
        static final class Add extends Expr {}
    }

    sealed interface Implicit {}
    final class ImplA implements Implicit {}
    record ImplB() implements Implicit {}

    record Empty() {}
    record Mixed(byte b, short s, char c, int i, long l, float f, double d, boolean z, String str, int[] arr, Object o) {}
    record Gen<T extends Number>(T value, List<? super T> sink) {}

    public static void main(String[] args) throws Exception {
        for (Class<?> c : List.of(Shape.class, Circle.class, Square.class, Poly.class, Tri.class, Expr.class,
                Expr.Lit.class, Implicit.class, ImplA.class, ImplB.class, Empty.class, Object.class, Record.class,
                Thread.State.class)) {
            Class<?>[] p = c.getPermittedSubclasses();
            System.out.println(c.getSimpleName() + ": sealed=" + c.isSealed() + " record=" + c.isRecord()
                    + " mods=" + Integer.toHexString(c.getModifiers())
                    + " permitted=" + (p == null ? "null" : names(p))
                    + " components=" + (c.getRecordComponents() == null ? "null" : c.getRecordComponents().length));
        }
        for (Class<?> c : List.of(Mixed.class, Gen.class, Empty.class)) {
            System.out.println(c.getSimpleName() + " components:");
            for (RecordComponent rc : c.getRecordComponents()) {
                System.out.println("  " + rc.getName() + " type=" + rc.getType().getTypeName() + " generic=" + rc.getGenericType()
                        + " sig=" + rc.getGenericSignature() + " decl=" + rc.getDeclaringRecord().getSimpleName()
                        + " accessor=" + rc.getAccessor().toGenericString() + " toString=" + rc);
            }
            List<String> ctors = new ArrayList<>();
            for (Constructor<?> k : c.getDeclaredConstructors()) ctors.add(k.toGenericString() + " mods=" + Integer.toHexString(k.getModifiers()));
            System.out.println("  ctors=" + ctors);
            List<String> ms = new ArrayList<>();
            for (Method m : c.getDeclaredMethods()) ms.add(m.getName() + Integer.toHexString(m.getModifiers()));
            Collections.sort(ms);
            System.out.println("  methods=" + ms);
        }
        Mixed m = new Mixed((byte) 1, (short) 2, 'c', 4, 5L, 6.5f, 7.25, true, "s", null, null);
        System.out.println("toString: " + m);
        System.out.println("equals: " + m.equals(new Mixed((byte) 1, (short) 2, 'c', 4, 5L, 6.5f, 7.25, true, "s", null, null)));
        System.out.println("hash: " + m.hashCode());
        System.out.println("NaN equals: " + new Circle(Double.NaN).equals(new Circle(Double.NaN))
                + " -0.0 equals: " + new Circle(-0.0).equals(new Circle(0.0)));
        System.out.println("empty: " + new Empty() + " hash=" + new Empty().hashCode());
        System.out.println("gen: " + new Gen<>(3, new ArrayList<Object>()));
        try {
            Circle.class.getRecordComponents()[0].getAccessor().invoke(null);
        } catch (Exception e) {
            System.out.println("accessor on null: " + e.getClass().getName() + ": " + e.getMessage());
        }
        System.out.println("superclass of Circle: " + Circle.class.getSuperclass().getName());
    }

    static String names(Class<?>[] cs) {
        List<String> out = new ArrayList<>();
        for (Class<?> c : cs) out.add(c.getSimpleName());
        return out.toString(); // attribute order, not sorted
    }
}
