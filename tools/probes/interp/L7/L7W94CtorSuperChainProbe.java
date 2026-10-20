// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L7: semantics of the frameless
// constructor call over a super chain (`CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN`,
// default off). Every row must print the same with the switch off and on:
//
//   chain       `new D3()` (D3 -> D2 -> D1 -> Object, default constructors):
//               the object is a D3, a D2 and a D1
//   side-effect a default constructor over a NON-trivial superclass
//               constructor (M1() { count++; }) still runs it: 1000
//   field       a field-store constructor over a one-link chain stores its
//               parameter (F2(int x) { this.x = x; })
//   jdk-chain   a subclass of java.util.AbstractCollection (whose constructor
//               is `protected AbstractCollection() {}`) works as a collection
//   jdk-field   a subclass of java.util.AbstractList, whose constructor
//               initialises `modCount = 0` explicitly (not trivial): modCount
//               starts at 0 and counts
//   soe         constructing at the stack limit still throws StackOverflowError
//
// Run (both modes, --nojit and with the JIT, switch off and on):
//   javac -d out L7W94CtorSuperChainProbe.java
//   java -cp out L7W94CtorSuperChainProbe
//   cratonvm --jdk-only --nojit -cp out L7W94CtorSuperChainProbe
//   CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN=1 cratonvm --jdk-only --nojit -cp out L7W94CtorSuperChainProbe
//   (and --compatible)
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// chain true true true L7W94CtorSuperChainProbe$D3
// side-effect 1000
// field 499500
// jdk-chain 3 true
// jdk-field 0 1
// soe true
// HOTSPOT_EXPECTED_END

import java.util.AbstractCollection;
import java.util.AbstractList;
import java.util.Iterator;
import java.util.NoSuchElementException;

public class L7W94CtorSuperChainProbe {
    static class D1 {}

    static class D2 extends D1 {}

    static class D3 extends D2 {}

    static int count;

    static class M1 {
        M1() {
            count++;
        }
    }

    static class M2 extends M1 {}

    static class M3 extends M2 {}

    static class F2 extends D1 {
        int x;

        F2(int x) {
            this.x = x;
        }
    }

    static final class Three extends AbstractCollection<Integer> {
        @Override
        public Iterator<Integer> iterator() {
            return new Iterator<Integer>() {
                int i;

                @Override
                public boolean hasNext() {
                    return i < 3;
                }

                @Override
                public Integer next() {
                    if (i >= 3) {
                        throw new NoSuchElementException();
                    }
                    return i++;
                }
            };
        }

        @Override
        public int size() {
            return 3;
        }
    }

    static final class Counted extends AbstractList<Integer> {
        @Override
        public Integer get(int index) {
            return index;
        }

        @Override
        public int size() {
            return 0;
        }

        int mods() {
            return modCount;
        }

        void bump() {
            modCount++;
        }
    }

    static int deep(int n) {
        new D3();
        return deep(n + 1) + 1;
    }

    public static void main(String[] args) {
        Object o = null;
        for (int i = 0; i < 1000; i++) {
            o = new D3();
        }
        System.out.println("chain " + (o instanceof D3) + " " + (o instanceof D2) + " "
                + (o instanceof D1) + " " + o.getClass().getName());
        for (int i = 0; i < 1000; i++) {
            new M3();
        }
        System.out.println("side-effect " + count);
        long sum = 0;
        for (int i = 0; i < 1000; i++) {
            sum += new F2(i).x;
        }
        System.out.println("field " + sum);
        Three three = null;
        for (int i = 0; i < 1000; i++) {
            three = new Three();
        }
        System.out.println("jdk-chain " + three.size() + " " + three.contains(2));
        Counted counted = null;
        for (int i = 0; i < 1000; i++) {
            counted = new Counted();
        }
        int before = counted.mods();
        counted.bump();
        System.out.println("jdk-field " + before + " " + counted.mods());
        boolean soe = false;
        try {
            deep(0);
        } catch (StackOverflowError e) {
            soe = true;
        }
        System.out.println("soe " + soe);
    }
}
