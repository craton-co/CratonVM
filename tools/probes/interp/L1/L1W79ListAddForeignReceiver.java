// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L1:
// docs/internal/fixed-bugs/interpreter-i74-L1-list-add-interface-native-writes-arraylist-slots-into-a-foreign-receiver-FIXED-20261111.md
//
// `native-collections` registers the ArrayList `add` natives on the abstract
// `java/util/List.add(Object)` / `add(int, Object)` declarations. A door that
// resolves the INTERFACE declaration (reflection on `List.class`, a method
// handle found on `List`) can hand them a non-ArrayList receiver; before wave
// 79 they stored a fresh one-element `Object[]` into the receiver's field 1
// (a `CopyOnWriteArrayList`'s `array`) and lost every earlier element. Each
// row below reaches `add` on a foreign list through the interface declaration
// and prints the list afterwards.
//
// Run: `cratonvm L1W79ListAddForeignReceiver` (jdk-only) and
// `cratonvm --compatible L1W79ListAddForeignReceiver`; both must print the
// block below.
//
// The expected block is DERIVED from the JDK 25 sources (CopyOnWriteArrayList
// and LinkedList `outOfBounds` / `outOfBoundsMsg` read "Index: i, Size: s";
// AbstractList.add(int, E) throws a message-less UnsupportedOperationException).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// reflect cowal add(Object): r=true [b, x]
// reflect cowal add(int,Object): [a, b, x]
// reflect cowal add(9,Object): java.lang.IndexOutOfBoundsException: Index: 9, Size: 3 list=[a, b, x]
// mh cowal add(Object): r=true [a, b, x, y]
// mh cowal add(int,Object): [a, z, b, x, y]
// reflect linked add(Object): r=true [b, x]
// mh linked add(int,Object): [a, b, x]
// reflect linked add(9,Object): java.lang.IndexOutOfBoundsException: Index: 9, Size: 3 list=[a, b, x]
// reflect fixed add(Object): java.lang.UnsupportedOperationException: null list=[f]
// done
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.AbstractList;
import java.util.LinkedList;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;

public class L1W79ListAddForeignReceiver {
    /** A fixed-size list: inherits AbstractList's throwing add. */
    static final class Fixed extends AbstractList<String> {
        @Override
        public String get(int index) {
            if (index != 0) {
                throw new IndexOutOfBoundsException();
            }
            return "f";
        }

        @Override
        public int size() {
            return 1;
        }
    }

    static String cause(Throwable t) {
        Throwable c = t instanceof InvocationTargetException ? t.getCause() : t;
        return c.getClass().getName() + ": " + c.getMessage();
    }

    public static void main(String[] args) throws Throwable {
        Method add1 = List.class.getMethod("add", Object.class);
        Method add2 = List.class.getMethod("add", int.class, Object.class);
        MethodHandles.Lookup lookup = MethodHandles.publicLookup();
        MethodHandle mh1 = lookup.findVirtual(List.class, "add",
                MethodType.methodType(boolean.class, Object.class));
        MethodHandle mh2 = lookup.findVirtual(List.class, "add",
                MethodType.methodType(void.class, int.class, Object.class));

        List<String> cow = new CopyOnWriteArrayList<>(List.of("b"));
        Object r = add1.invoke(cow, "x");
        System.out.println("reflect cowal add(Object): r=" + r + " " + cow);
        add2.invoke(cow, 0, "a");
        System.out.println("reflect cowal add(int,Object): " + cow);
        try {
            add2.invoke(cow, 9, "q");
            System.out.println("reflect cowal add(9,Object): no exception list=" + cow);
        } catch (InvocationTargetException e) {
            System.out.println("reflect cowal add(9,Object): " + cause(e) + " list=" + cow);
        }
        boolean b = (boolean) mh1.invoke(cow, "y");
        System.out.println("mh cowal add(Object): r=" + b + " " + cow);
        mh2.invoke(cow, 1, "z");
        System.out.println("mh cowal add(int,Object): " + cow);

        List<String> linked = new LinkedList<>(List.of("b"));
        r = add1.invoke(linked, "x");
        System.out.println("reflect linked add(Object): r=" + r + " " + linked);
        mh2.invoke(linked, 0, "a");
        System.out.println("mh linked add(int,Object): " + linked);
        try {
            add2.invoke(linked, 9, "q");
            System.out.println("reflect linked add(9,Object): no exception list=" + linked);
        } catch (InvocationTargetException e) {
            System.out.println("reflect linked add(9,Object): " + cause(e) + " list=" + linked);
        }

        List<String> fixed = new Fixed();
        try {
            add1.invoke(fixed, "q");
            System.out.println("reflect fixed add(Object): no exception list=" + fixed);
        } catch (InvocationTargetException e) {
            System.out.println("reflect fixed add(Object): " + cause(e) + " list=" + fixed);
        }
        System.out.println("done");
    }
}
