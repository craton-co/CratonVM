// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L2 (hunt): linkage and dispatch errors
// raised at call sites of COMPILED code, against the same site interpreted.
//
// javac never emits these shapes, so the probe writes its own classes (a
// minimal class-file writer below, version 52, straight-line code: no
// StackMapTable needed) and defines them with `Lookup.defineClass` in this
// class's package and loader:
//
//   * `L2W104GenNoImpl implements Iface` without `m()`          -> AbstractMethodError at invokeinterface
//   * `L2W104GenNoAb extends Base` without `ab()`               -> AbstractMethodError at invokevirtual
//   * `L2W104GenConflict implements D1, D2` (both default d())  -> IncompatibleClassChangeError
//   * `L2W104GenCaller`, one static method per call site: invokeinterface /
//     invokevirtual on the receiver it is handed, and sites whose reference
//     does not resolve or resolves to the wrong kind (NoSuchMethodError,
//     NoSuchFieldError, `Expecting non-static`, `Expected static`, `Found
//     interface ..., but class was expected`, `Found class ..., but interface
//     was expected`, InstantiationError).
//
// Every row warms its own call site WARM times (with a receiver that
// dispatches, where one exists, so the site's inline cache / itable path is
// what meets the bad receiver), and prints
// `name cold=<answer> hot=<answer> after=<answer>`: `cold` is the first call
// (interpreted), `hot` the same bad call once the site is compiled, `after` a
// good call right after it (the site must still dispatch). HotSpot 25 prints
// cold == hot on every row. Diff the whole output against `java`; run by
// default, with `--nojit`, with `CRATONVM_BG_COMPILE=0`, in both `--jdk-only`
// and `--compatible`.
//
// HotSpot's own tier-dependent text (wave 104 hand-back, Windows 25.0.3): for
// the two invokevirtual AbstractMethodError rows HotSpot says "Receiver class
// L2W104GenNoAb does not define or inherit an implementation of the resolved
// method 'abstract int ab()' of abstract class ...$Base." when the call is
// resolved by the interpreter / LinkResolver, and "Missing implementation of
// resolved method 'abstract int ab()' of abstract class ...$Base." when it
// comes from a compiled vtable stub -- and which of cold/hot gets which varies
// from run to run. CratonVM always answers with the first (the interpreter's)
// text. Those two rows therefore print both HotSpot spellings as one token,
// `<no implementation of ab() in L2W104GenNoAb>` (`normaliseAme`); any other
// text, and every other row, is printed verbatim.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L2W104CompiledDispatchErrors {
    static final int WARM = 30_000;
    static final String SELF = "L2W104CompiledDispatchErrors";
    static final String IFACE = SELF + "$Iface";
    static final String BASE = SELF + "$Base";
    static final String D1 = SELF + "$D1";
    static final String D2 = SELF + "$D2";

    interface Iface {
        int m();
    }

    static class Good implements Iface {
        public int m() {
            return 7;
        }
    }

    static class Good2 implements Iface {
        public int m() {
            return 8;
        }
    }

    static class Good3 implements Iface {
        public int m() {
            return 9;
        }
    }

    abstract static class Base {
        abstract int ab();

        int inst() {
            return 3;
        }

        static int stat() {
            return 4;
        }
    }

    static class GoodSub extends Base {
        int ab() {
            return 5;
        }
    }

    static class GoodSub2 extends Base {
        int ab() {
            return 6;
        }
    }

    interface D1 {
        default int d() {
            return 1;
        }
    }

    interface D2 {
        default int d() {
            return 2;
        }
    }

    static class OnlyD1 implements D1 {
    }

    // ---- a minimal class-file writer ----
    static final class ClassWriter {
        final ByteArrayOutputStream poolBytes = new ByteArrayOutputStream();
        final DataOutputStream pool = new DataOutputStream(poolBytes);
        final Map<String, Integer> entries = new HashMap<>();
        final List<byte[]> methods = new ArrayList<>();
        int count = 1;
        final int thisClass;
        final int superClass;
        final int[] interfaces;

        ClassWriter(String name, String superName, String... ifaces) throws IOException {
            thisClass = cls(name);
            superClass = cls(superName);
            interfaces = new int[ifaces.length];
            for (int i = 0; i < ifaces.length; i++) {
                interfaces[i] = cls(ifaces[i]);
            }
        }

        int utf8(String s) throws IOException {
            Integer old = entries.get("U" + s);
            if (old != null) {
                return old;
            }
            pool.writeByte(1);
            pool.writeUTF(s);
            entries.put("U" + s, count);
            return count++;
        }

        int cls(String internal) throws IOException {
            Integer old = entries.get("C" + internal);
            if (old != null) {
                return old;
            }
            int u = utf8(internal);
            pool.writeByte(7);
            pool.writeShort(u);
            entries.put("C" + internal, count);
            return count++;
        }

        int ref(int tag, String owner, String name, String desc) throws IOException {
            String key = "R" + tag + owner + "." + name + desc;
            Integer old = entries.get(key);
            if (old != null) {
                return old;
            }
            int c = cls(owner);
            int n = utf8(name);
            int d = utf8(desc);
            Integer natOld = entries.get("N" + name + desc);
            int nat;
            if (natOld != null) {
                nat = natOld;
            } else {
                pool.writeByte(12);
                pool.writeShort(n);
                pool.writeShort(d);
                entries.put("N" + name + desc, count);
                nat = count++;
            }
            pool.writeByte(tag);
            pool.writeShort(c);
            pool.writeShort(nat);
            entries.put(key, count);
            return count++;
        }

        int methodref(String owner, String name, String desc) throws IOException {
            return ref(10, owner, name, desc);
        }

        int imethodref(String owner, String name, String desc) throws IOException {
            return ref(11, owner, name, desc);
        }

        int fieldref(String owner, String name, String desc) throws IOException {
            return ref(9, owner, name, desc);
        }

        void method(int access, String name, String desc, int maxStack, int maxLocals, byte[] code)
                throws IOException {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            DataOutputStream o = new DataOutputStream(b);
            o.writeShort(access);
            o.writeShort(utf8(name));
            o.writeShort(utf8(desc));
            o.writeShort(1);
            o.writeShort(utf8("Code"));
            o.writeInt(12 + code.length);
            o.writeShort(maxStack);
            o.writeShort(maxLocals);
            o.writeInt(code.length);
            o.write(code);
            o.writeShort(0);
            o.writeShort(0);
            methods.add(b.toByteArray());
        }

        void defaultConstructor(String superName) throws IOException {
            int init = methodref(superName, "<init>", "()V");
            method(0x0001, "<init>", "()V", 1, 1,
                    new byte[] {0x2a, (byte) 0xb7, hi(init), lo(init), (byte) 0xb1});
        }

        byte[] toBytes() throws IOException {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            DataOutputStream o = new DataOutputStream(b);
            o.writeInt(0xCAFEBABE);
            o.writeShort(0);
            o.writeShort(52);
            o.writeShort(count);
            o.write(poolBytes.toByteArray());
            o.writeShort(0x0021);
            o.writeShort(thisClass);
            o.writeShort(superClass);
            o.writeShort(interfaces.length);
            for (int i : interfaces) {
                o.writeShort(i);
            }
            o.writeShort(0);
            o.writeShort(methods.size());
            for (byte[] m : methods) {
                o.write(m);
            }
            o.writeShort(0);
            return b.toByteArray();
        }
    }

    static byte hi(int v) {
        return (byte) (v >> 8);
    }

    static byte lo(int v) {
        return (byte) v;
    }

    static final byte ALOAD_0 = 0x2a;
    static final byte IRETURN = (byte) 0xac;
    static final byte CHECKCAST = (byte) 0xc0;

    // `static int <name>(Object)`: aload_0; [checkcast <cast>]; <invoke> <ref>; ireturn
    static void receiverSite(ClassWriter w, String name, String cast, byte invoke, int ref, boolean iface)
            throws IOException {
        ByteArrayOutputStream code = new ByteArrayOutputStream();
        code.write(ALOAD_0);
        if (cast != null) {
            int c = w.cls(cast);
            code.write(CHECKCAST);
            code.write(hi(c));
            code.write(lo(c));
        }
        code.write(invoke);
        code.write(hi(ref));
        code.write(lo(ref));
        if (iface) {
            code.write(1);
            code.write(0);
        }
        code.write(IRETURN);
        w.method(0x0009, name, "(Ljava/lang/Object;)I", 1, 1, code.toByteArray());
    }

    // `static int <name>()`: <op> <ref>; ireturn
    static void noArgSite(ClassWriter w, String name, byte op, int ref) throws IOException {
        w.method(0x0009, name, "()I", 1, 0, new byte[] {op, hi(ref), lo(ref), IRETURN});
    }

    static Class<?> define(MethodHandles.Lookup lookup, ClassWriter w) throws Exception {
        return lookup.defineClass(w.toBytes());
    }

    static MethodHandles.Lookup LOOKUP = MethodHandles.lookup();
    static Class<?> caller;

    static MethodHandle site(String name, MethodType type) throws Exception {
        return LOOKUP.findStatic(caller, name, type);
    }

    static String call(MethodHandle h, Object receiver) {
        try {
            if (h.type().parameterCount() == 0) {
                return String.valueOf((int) h.invokeExact());
            }
            return String.valueOf((int) h.invokeExact(receiver));
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void row(String name, MethodHandle h, Object bad, Object[] goods) {
        rowNormalised(name, h, bad, goods, false);
    }

    // The two invokevirtual AbstractMethodError rows: HotSpot's own text
    // depends on its tier (see the header), so both of its spellings for
    // `ab()` print one token. Any other text is printed whole.
    static String normaliseAme(String answer) {
        String prefix = "java.lang.AbstractMethodError: ";
        if (answer.startsWith(prefix) && answer.contains("resolved method 'abstract int ab()' of abstract class "
                + BASE.replace('/', '.') + ".")
                && (answer.startsWith(prefix + "Receiver class L2W104GenNoAb does not define or inherit an implementation of the resolved method")
                        || answer.startsWith(prefix + "Missing implementation of resolved method"))) {
            return prefix + "<no implementation of ab() in L2W104GenNoAb>";
        }
        return answer;
    }

    static void rowNormalised(String name, MethodHandle h, Object bad, Object[] goods, boolean ame) {
        String cold = call(h, bad);
        for (int i = 0; i < WARM; i++) {
            call(h, goods[i % goods.length]);
        }
        String hot = call(h, bad);
        String after = call(h, goods[0]);
        if (ame) {
            cold = normaliseAme(cold);
            hot = normaliseAme(hot);
        }
        System.out.println(name + " cold=" + cold + " hot=" + hot + " after=" + after);
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup lookup = LOOKUP;

        ClassWriter noImpl = new ClassWriter("L2W104GenNoImpl", "java/lang/Object", IFACE);
        noImpl.defaultConstructor("java/lang/Object");
        Class<?> noImplClass = define(lookup, noImpl);

        ClassWriter noAb = new ClassWriter("L2W104GenNoAb", BASE);
        noAb.defaultConstructor(BASE);
        Class<?> noAbClass = define(lookup, noAb);

        ClassWriter conflict = new ClassWriter("L2W104GenConflict", "java/lang/Object", D1, D2);
        conflict.defaultConstructor("java/lang/Object");
        Class<?> conflictClass = define(lookup, conflict);

        ClassWriter c = new ClassWriter("L2W104GenCaller", "java/lang/Object");
        byte invokeinterface = (byte) 0xb9;
        byte invokevirtual = (byte) 0xb6;
        byte invokestatic = (byte) 0xb8;
        receiverSite(c, "ifaceMono", null, invokeinterface, c.imethodref(IFACE, "m", "()I"), true);
        receiverSite(c, "ifaceMega", null, invokeinterface, c.imethodref(IFACE, "m", "()I"), true);
        receiverSite(c, "ifaceNotImplemented", null, invokeinterface, c.imethodref(IFACE, "m", "()I"), true);
        receiverSite(c, "ifaceNotImplementedMega", null, invokeinterface, c.imethodref(IFACE, "m", "()I"), true);
        receiverSite(c, "ifaceArrayReceiver", null, invokeinterface, c.imethodref(IFACE, "m", "()I"), true);
        receiverSite(c, "virtMono", BASE, invokevirtual, c.methodref(BASE, "ab", "()I"), false);
        receiverSite(c, "virtBi", BASE, invokevirtual, c.methodref(BASE, "ab", "()I"), false);
        receiverSite(c, "defaultConflict", null, invokeinterface, c.imethodref(D1, "d", "()I"), true);
        receiverSite(c, "missingVirtual", BASE, invokevirtual, c.methodref(BASE, "gone", "()I"), false);
        receiverSite(c, "staticAsVirtual", BASE, invokevirtual, c.methodref(BASE, "stat", "()I"), false);
        receiverSite(c, "virtualOnInterface", null, invokevirtual, c.methodref(IFACE, "m", "()I"), false);
        // checkcast first: the verifier wants a `Base` receiver for a class-named reference.
        receiverSite(c, "interfaceOnClass", BASE, invokeinterface, c.imethodref(BASE, "ab", "()I"), true);
        noArgSite(c, "missingStatic", invokestatic, c.methodref(SELF, "missingStatic", "()I"));
        noArgSite(c, "virtualAsStatic", invokestatic, c.methodref(BASE, "inst", "()I"));
        noArgSite(c, "missingField", (byte) 0xb2, c.fieldref(SELF, "noSuchField", "I"));
        // `static int missingFieldType()`: getstatic WARM:J (a long, two stack
        // slots); l2i; ireturn -- max_stack 2 (wave 104 hand-back: the first
        // cut reused the one-slot `()I` shape, which HotSpot's verifier
        // rejects with "Operand stack overflow" before any row runs).
        int wrongType = c.fieldref(SELF, "WARM", "J");
        c.method(0x0009, "missingFieldType", "()I", 2, 0,
                new byte[] {(byte) 0xb2, hi(wrongType), lo(wrongType), (byte) 0x88, IRETURN});
        // `static int newAbstract()`: new Base; pop; iconst_0; ireturn
        int baseClass = c.cls(BASE);
        c.method(0x0009, "newAbstract", "()I", 1, 0,
                new byte[] {(byte) 0xbb, hi(baseClass), lo(baseClass), 0x57, 0x03, IRETURN});
        int ifaceClass = c.cls(IFACE);
        c.method(0x0009, "newInterface", "()I", 1, 0,
                new byte[] {(byte) 0xbb, hi(ifaceClass), lo(ifaceClass), 0x57, 0x03, IRETURN});
        caller = define(lookup, c);

        MethodType recv = MethodType.methodType(int.class, Object.class);
        MethodType none = MethodType.methodType(int.class);
        Object badIface = noImplClass.getDeclaredConstructor().newInstance();
        Object badBase = noAbClass.getDeclaredConstructor().newInstance();
        Object badDefault = conflictClass.getDeclaredConstructor().newInstance();
        Object[] good = {new Good()};
        Object[] goods = {new Good(), new Good2(), new Good3()};
        Object[] goodSub = {new GoodSub()};
        Object[] goodSubs = {new GoodSub(), new GoodSub2()};

        row("ame-invokeinterface-mono", site("ifaceMono", recv), badIface, good);
        row("ame-invokeinterface-mega", site("ifaceMega", recv), badIface, goods);
        row("icce-not-implemented-mono", site("ifaceNotImplemented", recv), "a string", good);
        row("icce-not-implemented-mega", site("ifaceNotImplementedMega", recv), new Object(), goods);
        row("icce-array-receiver", site("ifaceArrayReceiver", recv), new int[1], good);
        rowNormalised("ame-invokevirtual-mono", site("virtMono", recv), badBase, goodSub, true);
        rowNormalised("ame-invokevirtual-bimorphic", site("virtBi", recv), badBase, goodSubs, true);
        row("icce-default-conflict", site("defaultConflict", recv), badDefault, new Object[] {new OnlyD1()});
        row("nsme-invokevirtual", site("missingVirtual", recv), new GoodSub(), goodSub);
        row("icce-static-as-virtual", site("staticAsVirtual", recv), new GoodSub(), goodSub);
        row("icce-virtual-on-interface", site("virtualOnInterface", recv), new Good(), good);
        row("icce-interface-on-class", site("interfaceOnClass", recv), new GoodSub(), goodSub);
        Object[] noArg = {null};
        row("nsme-invokestatic", site("missingStatic", none), null, noArg);
        row("icce-virtual-as-static", site("virtualAsStatic", none), null, noArg);
        row("nsfe-getstatic", site("missingField", none), null, noArg);
        row("nsfe-getstatic-wrong-type", site("missingFieldType", none), null, noArg);
        row("instantiation-abstract", site("newAbstract", none), null, noArg);
        row("instantiation-interface", site("newInterface", none), null, noArg);
    }
}
