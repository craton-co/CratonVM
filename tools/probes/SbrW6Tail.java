// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayInputStream;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.management.BufferPoolMXBean;
import java.lang.management.ManagementFactory;
import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.nio.ByteBuffer;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;
import javax.crypto.KeyAgreement;
import javax.crypto.SecretKey;
import javax.crypto.spec.IvParameterSpec;

/**
 * SBR wave 6, lane {@code tail}: {@code java.lang.reflect.Proxy} (+
 * {@code InvocationHandler.invokeDefault}), {@code jdk.internal.misc.Unsafe}
 * wrapper rows (reached through {@code sun.misc.Unsafe}), {@code KeyAgreement},
 * {@code IvParameterSpec}, {@code ByteArrayInputStream}, {@code reflect.Array},
 * and the buffer accounting rows.
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value), then
 * re-evaluated inside a hot loop and compared with it; {@code drift} counts
 * mismatches, so cold == warm is asserted. Diff the whole output against
 * HotSpot 25. Digits in proxy class names are masked; no identity hashes.
 *
 * <pre>
 *   java SbrW6Tail
 *   cratonvm --jdk-only -cp . SbrW6Tail
 * </pre>
 */
public class SbrW6Tail {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    interface X {
        Object get() throws Throwable;
    }

    static void rowx(String label, X x) {
        LABELS.add(label);
        ROWS.add(() -> {
            try {
                return mask(String.valueOf(x.get()));
            } catch (Throwable t) {
                Throwable c = t;
                return "!" + c.getClass().getName() + ": " + mask(String.valueOf(c.getMessage()));
            }
        });
    }

    static String mask(String s) {
        return s.replaceAll("\\$Proxy[0-9]+", "\\$ProxyN").replaceAll("jdk\\.proxy[0-9]+", "jdk.proxyN")
                .replaceAll("@[0-9a-f]{3,}", "@H");
    }

    public interface Greeter {
        String greet(String n);

        default String dflt(String n) {
            return "default:" + n;
        }

        default String twice(String n) {
            return greet(n) + greet(n);
        }
    }

    public interface Other {
        int num();
    }

    interface Hidden {
        String hid();
    }

    public @interface Ann {
    }

    @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)
    public @interface Tag {
        String value();
    }

    @Tag("x")
    public static class Tagged {
        public int b;
    }

    static final InvocationHandler H = (p, m, a) -> {
        if (m.getName().equals("greet")) return "hi " + a[0];
        if (m.getName().equals("num")) return 42;
        if (m.getName().equals("hid")) return "hidden";
        if (m.isDefault()) return InvocationHandler.invokeDefault(p, m, a);
        if (m.getName().equals("hashCode")) return 7;
        if (m.getName().equals("equals")) return p == a[0];
        if (m.getName().equals("toString")) return "P";
        return null;
    };

    static sun.misc.Unsafe unsafe() throws Exception {
        Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        return (sun.misc.Unsafe) f.get(null);
    }

    static final StringBuilder LOG = new StringBuilder();

    static class InitProbe {
        static int v;

        static {
            LOG.append("clinit");
            v = 5;
        }
    }

    static byte bfield;
    static int ifield = 5;
    long lf;

    public static void main(String[] args) throws Exception {
        ClassLoader cl = Greeter.class.getClassLoader();
        Class<?>[] g = {Greeter.class};

        // ---------------- Proxy
        rowx("proxy.new+call", () -> ((Greeter) Proxy.newProxyInstance(cl, g, H)).greet("a"));
        rowx("proxy.same class twice", () -> Proxy.newProxyInstance(cl, g, H).getClass() == Proxy.newProxyInstance(cl, g, H).getClass());
        rowx("proxy.two-iface same class", () -> Proxy.newProxyInstance(cl, new Class<?>[] {Greeter.class, Other.class}, H).getClass()
                == Proxy.newProxyInstance(cl, new Class<?>[] {Greeter.class, Other.class}, H).getClass());
        rowx("proxy.two-iface order differs", () -> Proxy.newProxyInstance(cl, new Class<?>[] {Greeter.class, Other.class}, H).getClass()
                == Proxy.newProxyInstance(cl, new Class<?>[] {Other.class, Greeter.class}, H).getClass());
        rowx("proxy.two-iface call", () -> {
            Object o = Proxy.newProxyInstance(cl, new Class<?>[] {Greeter.class, Other.class}, H);
            return ((Greeter) o).greet("z") + ((Other) o).num();
        });
        rowx("proxy.isProxyClass proxy", () -> Proxy.isProxyClass(Proxy.newProxyInstance(cl, g, H).getClass()));
        rowx("proxy.isProxyClass Object", () -> Proxy.isProxyClass(Object.class));
        rowx("proxy.isProxyClass Proxy", () -> Proxy.isProxyClass(Proxy.class));
        rowx("proxy.isProxyClass null", () -> Proxy.isProxyClass(null));
        rowx("proxy.getInvocationHandler", () -> Proxy.getInvocationHandler(Proxy.newProxyInstance(cl, g, H)) == H);
        rowx("proxy.getInvocationHandler non-proxy", () -> Proxy.getInvocationHandler("x"));
        rowx("proxy.getInvocationHandler null", () -> Proxy.getInvocationHandler(null));
        rowx("proxy.class name/pkg", () -> {
            Class<?> c = Proxy.newProxyInstance(cl, g, H).getClass();
            return c.getName() + " " + c.getPackageName() + " " + java.lang.reflect.Modifier.toString(c.getModifiers());
        });
        rowx("proxy.module", () -> {
            Module m = Proxy.newProxyInstance(cl, g, H).getClass().getModule();
            return (m.isNamed() ? m.getName().replaceAll("[0-9]+", "N") : "unnamed") + " " + m.isNamed();
        });
        rowx("proxy.hidden iface", () -> {
            Object o = Proxy.newProxyInstance(Hidden.class.getClassLoader(), new Class<?>[] {Hidden.class}, H);
            Class<?> c = o.getClass();
            return c.getPackageName().isEmpty() + " " + java.lang.reflect.Modifier.isPublic(c.getModifiers()) + " " + ((Hidden) o).hid();
        });
        rowx("proxy.getProxyClass", () -> {
            @SuppressWarnings("deprecation")
            Class<?> c = Proxy.getProxyClass(cl, Greeter.class);
            Greeter x = (Greeter) c.getConstructor(InvocationHandler.class).newInstance(H);
            return x.greet("pc") + " " + Proxy.isProxyClass(c);
        });
        rowx("proxy.getProxyClass == newProxyInstance class", () -> {
            @SuppressWarnings("deprecation")
            Class<?> c = Proxy.getProxyClass(cl, Greeter.class);
            return c == Proxy.newProxyInstance(cl, g, H).getClass();
        });
        rowx("proxy.null handler", () -> Proxy.newProxyInstance(cl, g, null));
        rowx("proxy.non-interface", () -> Proxy.newProxyInstance(cl, new Class<?>[] {String.class}, H));
        rowx("proxy.dup interface", () -> Proxy.newProxyInstance(cl, new Class<?>[] {Greeter.class, Greeter.class}, H));
        rowx("proxy.null interfaces", () -> Proxy.newProxyInstance(cl, null, H));
        rowx("proxy.interfaces", () -> java.util.Arrays.toString(Proxy.newProxyInstance(cl, new Class<?>[] {Greeter.class, Other.class}, H).getClass().getInterfaces()));
        rowx("proxy.methods", () -> {
            List<String> l = new ArrayList<>();
            for (Method m : Proxy.newProxyInstance(cl, g, H).getClass().getDeclaredMethods()) l.add(m.getName());
            java.util.Collections.sort(l);
            return l;
        });
        rowx("proxy.equals/hashCode/toString", () -> {
            Object o = Proxy.newProxyInstance(cl, g, H);
            return o.equals(o) + " " + o.hashCode() + " " + o;
        });
        rowx("proxy.undeclared checked", () -> {
            InvocationHandler bad = (p, m, a) -> {
                throw new java.io.IOException("io");
            };
            return ((Greeter) Proxy.newProxyInstance(cl, g, bad)).greet("x");
        });
        rowx("proxy.invokeDefault", () -> ((Greeter) Proxy.newProxyInstance(cl, g, H)).dflt("q"));
        rowx("proxy.invokeDefault calling abstract", () -> ((Greeter) Proxy.newProxyInstance(cl, g, H)).twice("q"));
        rowx("proxy.invokeDefault non-proxy", () -> InvocationHandler.invokeDefault("x", Greeter.class.getMethod("dflt", String.class), "a"));
        rowx("proxy.invokeDefault abstract method", () -> {
            Greeter p = (Greeter) Proxy.newProxyInstance(cl, g, H);
            return InvocationHandler.invokeDefault(p, Greeter.class.getMethod("greet", String.class), "a");
        });
        rowx("proxy.annotation isProxyClass", () -> {
            Tag t = Tagged.class.getAnnotation(Tag.class);
            return t.value() + " " + Proxy.isProxyClass(t.getClass()) + " " + t.annotationType().getName();
        });
        rowx("proxy.annotation handler", () -> {
            Tag t = Tagged.class.getAnnotation(Tag.class);
            return Proxy.getInvocationHandler(t).getClass().getName();
        });
        rowx("proxy.other loader", () -> {
            ClassLoader l2 = new ClassLoader(cl) {
            };
            Object a = Proxy.newProxyInstance(l2, g, H);
            Object b = Proxy.newProxyInstance(cl, g, H);
            return a.getClass().getClassLoader() == l2 ? (a.getClass() == b.getClass()) + "/l2" : "loader:" + (a.getClass() == b.getClass());
        });

        // ---------------- Unsafe (through sun.misc.Unsafe)
        rowx("unsafe.objectFieldOffset>0", () -> unsafe().objectFieldOffset(SbrW6Tail.class.getDeclaredField("lf")) >= 0);
        rowx("unsafe.objectFieldOffset static (IAE)", () -> unsafe().objectFieldOffset(SbrW6Tail.class.getDeclaredField("ifield")));
        rowx("unsafe.staticFieldOffset/Base roundtrip", () -> {
            sun.misc.Unsafe u = unsafe();
            Field f = SbrW6Tail.class.getDeclaredField("ifield");
            Object base = u.staticFieldBase(f);
            long off = u.staticFieldOffset(f);
            int before = u.getInt(base, off);
            u.putInt(base, off, 77);
            int mid = ifield;
            u.putInt(base, off, before);
            return before + " " + mid + " " + ifield;
        });
        rowx("unsafe.staticFieldOffset instance", () -> unsafe().staticFieldOffset(SbrW6Tail.class.getDeclaredField("lf")));
        rowx("unsafe.arrayIndexScale", () -> {
            sun.misc.Unsafe u = unsafe();
            return u.arrayIndexScale(byte[].class) + " " + u.arrayIndexScale(int[].class) + " " + u.arrayIndexScale(long[].class)
                    + " " + u.arrayIndexScale(Object[].class);
        });
        rowx("unsafe.arrayIndexScale non-array", () -> unsafe().arrayIndexScale(String.class));
        rowx("unsafe.alloc/put/get/free", () -> {
            sun.misc.Unsafe u = unsafe();
            long a = u.allocateMemory(16);
            u.putLong(a, 0x0102030405060708L);
            u.putByte(a + 8, (byte) 9);
            String r = Long.toHexString(u.getLong(a)) + " " + u.getByte(a + 8) + " " + u.getByte(a);
            u.freeMemory(a);
            return r;
        });
        rowx("unsafe.allocateMemory negative", () -> unsafe().allocateMemory(-1));
        rowx("unsafe.allocateMemory zero", () -> unsafe().allocateMemory(0));
        rowx("varhandle static init", () -> {
            LOG.setLength(0);
            VarHandle vh = MethodHandles.lookup().findStaticVarHandle(InitProbe.class, "v", int.class);
            LOG.append("|found");
            int v = (int) vh.get();
            return LOG + " " + v;
        });
        rowx("unsafe.compareAndSwapInt", () -> {
            sun.misc.Unsafe u = unsafe();
            Field f = SbrW6Tail.class.getDeclaredField("ifield");
            Object base = u.staticFieldBase(f);
            long off = u.staticFieldOffset(f);
            int cur = u.getInt(base, off);
            boolean a = u.compareAndSwapInt(base, off, cur, cur + 1);
            boolean b = u.compareAndSwapInt(base, off, cur, cur + 2);
            int now = ifield;
            u.putInt(base, off, cur);
            return a + " " + b + " " + (now - cur);
        });
        rowx("varhandle byte CAS", () -> {
            VarHandle vh = MethodHandles.lookup().findStaticVarHandle(SbrW6Tail.class, "bfield", byte.class);
            bfield = 3;
            boolean a = vh.compareAndSet((byte) 3, (byte) 8);
            boolean b = vh.compareAndSet((byte) 3, (byte) 9);
            byte r = bfield;
            bfield = 0;
            return a + " " + b + " " + r;
        });
        rowx("varhandle byte array CAS", () -> {
            VarHandle vh = MethodHandles.arrayElementVarHandle(byte[].class);
            byte[] arr = {1, 2, 3};
            boolean a = vh.compareAndSet(arr, 1, (byte) 2, (byte) 20);
            boolean b = vh.compareAndSet(arr, 1, (byte) 2, (byte) 30);
            return a + " " + b + " " + java.util.Arrays.toString(arr);
        });

        // ---------------- IvParameterSpec / ByteArrayInputStream / Array
        rowx("iv.getIV copy", () -> {
            byte[] b = {1, 2, 3};
            IvParameterSpec s = new IvParameterSpec(b);
            b[0] = 9;
            byte[] r = s.getIV();
            r[1] = 8;
            return java.util.Arrays.toString(s.getIV()) + java.util.Arrays.toString(r);
        });
        rowx("iv.slice", () -> java.util.Arrays.toString(new IvParameterSpec(new byte[] {1, 2, 3, 4}, 1, 2).getIV()));
        rowx("iv.null", () -> new IvParameterSpec(null));
        rowx("iv.bad slice", () -> new IvParameterSpec(new byte[2], 1, 5));
        rowx("bais.read all", () -> {
            ByteArrayInputStream in = new ByteArrayInputStream(new byte[] {(byte) 200, 2, 3});
            return in.read() + " " + in.read() + " " + in.read() + " " + in.read() + " " + in.available();
        });
        rowx("bais.read bulk", () -> {
            ByteArrayInputStream in = new ByteArrayInputStream(new byte[] {1, 2, 3, 4, 5}, 1, 3);
            byte[] d = new byte[6];
            int a = in.read(d, 1, 4);
            return a + " " + java.util.Arrays.toString(d) + " " + in.read(d, 0, 1) + " " + in.read(d, 0, 0);
        });
        rowx("bais.read bulk bounds", () -> new ByteArrayInputStream(new byte[3]).read(new byte[2], 1, 5));
        rowx("bais.read null dest", () -> new ByteArrayInputStream(new byte[3]).read(null, 0, 1));
        rowx("bais.close/mark/reset", () -> {
            ByteArrayInputStream in = new ByteArrayInputStream(new byte[] {1, 2, 3});
            in.read();
            in.mark(0);
            in.read();
            in.close();
            in.reset();
            return in.read() + " " + in.markSupported();
        });
        rowx("bais.readAllBytes/transferTo/skip", () -> {
            ByteArrayInputStream in = new ByteArrayInputStream(new byte[] {1, 2, 3, 4, 5});
            long s = in.skip(2);
            java.io.ByteArrayOutputStream o = new java.io.ByteArrayOutputStream();
            long t = in.transferTo(o);
            return s + " " + t + " " + java.util.Arrays.toString(o.toByteArray());
        });
        rowx("array.newInstance", () -> Array.newInstance(int.class, 3).getClass().getName() + Array.getLength(Array.newInstance(String.class, 2)));
        rowx("array.newInstance multi", () -> {
            Object o = Array.newInstance(int.class, 2, 3);
            return o.getClass().getName() + Array.getLength(Array.get(o, 1));
        });
        rowx("array.newInstance void", () -> Array.newInstance(void.class, 1));
        rowx("array.newInstance neg", () -> Array.newInstance(int.class, -1));
        rowx("array.newInstance null", () -> Array.newInstance(null, 1));
        rowx("array.newInstance dims empty", () -> Array.newInstance(int.class, new int[0]));
        rowx("array.newInstance dims neg", () -> Array.newInstance(int.class, new int[] {2, -1}));
        rowx("array.newInstance 256 dims", () -> Array.newInstance(int.class, new int[255]).getClass().getName().length());
        rowx("array.newInstance 300 dims", () -> Array.newInstance(int.class, new int[300]));

        // ---------------- KeyAgreement (ECDH / DH, X25519)
        rowx("ka.ecdh", () -> {
            KeyPairGenerator kg = KeyPairGenerator.getInstance("EC");
            kg.initialize(256);
            KeyPair a = kg.generateKeyPair();
            KeyPair b = kg.generateKeyPair();
            KeyAgreement ka = KeyAgreement.getInstance("ECDH");
            ka.init(a.getPrivate());
            ka.doPhase(b.getPublic(), true);
            byte[] s1 = ka.generateSecret();
            KeyAgreement kb = KeyAgreement.getInstance("ECDH");
            kb.init(b.getPrivate());
            kb.doPhase(a.getPublic(), true);
            return ka.getAlgorithm() + " " + s1.length + " " + java.util.Arrays.equals(s1, kb.generateSecret()) + " " + ka.getProvider().getName();
        });
        rowx("ka.x25519", () -> {
            KeyPairGenerator kg = KeyPairGenerator.getInstance("X25519");
            KeyPair a = kg.generateKeyPair();
            KeyPair b = kg.generateKeyPair();
            KeyAgreement ka = KeyAgreement.getInstance("X25519");
            ka.init(a.getPrivate());
            ka.doPhase(b.getPublic(), true);
            SecretKey k = ka.generateSecret("AES");
            return k.getAlgorithm() + " " + k.getEncoded().length;
        });
        rowx("ka.dh", () -> {
            KeyPairGenerator kg = KeyPairGenerator.getInstance("DH");
            kg.initialize(2048);
            KeyPair a = kg.generateKeyPair();
            KeyPair b = kg.generateKeyPair();
            KeyAgreement ka = KeyAgreement.getInstance("DH");
            ka.init(a.getPrivate());
            ka.doPhase(b.getPublic(), true);
            byte[] out = new byte[300];
            int n = ka.generateSecret(out, 0);
            return n;
        });
        rowx("ka.no such alg", () -> KeyAgreement.getInstance("NOPE"));
        rowx("ka.no such provider", () -> KeyAgreement.getInstance("ECDH", "NOPE"));
        rowx("ka.not inited", () -> KeyAgreement.getInstance("ECDH").generateSecret());
        rowx("ka.doPhase before init", () -> KeyAgreement.getInstance("ECDH").doPhase(null, true));
        rowx("ka.init null key", () -> {
            KeyAgreement.getInstance("ECDH").init(null);
            return "no throw";
        });

        // ---------------- buffer accounting
        rowx("bits.direct pool count/used", () -> {
            BufferPoolMXBean p = null;
            for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) {
                if (b.getName().equals("direct")) p = b;
            }
            long c0 = p.getCount();
            long u0 = p.getMemoryUsed();
            ByteBuffer bb = ByteBuffer.allocateDirect(4096);
            long c1 = p.getCount();
            long u1 = p.getMemoryUsed();
            long cap = p.getTotalCapacity();
            bb.put((byte) 1);
            return (c1 - c0) + " " + (u1 - u0 >= 4096) + " " + (cap >= 4096) + " " + p.getName();
        });
        rowx("buffer.direct/heap props", () -> {
            ByteBuffer d = ByteBuffer.allocateDirect(8);
            ByteBuffer h = ByteBuffer.allocate(8);
            return d.isDirect() + " " + h.isDirect() + " " + d.isReadOnly() + " " + h.asReadOnlyBuffer().isReadOnly()
                    + " " + d.hasArray() + " " + h.hasArray() + " " + d.slice().isDirect();
        });
        rowx("buffer.direct content", () -> {
            ByteBuffer d = ByteBuffer.allocateDirect(16);
            d.putInt(0, 0x01020304);
            d.putLong(8, 0x1122334455667788L);
            return Integer.toHexString(d.getInt(0)) + " " + Long.toHexString(d.getLong(8)) + " " + d.get(3);
        });

        // ------------------------------------------------ run
        int n = ROWS.size();
        String[] first = new String[n];
        for (int i = 0; i < n; i++) {
            first[i] = ROWS.get(i).get();
            System.out.println(LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        for (int round = 0; round < 40; round++) {
            for (int i = 0; i < n; i++) {
                if (LABELS.get(i).equals("ka.dh") || LABELS.get(i).equals("varhandle static init")) continue;
                String s = ROWS.get(i).get();
                if (!s.equals(first[i])) {
                    drift++;
                    if (drift <= 10) System.out.println("DRIFT " + LABELS.get(i) + ": " + first[i] + " -> " + s);
                }
            }
        }
        System.out.println("rows=" + n + " drift=" + drift);
    }
}
