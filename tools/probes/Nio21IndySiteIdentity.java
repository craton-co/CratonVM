// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * nio21 wave 11, lane L4: witness for
 * docs/internal/fixed-bugs/r21-lambda-call-site-eviction-rebootstraps-a-linked-site-FIXED-20261009.md.
 *
 * <p>JVMS 5.4.3.6 links an {@code invokedynamic} instruction (and resolves a
 * {@code CONSTANT_Dynamic}) ONCE: the same call site for the life of the class, its bootstrap
 * run once. CratonVM's resolution cache evicted its oldest call-site rows past 65,536 sites, and
 * an evicted lambda row was bootstrapped again (a new proxy class, a new non-capturing
 * singleton); generic {@code invokedynamic} instructions linked past the same count were never
 * published, so their bootstrap ran on every execution.
 *
 * <p>The probe generates classes (a hand-written class-file writer, no library) holding
 * {@code -Dnio21.sites=} (default 72000) distinct sites of each kind -- a non-capturing
 * {@code LambdaMetafactory} lambda, a generic {@code invokedynamic} whose bootstrap counts its
 * calls and returns a {@code ConstantCallSite} of a fresh object, and an {@code ldc} of a
 * {@code CONSTANT_Dynamic} whose bootstrap counts and returns a fresh object -- each in its own
 * constant-pool entry and instruction. Every site runs twice, oldest first; the second pass must
 * answer the identical object at every site and no bootstrap may run again. Straight-line code
 * only (no branches, so no {@code StackMapTable}). Conformance output only; HotSpot prints zero
 * mismatches and {@code bootstraps=<sites>}.
 *
 * <p>nio21 wave 12 L1: {@code -Dnio21.sites=120000} also crosses CratonVM's former
 * {@code MAX_LAMBDA_PROXIES} (100 000) registration cap
 * (docs/internal/fixed-bugs/r21-lambda-proxy-registration-cap-drops-the-100001st-proxy-FIXED-20261009.md).
 * Expected output is the same shape with 120000 in place of 72000 (zero mismatches, zero nulls,
 * {@code bootstraps first-pass=120000 total=120000}); under
 * {@code CRATONVM_LAMBDA_PROXY_UNCAPPED=0} the proxies past the cap are unregistered, so the
 * lambda rows (or the run) diverge -- the positive control. Expect roughly 1.7x the default run
 * time on HotSpot.
 */
public class Nio21IndySiteIdentity {
    static final int CHUNK = 2000; // sites per generated method (code stays under 64 KiB)
    static final int CHUNKS_PER_CLASS = 9; // 3 kinds x 18000 sites = 54000 pool entries

    public static int indyBoots;
    public static int condyBoots;

    public static CallSite indyBsm(MethodHandles.Lookup lookup, String name, MethodType type) {
        indyBoots++;
        return new ConstantCallSite(MethodHandles.constant(Object.class, new Object()));
    }

    public static Object condyBsm(MethodHandles.Lookup lookup, String name, Class<?> type) {
        condyBoots++;
        return new Object();
    }

    static final class GenLoader extends ClassLoader {
        GenLoader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(byte[] bytes) {
            return defineClass(null, bytes, 0, bytes.length);
        }
    }

    public static void main(String[] args) throws Throwable {
        int requested = Integer.getInteger("nio21.sites", 72000);
        int chunks = Math.max(1, (requested + CHUNK - 1) / CHUNK);
        int sites = chunks * CHUNK;
        GenLoader loader = new GenLoader(Nio21IndySiteIdentity.class.getClassLoader());
        // Per kind (L, G, C), the chunk methods in site order, oldest first.
        List<List<Method>> byKind = new ArrayList<>();
        for (int k = 0; k < 3; k++) byKind.add(new ArrayList<>());
        for (int c = 0, cls = 0; c < chunks; c += CHUNKS_PER_CLASS, cls++) {
            int n = Math.min(CHUNKS_PER_CLASS, chunks - c);
            String name = "Nio21IndyGen" + cls;
            Class<?> k = loader.define(generate(name, n));
            Map<String, Method> methods = new HashMap<>();
            for (Method m : k.getDeclaredMethods()) methods.put(m.getName(), m);
            for (int j = 0; j < n; j++) {
                byKind.get(0).add(methods.get("L" + j));
                byKind.get(1).add(methods.get("G" + j));
                byKind.get(2).add(methods.get("C" + j));
            }
        }
        Object[][][] first = runAll(byKind, chunks);
        int indyAfterFirst = indyBoots;
        int condyAfterFirst = condyBoots;
        Object[][][] second = runAll(byKind, chunks);

        System.out.println("sites " + sites);
        String[] kinds = {"lambda", "generic", "condy"};
        for (int k = 0; k < 3; k++) {
            int identity = 0;
            int klass = 0;
            int nulls = 0;
            for (int c = 0; c < chunks; c++) {
                for (int i = 0; i < CHUNK; i++) {
                    Object a = first[k][c][i];
                    Object b = second[k][c][i];
                    if (a == null || b == null) {
                        nulls++;
                        continue;
                    }
                    if (a != b) identity++;
                    if (a.getClass() != b.getClass()) klass++;
                }
            }
            System.out.println(kinds[k] + " identity-mismatches=" + identity
                    + " class-mismatches=" + klass + " nulls=" + nulls);
        }
        // Distinct sites are distinct linkages: two lambda sites never share an instance.
        System.out.println("lambda first-two-sites-distinct="
                + (first[0][0][0] != first[0][0][1]));
        System.out.println("generic bootstraps first-pass=" + indyAfterFirst
                + " total=" + indyBoots);
        System.out.println("condy bootstraps first-pass=" + condyAfterFirst
                + " total=" + condyBoots);
    }

    static Object[][][] runAll(List<List<Method>> byKind, int chunks) throws Throwable {
        Object[][][] out = new Object[3][chunks][];
        for (int k = 0; k < 3; k++) {
            for (int c = 0; c < chunks; c++) {
                Object[] arr = new Object[CHUNK];
                byKind.get(k).get(c).invoke(null, (Object) arr);
                out[k][c] = arr;
            }
        }
        return out;
    }

    // ------------------------------------------------------------------
    // Minimal class-file writer
    // ------------------------------------------------------------------

    static final class Pool {
        final ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        final DataOutputStream out = new DataOutputStream(bytes);
        final Map<String, Integer> utf8s = new HashMap<>();
        int count = 1;

        int utf8(String s) throws IOException {
            Integer have = utf8s.get(s);
            if (have != null) return have;
            out.writeByte(1);
            out.writeUTF(s);
            utf8s.put(s, count);
            return count++;
        }

        int u2(int tag, int a) throws IOException {
            out.writeByte(tag);
            out.writeShort(a);
            return count++;
        }

        int u2u2(int tag, int a, int b) throws IOException {
            out.writeByte(tag);
            out.writeShort(a);
            out.writeShort(b);
            return count++;
        }

        int classRef(String name) throws IOException {
            return u2(7, utf8(name));
        }

        int nat(String name, String desc) throws IOException {
            return u2u2(12, utf8(name), utf8(desc));
        }

        int methodRef(int cls, String name, String desc) throws IOException {
            return u2u2(10, cls, nat(name, desc));
        }

        int invokeStaticHandle(int methodRef) throws IOException {
            out.writeByte(15);
            out.writeByte(6); // REF_invokeStatic
            out.writeShort(methodRef);
            return count++;
        }

        int methodType(String desc) throws IOException {
            return u2(16, utf8(desc));
        }
    }

    static byte[] generate(String name, int chunks) throws IOException {
        final String obj = "()Ljava/lang/Object;";
        Pool cp = new Pool();
        int thisClass = cp.classRef(name);
        int superClass = cp.classRef("java/lang/Object");
        int code = cp.utf8("Code");
        int bsmAttr = cp.utf8("BootstrapMethods");
        int fillDesc = cp.utf8("([Ljava/lang/Object;)V");
        int implName = cp.utf8("impl");
        int implDesc = cp.utf8(obj);
        // Bootstrap method 0: LambdaMetafactory.metafactory(()Object, impl, ()Object).
        int lmf = cp.classRef("java/lang/invoke/LambdaMetafactory");
        int metafactory = cp.invokeStaticHandle(cp.methodRef(lmf, "metafactory",
                "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;"
                        + "Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;"
                        + "Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)"
                        + "Ljava/lang/invoke/CallSite;"));
        int erased = cp.methodType(obj);
        int implHandle = cp.invokeStaticHandle(cp.methodRef(thisClass, "impl", obj));
        // Bootstrap methods 1 and 2: this probe's counting bootstraps.
        int probe = cp.classRef("Nio21IndySiteIdentity");
        int indyBsm = cp.invokeStaticHandle(cp.methodRef(probe, "indyBsm",
                "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;"
                        + "Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;"));
        int condyBsm = cp.invokeStaticHandle(cp.methodRef(probe, "condyBsm",
                "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)"
                        + "Ljava/lang/Object;"));
        int lambdaNat = cp.nat("get", "()Ljava/util/function/Supplier;");
        int genericNat = cp.nat("site", obj);
        int condyNat = cp.nat("value", "Ljava/lang/Object;");

        ByteArrayOutputStream methods = new ByteArrayOutputStream();
        DataOutputStream m = new DataOutputStream(methods);
        int methodCount = 0;
        // private static Object impl() { return null; }
        writeMethod(m, 0x000A, implName, implDesc, code, 1, 0, new byte[] {0x01, (byte) 0xB0});
        methodCount++;
        String[] prefixes = {"L", "G", "C"};
        for (int kind = 0; kind < 3; kind++) {
            for (int j = 0; j < chunks; j++) {
                ByteArrayOutputStream body = new ByteArrayOutputStream();
                for (int i = 0; i < CHUNK; i++) {
                    body.write(0x2A); // aload_0
                    body.write(0x11); // sipush i
                    body.write(i >> 8);
                    body.write(i);
                    if (kind == 2) {
                        int entry = cp.u2u2(17, 2, condyNat); // CONSTANT_Dynamic
                        body.write(0x13); // ldc_w
                        body.write(entry >> 8);
                        body.write(entry);
                    } else {
                        int entry = cp.u2u2(18, kind, kind == 0 ? lambdaNat : genericNat);
                        body.write(0xBA); // invokedynamic
                        body.write(entry >> 8);
                        body.write(entry);
                        body.write(0);
                        body.write(0);
                    }
                    body.write(0x53); // aastore
                }
                body.write(0xB1); // return
                writeMethod(m, 0x0009, cp.utf8(prefixes[kind] + j), fillDesc, code, 3, 1,
                        body.toByteArray());
                methodCount++;
            }
        }
        if (cp.count > 0xFFFF) throw new IllegalStateException("constant pool overflow");

        ByteArrayOutputStream file = new ByteArrayOutputStream();
        DataOutputStream f = new DataOutputStream(file);
        f.writeInt(0xCAFEBABE);
        f.writeShort(0);
        f.writeShort(61);
        f.writeShort(cp.count);
        cp.out.flush();
        f.write(cp.bytes.toByteArray());
        f.writeShort(0x0021); // public super
        f.writeShort(thisClass);
        f.writeShort(superClass);
        f.writeShort(0); // interfaces
        f.writeShort(0); // fields
        f.writeShort(methodCount);
        m.flush();
        f.write(methods.toByteArray());
        f.writeShort(1); // class attributes: BootstrapMethods
        f.writeShort(bsmAttr);
        f.writeInt(2 + (4 + 2 * 3) + 4 + 4);
        f.writeShort(3);
        f.writeShort(metafactory);
        f.writeShort(3);
        f.writeShort(erased);
        f.writeShort(implHandle);
        f.writeShort(erased);
        f.writeShort(indyBsm);
        f.writeShort(0);
        f.writeShort(condyBsm);
        f.writeShort(0);
        f.flush();
        return file.toByteArray();
    }

    static void writeMethod(DataOutputStream m, int access, int name, int desc, int codeAttr,
            int maxStack, int maxLocals, byte[] code) throws IOException {
        m.writeShort(access);
        m.writeShort(name);
        m.writeShort(desc);
        m.writeShort(1);
        m.writeShort(codeAttr);
        m.writeInt(12 + code.length);
        m.writeShort(maxStack);
        m.writeShort(maxLocals);
        m.writeInt(code.length);
        m.write(code);
        m.writeShort(0); // exception table
        m.writeShort(0); // attributes
    }
}
