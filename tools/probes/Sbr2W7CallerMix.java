// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.nio.ByteBuffer;
import java.security.Security;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.function.Supplier;
import java.util.stream.Collectors;
import javax.crypto.Mac;
import javax.crypto.spec.IvParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * SBR round 2, wave 7, lane {@code caller}: the caller-sensitive reflection family in one
 * diff-against-HotSpot probe. Each row prints its outcome (value or exception class and message);
 * the whole matrix is run cold and then again after a hot loop, and every row whose text changed
 * between the two passes is printed as a DRIFT line (HotSpot has a few of its own: C2 drops NPE
 * messages).
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr2W7CallerMix [hotIterations]
 * </pre>
 */
public class Sbr2W7CallerMix {
    public static class Base {
        public int bi = 7;
        protected long bl = 8;
        private String bp = "priv";
    }

    public static class H extends Base {
        public boolean z = true;
        public byte b = 1;
        public char c = 'c';
        public short s = 2;
        public int i = 3;
        public long l = 4;
        public float f = 5f;
        public double d = 6d;
        public Object o = "obj";
        public String str = "s";
        public int[] arr = {1, 2};
        public static int si = 9;
        public static Object so = "so";
        public final int fin = 11;
        public static final int SFIN = 12;
        public volatile int vol = 13;
        private int priv = 14;
        int pkg = 15;
        public transient long tr = 16;
    }

    static class Hidden {
        public int pubInHidden = 5;
    }

    static final H H0 = new H();
    static final List<String> COLD = new ArrayList<>();
    static final List<String> HOT = new ArrayList<>();
    static List<String> out = COLD;

    static void row(String name, Supplier<Object> body) {
        String r;
        try {
            Object v = body.get();
            r = v instanceof Object[] a ? java.util.Arrays.deepToString(a) : String.valueOf(v);
        } catch (Throwable t) {
            r = t.getClass().getName() + ": " + t.getMessage();
        }
        out.add(name + " = " + r);
    }

    interface Thrower<T> {
        T run() throws Throwable;
    }

    static <T> Supplier<Object> t(Thrower<T> th) {
        return () -> {
            try {
                return th.run();
            } catch (Throwable e) {
                if (e instanceof RuntimeException re) throw re;
                if (e instanceof Error er) throw er;
                throw new RuntimeException(e.getClass().getName() + ": " + e.getMessage());
            }
        };
    }

    static Field fld(String n) {
        try {
            return H.class.getField(n);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    static Field dfld(Class<?> c, String n) {
        try {
            return c.getDeclaredField(n);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    static void fields() {
        H h = new H();
        for (String n : new String[] {"z", "b", "c", "s", "i", "l", "f", "d", "o", "str", "arr", "si", "so", "fin", "SFIN", "vol", "tr", "bi"}) {
            Field f = fld(n);
            row("get." + n, t(() -> f.get(h)));
            row("getInt." + n, t(() -> f.getInt(h)));
            row("getLong." + n, t(() -> f.getLong(h)));
            row("getDouble." + n, t(() -> f.getDouble(h)));
            row("getBoolean." + n, t(() -> f.getBoolean(h)));
            row("getChar." + n, t(() -> f.getChar(h)));
            row("getByte." + n, t(() -> f.getByte(h)));
            row("getShort." + n, t(() -> f.getShort(h)));
            row("getFloat." + n, t(() -> f.getFloat(h)));
        }
        row("get.static null", t(() -> fld("si").get(null)));
        row("get.static wrong", t(() -> fld("si").get("x")));
        row("get.inst null", t(() -> fld("i").get(null)));
        row("get.inst wrong", t(() -> fld("i").get("x")));
        row("get.inst base recv", t(() -> fld("i").get(new Base())));
        row("get.base on sub", t(() -> Base.class.getField("bi").get(h)));
        row("getInt.inst null", t(() -> fld("i").getInt(null)));
        row("set.z", t(() -> { fld("z").set(h, false); return h.z; }));
        row("set.z int", t(() -> { fld("z").set(h, 1); return h.z; }));
        row("setBoolean.z", t(() -> { fld("z").setBoolean(h, false); return h.z; }));
        row("setBoolean.i", t(() -> { fld("i").setBoolean(h, false); return h.i; }));
        row("set.b", t(() -> { fld("b").set(h, (byte) 5); return h.b; }));
        row("set.b int", t(() -> { fld("b").set(h, 5); return h.b; }));
        row("setByte.i", t(() -> { fld("i").setByte(h, (byte) 5); return h.i; }));
        row("set.c", t(() -> { fld("c").set(h, 'x'); return h.c; }));
        row("setChar.i", t(() -> { fld("i").setChar(h, 'x'); return h.i; }));
        row("set.s", t(() -> { fld("s").set(h, (short) 5); return h.s; }));
        row("setShort.l", t(() -> { fld("l").setShort(h, (short) 5); return h.l; }));
        row("set.i", t(() -> { fld("i").set(h, 5); return h.i; }));
        row("set.i long", t(() -> { fld("i").set(h, 5L); return h.i; }));
        row("set.i char", t(() -> { fld("i").set(h, 'a'); return h.i; }));
        row("setInt.l", t(() -> { fld("l").setInt(h, 5); return h.l; }));
        row("setInt.f", t(() -> { fld("f").setInt(h, 5); return h.f; }));
        row("setInt.d", t(() -> { fld("d").setInt(h, 5); return h.d; }));
        row("setInt.o", t(() -> { fld("o").setInt(h, 5); return h.o; }));
        row("set.l", t(() -> { fld("l").set(h, 5); return h.l; }));
        row("setLong.f", t(() -> { fld("f").setLong(h, 5L); return h.f; }));
        row("set.f", t(() -> { fld("f").set(h, 5); return h.f; }));
        row("setFloat.d", t(() -> { fld("d").setFloat(h, 1.5f); return h.d; }));
        row("set.d", t(() -> { fld("d").set(h, 1.5f); return h.d; }));
        row("setDouble.f", t(() -> { fld("f").setDouble(h, 1.5); return h.f; }));
        row("set.o", t(() -> { fld("o").set(h, "y"); return h.o; }));
        row("set.o null", t(() -> { fld("o").set(h, null); return h.o; }));
        row("set.str wrong", t(() -> { fld("str").set(h, 5); return h.str; }));
        row("set.arr wrong", t(() -> { fld("arr").set(h, new long[1]); return h.arr; }));
        row("set.i null", t(() -> { fld("i").set(h, null); return h.i; }));
        row("set.si", t(() -> { fld("si").set(null, 10); return H.si; }));
        row("set.so", t(() -> { fld("so").set(null, "z"); return H.so; }));
        row("set.fin", t(() -> { fld("fin").set(h, 99); return h.fin; }));
        row("set.SFIN", t(() -> { fld("SFIN").set(null, 99); return H.SFIN; }));
        row("set.vol", t(() -> { fld("vol").setInt(h, 99); return h.vol; }));
        row("set.inst null", t(() -> { fld("i").setInt(null, 1); return 0; }));
        row("set.inst wrong", t(() -> { fld("i").setInt("x", 1); return 0; }));
        // access checks
        row("priv.get", t(() -> dfld(H.class, "priv").getInt(h)));
        row("priv.set", t(() -> { dfld(H.class, "priv").setInt(h, 1); return 0; }));
        row("pkg.get", t(() -> dfld(H.class, "pkg").getInt(h)));
        row("base.priv.get", t(() -> dfld(Base.class, "bp").get(new Base())));
        row("base.prot.get", t(() -> dfld(Base.class, "bl").get(new Base())));
        row("hidden.get", t(() -> dfld(Hidden.class, "pubInHidden").getInt(new Hidden())));
        row("jdk.private.get", t(() -> dfld(String.class, "value").get("abc")));
        row("jdk.private.setAcc", t(() -> {
            Field f = dfld(String.class, "value");
            f.setAccessible(true);
            return f.get("abc");
        }));
        row("jdk.pkgpriv.get", t(() -> dfld(java.util.ArrayList.class, "size").getInt(new ArrayList<>(List.of(1, 2)))));
        row("jdk.pub.static.get", t(() -> Integer.class.getField("MAX_VALUE").getInt(null)));
        row("jdk.pub.static.set", t(() -> { Integer.class.getField("MAX_VALUE").setInt(null, 1); return 0; }));
        Field pa = dfld(H.class, "priv");
        pa.setAccessible(true);
        row("priv.acc.get", t(() -> pa.getInt(h)));
        row("priv.acc.set", t(() -> { pa.setInt(h, 21); return h.priv; }));
        row("priv.acc.get null", t(() -> pa.getInt(null)));
        row("priv.acc.get wrong", t(() -> pa.getInt("x")));
        Field fa = dfld(H.class, "fin");
        fa.setAccessible(true);
        row("fin.acc.set", t(() -> { fa.setInt(h, 31); return h.fin; }));
        Field sfa = dfld(H.class, "SFIN");
        sfa.setAccessible(true);
        row("sfin.acc.set", t(() -> { sfa.setInt(null, 31); return H.SFIN; }));
        row("get inst on rec", t(() -> {
            record R(int x) {}
            Field f = R.class.getDeclaredField("x");
            f.setAccessible(true);
            return f.getInt(new R(3));
        }));
        row("modifiers", t(() -> Modifier.toString(fld("vol").getModifiers())));
        row("canAccess", t(() -> fld("i").canAccess(h)));
        row("set-accessible-false.get", t(() -> {
            Field f = fld("i");
            f.setAccessible(true);
            f.setAccessible(false);
            return f.getInt(h);
        }));
        // getCallerClass reached through wrappers
        row("via.lambda", t(() -> { Supplier<Object> s = () -> { try { return dfld(H.class, "priv").getInt(h); } catch (Exception e) { return e.getMessage(); } }; return s.get(); }));
        row("via.stream", t(() -> java.util.stream.Stream.of(1).map(x -> { try { return dfld(H.class, "priv").getInt(h); } catch (Exception e) { return e.getMessage(); } }).collect(Collectors.toList())));
        row("via.method", t(() -> Sbr2W7CallerMix.class.getDeclaredMethod("viaMethod", H.class).invoke(null, h)));
    }

    static Object viaMethod(H h) throws Exception {
        try {
            return dfld(H.class, "priv").getInt(h);
        } catch (Exception e) {
            return e.getMessage();
        }
    }

    static byte[] K16 = new byte[16];
    static byte[] K32 = new byte[32];
    static byte[] DATA = new byte[300];

    static {
        for (int i = 0; i < 300; i++) DATA[i] = (byte) (i * 7 + 1);
        for (int i = 0; i < 32; i++) { K32[i] = (byte) i; if (i < 16) K16[i] = (byte) (i + 100); }
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(String.format("%02x", x));
        return sb.toString();
    }

    static void macs() {
        for (String a : new String[] {"HmacSHA1", "HmacSHA224", "HmacSHA256", "HmacSHA384", "HmacSHA512", "HmacMD5", "HmacSHA512/256", "HmacSHA3-256", "hmacsha256", "PBEWithHmacSHA256", "HmacNOPE", "", "AES"}) {
            row("mac.new." + a, t(() -> { Mac m = Mac.getInstance(a); m.init(new SecretKeySpec(K32, a.isEmpty() ? "X" : "RAW")); m.update(DATA); return m.getAlgorithm() + "/" + m.getMacLength() + "/" + m.getProvider().getName() + "/" + hex(m.doFinal()); }));
            row("mac.prov." + a, t(() -> Mac.getInstance(a, "SunJCE").getMacLength()));
            row("mac.provobj." + a, t(() -> Mac.getInstance(a, Security.getProvider("SunJCE")).getAlgorithm()));
        }
        row("mac.badprov", t(() -> Mac.getInstance("HmacSHA256", "NoSuchProv")));
        row("mac.nullprov", t(() -> Mac.getInstance("HmacSHA256", (String) null)));
        row("mac.nullprovobj", t(() -> Mac.getInstance("HmacSHA256", (java.security.Provider) null)));
        row("mac.nullalg", t(() -> Mac.getInstance(null)));
        row("mac.noinit.final", t(() -> Mac.getInstance("HmacSHA256").doFinal()));
        row("mac.noinit.update", t(() -> { Mac.getInstance("HmacSHA256").update(DATA); return 0; }));
        row("mac.noinit.updatebyte", t(() -> { Mac.getInstance("HmacSHA256").update((byte) 1); return 0; }));
        row("mac.nullkey", t(() -> { Mac.getInstance("HmacSHA256").init(null); return 0; }));
        row("mac.badspec", t(() -> { Mac.getInstance("HmacSHA256").init(new SecretKeySpec(K16, "H"), new IvParameterSpec(K16)); return 0; }));
        row("mac.emptykey", t(() -> { Mac.getInstance("HmacSHA256").init(new SecretKeySpec(new byte[1], "H")); return 0; }));
        row("mac.nullupdate", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.update((byte[]) null); return 0; }));
        row("mac.nullbb", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.update((ByteBuffer) null); return 0; }));
        row("mac.badoff", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.update(DATA, 290, 20); return 0; }));
        row("mac.badoff2", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.update(DATA, -1, 2); return 0; }));
        row("mac.shortout", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.doFinal(new byte[8], 0); return 0; }));
        row("mac.into", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); byte[] b = new byte[40]; m.doFinal(b, 4); return hex(b); }));
        row("mac.doFinalArg", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); return hex(m.doFinal(DATA)); }));
        row("mac.reuse", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.doFinal(DATA); m.update(K16); return hex(m.doFinal()); }));
        row("mac.reset", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.update(DATA); m.reset(); return hex(m.doFinal(K16)); }));
        row("mac.clone", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); m.update(DATA, 0, 50); Mac n = (Mac) m.clone(); m.update((byte) 1); return hex(m.doFinal()) + "/" + hex(n.doFinal()); }));
        row("mac.clone.uninit", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); Mac n = (Mac) m.clone(); return n.getMacLength(); }));
        row("mac.bb", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); ByteBuffer b = ByteBuffer.wrap(DATA); b.position(5); m.update(b); return hex(m.doFinal()) + b.remaining(); }));
        row("mac.direct.bb", t(() -> { Mac m = Mac.getInstance("HmacSHA256"); m.init(new SecretKeySpec(K16, "H")); ByteBuffer b = ByteBuffer.allocateDirect(64); b.put(DATA, 0, 64).flip(); m.update(b); return hex(m.doFinal()) + b.remaining(); }));
        row("mac.class", t(() -> Mac.getInstance("HmacSHA256").getClass().getName()));
        row("mac.tostring", t(() -> Mac.getInstance("HmacSHA256").toString().startsWith("javax.crypto.Mac@")));
        row("mac.algs", t(() -> Security.getAlgorithms("Mac").contains("HMACSHA256")));
        row("mac.svc", t(() -> Security.getProvider("SunJCE").getService("Mac", "HmacSHA256").getClassName()));
        row("mac.key.destroyed", t(() -> {
            SecretKeySpec k = new SecretKeySpec(K16, "H");
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(k);
            return hex(m.doFinal(DATA));
        }));
    }

    static Object walkFrames(int depth) {
        if (depth > 0) return walkFrames(depth - 1);
        StackWalker sw = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
        return sw.walk(s -> s.limit(4).map(f -> f.getClassName() + "." + f.getMethodName() + ":" + f.getByteCodeIndex() + "/" + f.getDeclaringClass().getSimpleName()).collect(Collectors.toList()));
    }

    static Object callerOf() {
        return StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).getCallerClass().getName();
    }

    static class Inner {
        static Object callerOfInner() {
            return callerOf();
        }
    }

    static void walkers() {
        row("sw.callerClass", t(() -> callerOf()));
        row("sw.callerClass.inner", t(() -> Inner.callerOfInner()));
        row("sw.callerClass.noRetain", t(() -> StackWalker.getInstance().getCallerClass()));
        row("sw.callerClass.lambda", t(() -> { Supplier<Object> s = () -> callerOf(); return s.get(); }));
        row("sw.callerClass.stream", t(() -> java.util.stream.Stream.of(1).map(x -> callerOf()).collect(Collectors.toList())));
        row("sw.callerClass.method", t(() -> Sbr2W7CallerMix.class.getDeclaredMethod("callerOf").invoke(null)));
        row("sw.callerClass.thread", t(() -> { Object[] r = new Object[1]; Thread th = new Thread(() -> r[0] = callerOf(), "w"); th.start(); th.join(); return r[0]; }));
        row("sw.walk.count", t(() -> StackWalker.getInstance().walk(s -> s.count() > 0)));
        row("sw.walk.top", t(() -> StackWalker.getInstance().walk(s -> s.findFirst().map(f -> f.getClassName() + "." + f.getMethodName()).orElse("none"))));
        row("sw.walk.d0", t(() -> walkFrames(0)));
        row("sw.walk.d5", t(() -> walkFrames(5)));
        row("sw.walk.d40", t(() -> StackWalker.getInstance().walk(s -> s.limit(2).count())));
        row("sw.walk.deep", t(() -> deepCount(60)));
        row("sw.walk.skip", t(() -> StackWalker.getInstance().walk(s -> s.skip(1).findFirst().map(StackWalker.StackFrame::getMethodName).orElse("none"))));
        row("sw.walk.ste", t(() -> StackWalker.getInstance().walk(s -> s.findFirst().get().toStackTraceElement().getMethodName())));
        row("sw.walk.desc", t(() -> StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES).walk(s -> s.findFirst().get().getDescriptor())));
        row("sw.walk.nodecl", t(() -> StackWalker.getInstance().walk(s -> s.findFirst().get().getDeclaringClass())));
        row("sw.walk.escape", t(() -> { Object[] r = new Object[1]; StackWalker.getInstance().walk(s -> { r[0] = s; return 0; }); return ((java.util.stream.Stream<?>) r[0]).count(); }));
        row("sw.walk.null", t(() -> StackWalker.getInstance().walk(null)));
        row("sw.forEach", t(() -> { List<String> l = new ArrayList<>(); StackWalker.getInstance().forEach(f -> { if (l.size() < 3) l.add(f.getMethodName()); }); return l; }));
        row("sw.forEach.null", t(() -> { StackWalker.getInstance().forEach(null); return 0; }));
        row("sw.getInstance.set", t(() -> StackWalker.getInstance(Set.of(StackWalker.Option.RETAIN_CLASS_REFERENCE, StackWalker.Option.SHOW_REFLECT_FRAMES)).getCallerClass().getName()));
        row("sw.getInstance.depth", t(() -> StackWalker.getInstance(Set.of(), 3).walk(s -> s.count() > 0)));
        row("sw.getInstance.baddepth", t(() -> StackWalker.getInstance(Set.of(), 0)));
        row("sw.reflect.frames", t(() -> {
            try {
                return Sbr2W7CallerMix.class.getDeclaredMethod("reflectWalk").invoke(null);
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        }));
    }

    static Object reflectWalk() {
        return StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES).walk(s -> s.limit(3).map(f -> f.getClassName().replaceAll("\\$\\$Lambda.*", "L") + "." + f.getMethodName()).collect(Collectors.toList()));
    }

    static Object deepCount(int d) {
        if (d > 0) return deepCount(d - 1);
        return StackWalker.getInstance().walk(s -> s.filter(f -> f.getClassName().equals("Sbr2W7CallerMix")).count());
    }

    static void all() {
        fields();
        macs();
        walkers();
    }

    public static void main(String[] args) throws Exception {
        int hot = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        all();
        for (String s : COLD) System.out.println(s);
        out = HOT;
        for (int i = 0; i < hot; i++) {
            HOT.clear();
            all();
        }
        int drift = 0;
        for (int i = 0; i < COLD.size() && i < HOT.size(); i++) {
            if (!COLD.get(i).equals(HOT.get(i))) {
                drift++;
                System.out.println("DRIFT cold: " + COLD.get(i));
                System.out.println("DRIFT hot : " + HOT.get(i));
            }
        }
        System.out.println("rows " + COLD.size() + " hotrows " + HOT.size() + " drift " + drift);
    }
}
