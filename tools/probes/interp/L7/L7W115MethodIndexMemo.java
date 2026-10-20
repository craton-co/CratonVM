// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 115, lane L7: `CRATONVM_EXECUTE_METHOD_INDEX_MEMO` (default off;
// page interpreter-i114-L7-native-reentry-cost-split-KEPT-OFF-20261224). `interpreter::execute` used to find its
// method by a linear name/descriptor scan of the class's declared methods on every call, so a
// native re-entry into a class with hundreds of methods (`DirectMethodHandle$Holder`: 217 in
// JDK 25) paid a scan per call. Under the switch the declared index comes from a per-VM memo,
// used only when the method at that index still has exactly that name and descriptor.
//
// Semantics (deterministic lines, both switch arms identical, both modes, JIT and --nojit):
// overloads of one name in a class of 200+ methods, called through reflection and method
// handles, interleaved so a memo row for one overload is probed with another's key; an
// inherited method called on a subclass; a hidden class defined twice with the same method
// names in a different declaration order (a memo row for one class must not answer the other).
//
// Bench (TIME lines; the battery ignores them): `mh-first` / `mh-last` call the first and the
// last declared of 200 one-line static methods through a `MethodHandle`, `reflect-first` /
// `reflect-last` through `Method.invoke`. Switch off, `*-last` should cost more than `*-first`
// (the scan walks ~200 entries more); on, the two should match. The JDK's own
// `DirectMethodHandle$Holder` scan is what every row pays in both arms when it re-enters
// `invokeStatic` there; `L7W114NativeReentryBench` is the companion for that.
//
// Sized for --nojit (wave 116: a few seconds; 5 000 calls a row, min of 3). Run (interleave
// the arms, medians of 5 runs):
//   cratonvm --java-home <jdk25> --jdk-only -cp <dir> L7W115MethodIndexMemo
//   CRATONVM_EXECUTE_METHOD_INDEX_MEMO=1 cratonvm --java-home <jdk25> --jdk-only -cp <dir> L7W115MethodIndexMemo
// and both arms with --compatible and with --nojit; the non-TIME lines must equal the block.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// overloads f(int)399 f(long)399 f(String)s0 f(Object)4 f(int,int)400 f(int)399 f(long)-399 f(String)t m=200
// overloads digest=2262030231563600426
// inherited g@Big Big2.h299 Big2.h(String)x g@Big
// hidden a1=1 b1=2 a2=2 b2=1 sum=366300
// mh-first checksum=12497500
// mh-last checksum=13492500
// reflect-first checksum=12497500
// reflect-last checksum=13492500
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

public class L7W115MethodIndexMemo {
    static final class Big {
        static int m000(int x) { return x + 0; }
        static int m001(int x) { return x + 1; }
        static int m002(int x) { return x + 2; }
        static int m003(int x) { return x + 3; }
        static int m004(int x) { return x + 4; }
        static int m005(int x) { return x + 5; }
        static int m006(int x) { return x + 6; }
        static int m007(int x) { return x + 7; }
        static int m008(int x) { return x + 8; }
        static int m009(int x) { return x + 9; }
        static int m010(int x) { return x + 10; }
        static int m011(int x) { return x + 11; }
        static int m012(int x) { return x + 12; }
        static int m013(int x) { return x + 13; }
        static int m014(int x) { return x + 14; }
        static int m015(int x) { return x + 15; }
        static int m016(int x) { return x + 16; }
        static int m017(int x) { return x + 17; }
        static int m018(int x) { return x + 18; }
        static int m019(int x) { return x + 19; }
        static int m020(int x) { return x + 20; }
        static int m021(int x) { return x + 21; }
        static int m022(int x) { return x + 22; }
        static int m023(int x) { return x + 23; }
        static int m024(int x) { return x + 24; }
        static int m025(int x) { return x + 25; }
        static int m026(int x) { return x + 26; }
        static int m027(int x) { return x + 27; }
        static int m028(int x) { return x + 28; }
        static int m029(int x) { return x + 29; }
        static int m030(int x) { return x + 30; }
        static int m031(int x) { return x + 31; }
        static int m032(int x) { return x + 32; }
        static int m033(int x) { return x + 33; }
        static int m034(int x) { return x + 34; }
        static int m035(int x) { return x + 35; }
        static int m036(int x) { return x + 36; }
        static int m037(int x) { return x + 37; }
        static int m038(int x) { return x + 38; }
        static int m039(int x) { return x + 39; }
        static int m040(int x) { return x + 40; }
        static int m041(int x) { return x + 41; }
        static int m042(int x) { return x + 42; }
        static int m043(int x) { return x + 43; }
        static int m044(int x) { return x + 44; }
        static int m045(int x) { return x + 45; }
        static int m046(int x) { return x + 46; }
        static int m047(int x) { return x + 47; }
        static int m048(int x) { return x + 48; }
        static int m049(int x) { return x + 49; }
        static int m050(int x) { return x + 50; }
        static int m051(int x) { return x + 51; }
        static int m052(int x) { return x + 52; }
        static int m053(int x) { return x + 53; }
        static int m054(int x) { return x + 54; }
        static int m055(int x) { return x + 55; }
        static int m056(int x) { return x + 56; }
        static int m057(int x) { return x + 57; }
        static int m058(int x) { return x + 58; }
        static int m059(int x) { return x + 59; }
        static int m060(int x) { return x + 60; }
        static int m061(int x) { return x + 61; }
        static int m062(int x) { return x + 62; }
        static int m063(int x) { return x + 63; }
        static int m064(int x) { return x + 64; }
        static int m065(int x) { return x + 65; }
        static int m066(int x) { return x + 66; }
        static int m067(int x) { return x + 67; }
        static int m068(int x) { return x + 68; }
        static int m069(int x) { return x + 69; }
        static int m070(int x) { return x + 70; }
        static int m071(int x) { return x + 71; }
        static int m072(int x) { return x + 72; }
        static int m073(int x) { return x + 73; }
        static int m074(int x) { return x + 74; }
        static int m075(int x) { return x + 75; }
        static int m076(int x) { return x + 76; }
        static int m077(int x) { return x + 77; }
        static int m078(int x) { return x + 78; }
        static int m079(int x) { return x + 79; }
        static int m080(int x) { return x + 80; }
        static int m081(int x) { return x + 81; }
        static int m082(int x) { return x + 82; }
        static int m083(int x) { return x + 83; }
        static int m084(int x) { return x + 84; }
        static int m085(int x) { return x + 85; }
        static int m086(int x) { return x + 86; }
        static int m087(int x) { return x + 87; }
        static int m088(int x) { return x + 88; }
        static int m089(int x) { return x + 89; }
        static int m090(int x) { return x + 90; }
        static int m091(int x) { return x + 91; }
        static int m092(int x) { return x + 92; }
        static int m093(int x) { return x + 93; }
        static int m094(int x) { return x + 94; }
        static int m095(int x) { return x + 95; }
        static int m096(int x) { return x + 96; }
        static int m097(int x) { return x + 97; }
        static int m098(int x) { return x + 98; }
        static int m099(int x) { return x + 99; }
        static int m100(int x) { return x + 100; }
        static int m101(int x) { return x + 101; }
        static int m102(int x) { return x + 102; }
        static int m103(int x) { return x + 103; }
        static int m104(int x) { return x + 104; }
        static int m105(int x) { return x + 105; }
        static int m106(int x) { return x + 106; }
        static int m107(int x) { return x + 107; }
        static int m108(int x) { return x + 108; }
        static int m109(int x) { return x + 109; }
        static int m110(int x) { return x + 110; }
        static int m111(int x) { return x + 111; }
        static int m112(int x) { return x + 112; }
        static int m113(int x) { return x + 113; }
        static int m114(int x) { return x + 114; }
        static int m115(int x) { return x + 115; }
        static int m116(int x) { return x + 116; }
        static int m117(int x) { return x + 117; }
        static int m118(int x) { return x + 118; }
        static int m119(int x) { return x + 119; }
        static int m120(int x) { return x + 120; }
        static int m121(int x) { return x + 121; }
        static int m122(int x) { return x + 122; }
        static int m123(int x) { return x + 123; }
        static int m124(int x) { return x + 124; }
        static int m125(int x) { return x + 125; }
        static int m126(int x) { return x + 126; }
        static int m127(int x) { return x + 127; }
        static int m128(int x) { return x + 128; }
        static int m129(int x) { return x + 129; }
        static int m130(int x) { return x + 130; }
        static int m131(int x) { return x + 131; }
        static int m132(int x) { return x + 132; }
        static int m133(int x) { return x + 133; }
        static int m134(int x) { return x + 134; }
        static int m135(int x) { return x + 135; }
        static int m136(int x) { return x + 136; }
        static int m137(int x) { return x + 137; }
        static int m138(int x) { return x + 138; }
        static int m139(int x) { return x + 139; }
        static int m140(int x) { return x + 140; }
        static int m141(int x) { return x + 141; }
        static int m142(int x) { return x + 142; }
        static int m143(int x) { return x + 143; }
        static int m144(int x) { return x + 144; }
        static int m145(int x) { return x + 145; }
        static int m146(int x) { return x + 146; }
        static int m147(int x) { return x + 147; }
        static int m148(int x) { return x + 148; }
        static int m149(int x) { return x + 149; }
        static int m150(int x) { return x + 150; }
        static int m151(int x) { return x + 151; }
        static int m152(int x) { return x + 152; }
        static int m153(int x) { return x + 153; }
        static int m154(int x) { return x + 154; }
        static int m155(int x) { return x + 155; }
        static int m156(int x) { return x + 156; }
        static int m157(int x) { return x + 157; }
        static int m158(int x) { return x + 158; }
        static int m159(int x) { return x + 159; }
        static int m160(int x) { return x + 160; }
        static int m161(int x) { return x + 161; }
        static int m162(int x) { return x + 162; }
        static int m163(int x) { return x + 163; }
        static int m164(int x) { return x + 164; }
        static int m165(int x) { return x + 165; }
        static int m166(int x) { return x + 166; }
        static int m167(int x) { return x + 167; }
        static int m168(int x) { return x + 168; }
        static int m169(int x) { return x + 169; }
        static int m170(int x) { return x + 170; }
        static int m171(int x) { return x + 171; }
        static int m172(int x) { return x + 172; }
        static int m173(int x) { return x + 173; }
        static int m174(int x) { return x + 174; }
        static int m175(int x) { return x + 175; }
        static int m176(int x) { return x + 176; }
        static int m177(int x) { return x + 177; }
        static int m178(int x) { return x + 178; }
        static int m179(int x) { return x + 179; }
        static int m180(int x) { return x + 180; }
        static int m181(int x) { return x + 181; }
        static int m182(int x) { return x + 182; }
        static int m183(int x) { return x + 183; }
        static int m184(int x) { return x + 184; }
        static int m185(int x) { return x + 185; }
        static int m186(int x) { return x + 186; }
        static int m187(int x) { return x + 187; }
        static int m188(int x) { return x + 188; }
        static int m189(int x) { return x + 189; }
        static int m190(int x) { return x + 190; }
        static int m191(int x) { return x + 191; }
        static int m192(int x) { return x + 192; }
        static int m193(int x) { return x + 193; }
        static int m194(int x) { return x + 194; }
        static int m195(int x) { return x + 195; }
        static int m196(int x) { return x + 196; }
        static int m197(int x) { return x + 197; }
        static int m198(int x) { return x + 198; }
        static int m199(int x) { return x + 199; }

        static String f(int x) {
            return "f(int)" + x;
        }

        static String f(long x) {
            return "f(long)" + x;
        }

        static String f(String x) {
            return "f(String)" + x;
        }

        static String f(Object x) {
            return "f(Object)" + x;
        }

        static String f(int x, int y) {
            return "f(int,int)" + (x + y);
        }

        String g() {
            return "g@" + getClass().getSimpleName();
        }
    }

    static final class Sub {
        static String call(Big b) {
            return b.g();
        }
    }

    static class BigChild extends Big2 {
    }

    static class Big2 {
        public String h(int x) {
            return "Big2.h" + x;
        }

        public String h(String x) {
            return "Big2.h(String)" + x;
        }
    }

    interface Row {
        long run(int n) throws Throwable;
    }

    static final int REPS = 3;
    static final int N = 5_000;

    static final MethodHandle MH_FIRST;
    static final MethodHandle MH_LAST;
    static final Method M_FIRST;
    static final Method M_LAST;

    static {
        try {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodType t = MethodType.methodType(int.class, int.class);
            MH_FIRST = lookup.findStatic(Big.class, "m000", t);
            MH_LAST = lookup.findStatic(Big.class, "m199", t);
            M_FIRST = Big.class.getDeclaredMethod("m000", int.class);
            M_LAST = Big.class.getDeclaredMethod("m199", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    public static void main(String[] args) throws Throwable {
        semantics();
        hiddenTwice();
        bench(args.length > 0 ? args[0] : null);
    }

    static void semantics() throws Throwable {
        Method fi = Big.class.getDeclaredMethod("f", int.class);
        Method fl = Big.class.getDeclaredMethod("f", long.class);
        Method fs = Big.class.getDeclaredMethod("f", String.class);
        Method fo = Big.class.getDeclaredMethod("f", Object.class);
        Method fii = Big.class.getDeclaredMethod("f", int.class, int.class);
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle hi = lookup.findStatic(Big.class, "f", MethodType.methodType(String.class, int.class));
        MethodHandle hl = lookup.findStatic(Big.class, "f", MethodType.methodType(String.class, long.class));
        MethodHandle hs = lookup.findStatic(Big.class, "f", MethodType.methodType(String.class, String.class));
        StringBuilder sb = new StringBuilder();
        long digest = 0;
        for (int i = 0; i < 400; i++) {
            String a = (String) fi.invoke(null, i);
            String b = (String) fl.invoke(null, (long) i);
            String c = (String) fs.invoke(null, "s" + (i % 7));
            String d = (String) fo.invoke(null, Integer.valueOf(i % 5));
            String e = (String) fii.invoke(null, i, 1);
            String f = (String) hi.invokeExact(i);
            String g = (String) hl.invokeExact((long) -i);
            String h = (String) hs.invokeExact("t");
            int m = (int) Big.class.getDeclaredMethod("m" + String.format("%03d", i % 200), int.class)
                    .invoke(null, 1);
            digest = digest * 31 + a.hashCode() + b.hashCode() + c.hashCode() + d.hashCode()
                    + e.hashCode() + f.hashCode() + g.hashCode() + h.hashCode() + m;
            if (i == 399) {
                sb.append(a).append(' ').append(b).append(' ').append(c).append(' ').append(d)
                        .append(' ').append(e).append(' ').append(f).append(' ').append(g)
                        .append(' ').append(h).append(" m=").append(m);
            }
        }
        System.out.println("overloads " + sb);
        System.out.println("overloads digest=" + digest);
        Method gm = Big.class.getDeclaredMethod("g");
        Method h1 = BigChild.class.getMethod("h", int.class);
        Method h2 = BigChild.class.getMethod("h", String.class);
        BigChild child = new BigChild();
        String last = null;
        for (int i = 0; i < 300; i++) {
            last = gm.invoke(new Big()) + " " + h1.invoke(child, i) + " " + h2.invoke(child, "x")
                    + " " + Sub.call(new Big());
        }
        System.out.println("inherited " + last);
    }

    // Two hidden classes from bytes javac would emit for two sources that declare the same
    // methods in a different order: the memo row filed for `a` in one names another index in
    // the other, which must miss there (the method at that index has another name).
    static void hiddenTwice() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        byte[] ab = HiddenBytes.classBytes("L7W115HiddenAB", new String[] {"a", "b"});
        byte[] ba = HiddenBytes.classBytes("L7W115HiddenBA", new String[] {"b", "a"});
        Class<?> c1 = lookup.defineHiddenClass(ab, true).lookupClass();
        Class<?> c2 = lookup.defineHiddenClass(ba, true).lookupClass();
        MethodHandles.Lookup l1 = MethodHandles.privateLookupIn(c1, lookup);
        MethodHandles.Lookup l2 = MethodHandles.privateLookupIn(c2, lookup);
        MethodType t = MethodType.methodType(int.class);
        MethodHandle a1 = l1.findStatic(c1, "a", t);
        MethodHandle b1 = l1.findStatic(c1, "b", t);
        MethodHandle a2 = l2.findStatic(c2, "a", t);
        MethodHandle b2 = l2.findStatic(c2, "b", t);
        long s = 0;
        for (int i = 0; i < 300; i++) {
            s += (int) a1.invokeExact() * 1000 + (int) b1.invokeExact() * 100
                    + (int) a2.invokeExact() * 10 + (int) b2.invokeExact();
        }
        System.out.println("hidden a1=" + (int) a1.invokeExact() + " b1=" + (int) b1.invokeExact()
                + " a2=" + (int) a2.invokeExact() + " b2=" + (int) b2.invokeExact() + " sum=" + s);
    }

    static long mhFirst(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) MH_FIRST.invokeExact(i);
        }
        return s;
    }

    static long mhLast(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) MH_LAST.invokeExact(i);
        }
        return s;
    }

    static long reflectFirst(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) M_FIRST.invoke(null, i);
        }
        return s;
    }

    static long reflectLast(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) M_LAST.invoke(null, i);
        }
        return s;
    }

    static void bench(String only) throws Throwable {
        String[] names = {"mh-first", "mh-last", "reflect-first", "reflect-last"};
        Row[] rows = {
            L7W115MethodIndexMemo::mhFirst,
            L7W115MethodIndexMemo::mhLast,
            L7W115MethodIndexMemo::reflectFirst,
            L7W115MethodIndexMemo::reflectLast,
        };
        for (int r = 0; r < rows.length; r++) {
            if (only != null && !only.equals(names[r])) {
                continue;
            }
            long checksum = rows[r].run(N);
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < REPS; rep++) {
                long t0 = System.nanoTime();
                long c = rows[r].run(N);
                long dt = System.nanoTime() - t0;
                if (c != checksum) {
                    System.out.println(names[r] + " UNSTABLE " + c + " != " + checksum);
                }
                best = Math.min(best, dt);
            }
            System.out.println(names[r] + " checksum=" + checksum);
            System.out.println("TIME " + names[r] + " " + (best / N) + " ns/op (min of " + REPS + ")");
        }
    }

    // A minimal class-file writer for the hidden-class row: `public final class <name>` with
    // a static `()I` method per name, method i returning i + 1 (so `a` returns 1 in one class
    // and 2 in the other).
    static final class HiddenBytes {
        static byte[] classBytes(String name, String[] methods) {
            java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
            java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
            try {
                // Constant pool: 1 Utf8 name, 2 Class #1, 3 Utf8 java/lang/Object, 4 Class #3,
                // 5 Utf8 ()I, 6 Utf8 Code, then one Utf8 per method name.
                int cpCount = 7 + methods.length;
                out.writeInt(0xCAFEBABE);
                out.writeShort(0);
                out.writeShort(69);
                out.writeShort(cpCount);
                out.writeByte(1);
                out.writeUTF(name);
                out.writeByte(7);
                out.writeShort(1);
                out.writeByte(1);
                out.writeUTF("java/lang/Object");
                out.writeByte(7);
                out.writeShort(3);
                out.writeByte(1);
                out.writeUTF("()I");
                out.writeByte(1);
                out.writeUTF("Code");
                for (String m : methods) {
                    out.writeByte(1);
                    out.writeUTF(m);
                }
                out.writeShort(0x0011); // ACC_PUBLIC | ACC_FINAL
                out.writeShort(2);
                out.writeShort(4);
                out.writeShort(0); // interfaces
                out.writeShort(0); // fields
                out.writeShort(methods.length);
                for (int i = 0; i < methods.length; i++) {
                    out.writeShort(0x0009); // ACC_PUBLIC | ACC_STATIC
                    out.writeShort(7 + i);
                    out.writeShort(5);
                    out.writeShort(1); // one attribute: Code
                    out.writeShort(6);
                    out.writeInt(12 + 2);
                    out.writeShort(1); // max_stack
                    out.writeShort(0); // max_locals
                    out.writeInt(2);
                    out.writeByte(0x04 + i); // iconst_1 / iconst_2
                    out.writeByte(0xac); // ireturn
                    out.writeShort(0); // exception table
                    out.writeShort(0); // attributes
                }
                out.writeShort(0); // class attributes
                out.flush();
            } catch (java.io.IOException e) {
                throw new AssertionError(e);
            }
            return bytes.toByteArray();
        }
    }
}
