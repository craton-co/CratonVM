// Interpreter round i1 wave 22, lane L5 — timing probe (A/B), no behaviour change.
//
// What it times: compiled code whose static accesses and allocations go
// through the JIT's "class is initialized" memo (`jit::helpers::class_init_memo`,
// now per VM: `ClassRealm::class_init_bits`, published Release / read Acquire),
// and a burst of class definitions, whose ClassLoad hook now names its VM
// (`runtime::jvmti::fire_class_load_for_vm` instead of the unattributed row).
// In a one-VM process both changes should be neutral; the win is for a second
// VM in the same process, which a Java probe cannot show (the Rust tests
// `vm_init::tests::a_class_load_reaches_only_the_vm_that_defined_the_class`
// and `jit::helpers::tests::class_init_memo_is_per_vm_and_every_vm_has_one`
// cover that).
//
// Run (JIT on, the default tiering), before/after builds interleaved:
//   cratonvm -cp <dir> L5W22StaticInitMemoBench
//   cratonvm --compatible -cp <dir> L5W22StaticInitMemoBench
// Rows to compare: `statics ns/iter`, `new ns/alloc`, `define us/class`.
// Expected direction: unchanged (within noise) after/before.
//
// HotSpot 25 prints (timings vary; the checksum lines are exact):
//   statics checksum=12499997500000
//   new checksum=20000000
//   define classes=200
// followed by the three timing rows.

import java.util.ArrayList;
import java.util.List;

public class L5W22StaticInitMemoBench {
    static final class Holder {
        static long counter;
        static int bump = 1;
    }

    static final class Box {
        final int v;

        Box(int v) {
            this.v = v;
        }
    }

    static long statics(int n) {
        for (int i = 0; i < n; i++) {
            Holder.counter += (long) i * Holder.bump;
        }
        return Holder.counter;
    }

    static long allocs(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += new Box(2).v;
        }
        return sum;
    }

    /** A class file with no members: `name extends java/lang/Object`. */
    static byte[] classBytes(String name) {
        String sup = "java/lang/Object";
        java.io.ByteArrayOutputStream b = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream d = new java.io.DataOutputStream(b);
        try {
            d.writeInt(0xCAFEBABE);
            d.writeShort(0);
            d.writeShort(52);
            d.writeShort(5);
            d.writeByte(7);
            d.writeShort(2);
            d.writeByte(1);
            d.writeUTF(name);
            d.writeByte(7);
            d.writeShort(4);
            d.writeByte(1);
            d.writeUTF(sup);
            d.writeShort(0x21);
            d.writeShort(1);
            d.writeShort(3);
            d.writeShort(0);
            d.writeShort(0);
            d.writeShort(0);
            d.writeShort(0);
        } catch (java.io.IOException e) {
            throw new RuntimeException(e);
        }
        return b.toByteArray();
    }

    static final class Definer extends ClassLoader {
        Class<?> define(String name) {
            byte[] bytes = classBytes(name.replace('.', '/'));
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static int define(int n, int round) {
        Definer loader = new Definer();
        List<Class<?>> kept = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            kept.add(loader.define("l5w22.Gen" + round + "_" + i));
        }
        return kept.size();
    }

    public static void main(String[] args) {
        final int n = 10_000_000;
        for (int w = 0; w < 5; w++) {
            Holder.counter = 0;
            statics(n / 10);
            allocs(n / 10);
        }
        Holder.counter = 0;
        long t0 = System.nanoTime();
        long s = statics(n / 2);
        long t1 = System.nanoTime();
        System.out.println("statics checksum=" + s);
        long a = allocs(n);
        long t2 = System.nanoTime();
        System.out.println("new checksum=" + a);
        define(50, 0);
        long t3 = System.nanoTime();
        int defined = define(200, 1);
        long t4 = System.nanoTime();
        System.out.println("define classes=" + defined);
        System.out.printf(java.util.Locale.ROOT, "statics ns/iter %.2f%n", (t1 - t0) / (double) (n / 2));
        System.out.printf(java.util.Locale.ROOT, "new ns/alloc %.2f%n", (t2 - t1) / (double) n);
        System.out.printf(java.util.Locale.ROOT, "define us/class %.2f%n", (t4 - t3) / 1000.0 / defined);
    }
}
