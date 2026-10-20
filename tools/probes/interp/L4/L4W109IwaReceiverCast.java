// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 109, lane L4 (hunt): `invokeWithArguments` casts
// the receiver to the HANDLE's receiver type, `type().parameterType(0)`:
// `refc` for `findVirtual(refc, ..)` of an inherited method, and the lookup
// class for a protected member found from a subclass in another package
// (`restrictReceiver`). CratonVM's served direct lane (`mh_real_iwa_direct`)
// judged the receiver against the member's DECLARING class, so an instance of
// that class which is not of the handle's receiver type was called where
// HotSpot raises `ClassCastException` from the `asType` adapter.
//
// Rows (each called three times, so warm lanes are reached too):
//   inherited-ok    findVirtual(Sub, "m") on a Sub
//   inherited-cce   the same handle on a plain Super
//   protected-ok    findVirtual(AbstractList, "removeRange") from Mine, on a Mine
//   protected-cce   the same handle on an ArrayList (an AbstractList, not a Mine)
//   list-after      the ArrayList's contents after the refused calls
//
// Run: javac -d . L4W109IwaReceiverCast.java
//      java -cp . L4W109IwaReceiverCast                     (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W109IwaReceiverCast
//      cratonvm --compatible [--nojit] -cp . L4W109IwaReceiverCast
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// inherited-ok 1 1 1
// inherited-cce java.lang.ClassCastException: Cannot cast L4W109IwaReceiverCast$Super to L4W109IwaReceiverCast$Sub (x3)
// protected-ok [c] [c] [c]
// protected-cce java.lang.ClassCastException: Cannot cast java.util.ArrayList to L4W109IwaReceiverCast$Mine (x3)
// list-after [a, b, c]
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.AbstractList;
import java.util.ArrayList;
import java.util.List;

public class L4W109IwaReceiverCast {
    public static class Super {
        public int m() {
            return 1;
        }
    }

    public static class Sub extends Super {
    }

    public static class Mine extends AbstractList<Object> {
        final ArrayList<Object> items = new ArrayList<>();

        @Override
        public Object get(int i) {
            return items.get(i);
        }

        @Override
        public int size() {
            return items.size();
        }

        @Override
        public Object remove(int i) {
            return items.remove(i);
        }

        static MethodHandle removeRange() throws Throwable {
            // A protected method of a superclass in another package, found
            // from this subclass: the handle's receiver is restricted to Mine.
            return MethodHandles.lookup().findVirtual(AbstractList.class, "removeRange",
                    MethodType.methodType(void.class, int.class, int.class));
        }
    }

    static String failures(MethodHandle h, Object receiver, Object... rest) {
        String first = null;
        int same = 0;
        for (int i = 0; i < 3; i++) {
            Object[] args = new Object[rest.length + 1];
            args[0] = receiver;
            System.arraycopy(rest, 0, args, 1, rest.length);
            String got;
            try {
                got = "returned " + h.invokeWithArguments(args);
            } catch (Throwable t) {
                got = t.getClass().getName() + ": " + t.getMessage();
            }
            if (first == null) {
                first = got;
            }
            if (got.equals(first)) {
                same++;
            }
        }
        return first + " (x" + same + ")";
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle m = MethodHandles.lookup().findVirtual(Sub.class, "m",
                MethodType.methodType(int.class));
        StringBuilder ok = new StringBuilder("inherited-ok");
        for (int i = 0; i < 3; i++) {
            ok.append(' ').append(m.invokeWithArguments(new Sub()));
        }
        System.out.println(ok);
        System.out.println("inherited-cce " + failures(m, new Super()));

        MethodHandle rr = Mine.removeRange();
        StringBuilder pok = new StringBuilder("protected-ok");
        for (int i = 0; i < 3; i++) {
            Mine mine = new Mine();
            mine.items.addAll(List.of("a", "b", "c"));
            rr.invokeWithArguments(mine, 0, 2);
            pok.append(' ').append(mine.items);
        }
        System.out.println(pok);
        ArrayList<Object> plain = new ArrayList<>(List.of("a", "b", "c"));
        System.out.println("protected-cce " + failures(rr, plain, 0, 1));
        System.out.println("list-after " + plain);
    }
}
