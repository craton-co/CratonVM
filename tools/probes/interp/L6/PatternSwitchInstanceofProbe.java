/*
 * Interpreter round i1 wave 3, lane L6: a pattern `switch` type label must answer
 * exactly what `instanceof` answers (JLS 14.30.2), and an enum `switch` that mixes
 * constant labels with a type pattern must reach the type pattern.
 *
 * javac compiles the `switch (o)` below to `SwitchBootstraps.typeSwitch` and the
 * `switch (e)` to `SwitchBootstraps.enumSwitch` with a `Class` label beside the
 * `"A"` label (check with `javap -v`). Each object line prints the label the
 * switch took and the label an equivalent `instanceof` chain takes; they must be
 * equal on every line.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   String[]: switch=Object[] instanceof=Object[]
 *   int[]: switch=int[] instanceof=int[]
 *   lambda: switch=Runnable instanceof=Runnable
 *   proxy: switch=Runnable instanceof=Runnable
 *   List.of(): switch=AbstractCollection instanceof=AbstractCollection
 *   emptyList(): switch=AbstractCollection instanceof=AbstractCollection
 *   unmodifiableList: switch=List instanceof=List
 *   string: switch=CharSequence instanceof=CharSequence
 *   enum: A=1 B=2 C=2
 *   enum from 1: A=3
 *
 * CratonVM before wave 3: the enum lines printed `B=3 C=3` (a `Class` label was
 * read as the name ""), and the object lines could disagree for receivers the
 * `instanceof` opcode admits through a fallback (display classes of
 * immutable/unmodifiable collections, `$Proxy` instances).
 */
import java.lang.reflect.Proxy;
import java.util.AbstractCollection;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public class PatternSwitchInstanceofProbe {
    enum E { A, B, C }

    static String bySwitch(Object o) {
        return switch (o) {
            case Object[] a -> "Object[]";
            case int[] a -> "int[]";
            case Runnable r -> "Runnable";
            case AbstractCollection<?> c -> "AbstractCollection";
            case List<?> l -> "List";
            case CharSequence s -> "CharSequence";
            default -> "Object";
        };
    }

    static String byInstanceof(Object o) {
        if (o instanceof Object[]) return "Object[]";
        if (o instanceof int[]) return "int[]";
        if (o instanceof Runnable) return "Runnable";
        if (o instanceof AbstractCollection<?>) return "AbstractCollection";
        if (o instanceof List<?>) return "List";
        if (o instanceof CharSequence) return "CharSequence";
        return "Object";
    }

    static int enumSwitch(E e) {
        return switch (e) {
            case A -> 1;
            case E x when x.ordinal() > 0 -> 2;
            default -> 3;
        };
    }

    static void line(String label, Object o) {
        System.out.println(label + ": switch=" + bySwitch(o) + " instanceof=" + byInstanceof(o));
    }

    public static void main(String[] args) {
        line("String[]", new String[0]);
        line("int[]", new int[0]);
        Runnable lambda = () -> { };
        line("lambda", lambda);
        Runnable proxy = (Runnable) Proxy.newProxyInstance(
                PatternSwitchInstanceofProbe.class.getClassLoader(),
                new Class<?>[] {Runnable.class},
                (p, m, a) -> null);
        line("proxy", proxy);
        line("List.of()", List.of());
        line("emptyList()", Collections.emptyList());
        line("unmodifiableList", Collections.unmodifiableList(new ArrayList<>()));
        line("string", "s");
        System.out.println("enum: A=" + enumSwitch(E.A) + " B=" + enumSwitch(E.B) + " C=" + enumSwitch(E.C));
        // A guard that fails re-enters the switch from the next label (the
        // bootstrap's `startIndex`): the `Class` label matches `A` at index 0,
        // its guard fails, `"B"` at index 1 does not match, so `A` takes the
        // default arm.
        System.out.println("enum from 1: A=" + guardFails(E.A));
    }

    static int guardFails(E e) {
        return switch (e) {
            case E x when x.ordinal() > 5 -> 2;
            case B -> 4;
            default -> 3;
        };
    }
}
