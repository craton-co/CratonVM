// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i4 wave 96, lane L2
// (`interpreter-i86-L2-synthetic-serialization-stand-in-is-not-hotspots-RETIRED-20261015.md`).
//
// The DEFAULT serialVersionUID (`ObjectStreamClass.computeDefaultSUID`) of
// shapes L2W86SerialVersionUid does not cover. In the shipping builds the
// JDK's own bytecode computes it, so each row checks the reflection answers it
// hashes (`Class.getModifiers` of a member class and of arrays, declared
// members incl. a javac bridge, `Arrays.sort` of names by UTF-16 code units);
// under `experimental-serialization` / `synthetic-jdk` the stand-in's
// `serialization.rs` `default_svuid_of_shape` computes it and must agree.
//
//   array-string / array-int  the values every serialized String[] / int[]
//       carries (0xADD256E7E91D7B47, 0x4DBA602676EAB2A5)
//   array-prot   array of a protected member class: FINAL|ABSTRACT|PROTECTED,
//                masked to FINAL|ABSTRACT
//   prot         protected static member class (class file says PUBLIC, the
//                InnerClasses entry PROTECTED|STATIC), protected default ctor
//   nometh       member interface with no methods (ABSTRACT removed)
//   unicode      fields U+FF41, U+10400 and "caf" + U+00E9: the UTF-16 order
//                puts the surrogate pair before U+FF41, code point order
//                after; writeUTF's 2- and 3-byte forms
//   bridge       Comparable<Cmp>: compareTo(Cmp) and javac's public bridge
//                compareTo(Object) (0x1041 masked to PUBLIC), two interfaces
//   mixed        <clinit>, static / private static / private transient /
//                volatile / protected final / private fields, synchronized,
//                static native, overloads, a private and a public ctor
//   explicit-int `static final int serialVersionUID = 7` (getLong widens)
//
// The values were derived outside any JVM from the JDK 25 algorithm over the
// members javac 25 emits for these classes (the same derivation reproduces
// L2W86SerialVersionUid's three HotSpot-verified values and the two well-known
// array values). Not derived with certainty: `explicit-int` (that
// `getDeclaredSUID` reads an int field with `Field.getLong` and no type check),
// and javac's member set (default-constructor access = class access, JLS
// 8.8.9; bridge flags 0x1041).
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W96DefaultSuidShapes.java
//   $B --java-home $J/.. -cp out L2W96DefaultSuidShapes
//   $B --compatible --java-home $J/.. -cp out L2W96DefaultSuidShapes
//   $J/java -cp out L2W96DefaultSuidShapes
//
// HOTSPOT_EXPECTED_BEGIN
// array-string -5921575005990323385
// array-int 5600894804908749477
// array-prot 8018541982324586844
// prot 8607344207191864123
// nometh 2656638215363774794
// unicode -5292117238798028919
// bridge 1005040473018350497
// mixed -6069016023722827641
// explicit-int 7
// HOTSPOT_EXPECTED_END
import java.io.ObjectStreamClass;
import java.io.Serializable;

public class L2W96DefaultSuidShapes {
    protected static class Prot implements Serializable {
        public int v;
    }

    interface NoMeth extends Serializable {
    }

    static final class Uni implements Serializable {
        int ａ;
        int 𐐀;
        long café;
    }

    static class Cmp implements Comparable<Cmp>, Serializable {
        public int compareTo(Cmp o) {
            return 0;
        }
    }

    static class Mixed implements Serializable {
        static int X = 3;
        private static int hidden;
        private transient int t;
        volatile int vol;
        protected final int fin = 1;
        private int p;

        private Mixed(int x) {
        }

        public Mixed() {
        }

        synchronized void sync() {
        }

        static native void nat();

        final void a(int x) {
        }

        void a(long x) {
        }
    }

    static final class ExplicitInt implements Serializable {
        static final int serialVersionUID = 7;
    }

    static long suid(Class<?> c) {
        return ObjectStreamClass.lookup(c).getSerialVersionUID();
    }

    public static void main(String[] args) {
        System.out.println("array-string " + suid(String[].class));
        System.out.println("array-int " + suid(int[].class));
        System.out.println("array-prot " + suid(Prot[].class));
        System.out.println("prot " + suid(Prot.class));
        System.out.println("nometh " + suid(NoMeth.class));
        System.out.println("unicode " + suid(Uni.class));
        System.out.println("bridge " + suid(Cmp.class));
        System.out.println("mixed " + suid(Mixed.class));
        System.out.println("explicit-int " + suid(ExplicitInt.class));
    }
}
