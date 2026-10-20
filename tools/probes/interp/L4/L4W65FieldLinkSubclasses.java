// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 65, lane L4 -- the field-accessor link admits
// every subclass of the handle's owner (`CRATONVM_MH_FIELD_LINK_OWNER_ADMITS`,
// opt-in, performance; proposal
// `i64-L4-proposal-field-link-admits-the-owners-subclasses-20261028.md`).
//
// A performance change: every row must print the same with and without the
// switch, in both modes, and match HotSpot. One handle over `Base` is read
// and written through `invokeExact` with receivers of `Base` and four
// subclasses in rotation (more alternations than the seven relinks a
// one-class link allows), including subclasses that HIDE `Base.v` (the
// handle must keep reading `Base`'s slot), and a reference setter whose value
// is cast (`CharSequence`, with `String` and `StringBuilder` values). After
// the warm loops, the casts the link must never skip: another loader's `Base`
// and its subclass (`ClassCastException`, not a read of loader B's slot), a
// `String`, a `null` (the message-less NPE).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// namesakes: true true
// poly-get: 5000
// poly-get-object: 5000
// hidden-sub2: 1/2
// poly-set: 0+1+2+3+4
// hidden-after-set: 2/2/4
// poly-cs: x,sb,x,sb,x
// warm: 120000
// after-poly-get: 5
// foreign: java.lang.ClassCastException: Cannot cast L4W65FieldLinkSubclasses$Base to L4W65FieldLinkSubclasses$Base
// foreign-sub: java.lang.ClassCastException: Cannot cast L4W65FieldLinkSubclasses$Sub1 to L4W65FieldLinkSubclasses$Base
// foreign-set: java.lang.ClassCastException: Cannot cast L4W65FieldLinkSubclasses$Sub2 to L4W65FieldLinkSubclasses$Base
// foreign-left: 1
// string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W65FieldLinkSubclasses$Base
// string-set: java.lang.ClassCastException: Cannot cast java.lang.String to L4W65FieldLinkSubclasses$Base
// null: java.lang.NullPointerException
// null-set: java.lang.NullPointerException
// after-sub4: 5
// HOTSPOT_EXPECTED_END
//
// Switches: `CRATONVM_MH_FIELD_LINK_OWNER_ADMITS=1` on top of
// `CRATONVM_MH_OWNER_ID_AUTHORITY` (default on); run with and without
// `CRATONVM_MH_FIELD_SLOT_FROM_MINT=1` (an owner link needs the owner's slot:
// minted, or found by id). `--compatible` links field handles under
// `CRATONVM_MH_FIELD_LINK_COMPATIBLE` (default on); under `--jdk-only` the
// finders answer the JDK's own `DirectMethodHandle`s. `CRATONVM_DBG_MH_FIELD_LINK=1`
// (`--compatible`) prints one line per link made: with the switch on, the
// rotations link each handle about once instead of seven times.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W65FieldLinkSubclasses`.
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W65FieldLinkSubclasses {
    public static class Base {
        public int v = 1;
        public CharSequence cs = "cs";
    }

    public static class Sub1 extends Base {
        public int one = 11;
    }

    public static class Sub2 extends Base {
        public int v = 2;
    }

    public static class Sub3 extends Sub1 {
        public long three = 33L;
    }

    public static class Sub4 extends Sub2 {
        public int v = 4;
    }

    static final String BASE = "L4W65FieldLinkSubclasses$Base";
    static final String SUB1 = "L4W65FieldLinkSubclasses$Sub1";
    static final String SUB2 = "L4W65FieldLinkSubclasses$Sub2";

    /** Defines `Base`, `Sub1` and `Sub2` itself (child-first); delegates the rest. */
    static final class Isolating extends ClassLoader {
        Isolating(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(BASE) && !name.equals(SUB1) && !name.equals(SUB2)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] bytes;
                    try (InputStream in = L4W65FieldLinkSubclasses.class.getResourceAsStream(name + ".class")) {
                        if (in == null) {
                            throw new ClassNotFoundException(name);
                        }
                        bytes = in.readAllBytes();
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle get = lk.findGetter(Base.class, "v", int.class);
        MethodHandle set = lk.findSetter(Base.class, "v", int.class);
        MethodHandle getObj = get.asType(MethodType.methodType(int.class, Object.class));
        MethodHandle setObj = set.asType(MethodType.methodType(void.class, Object.class, int.class));
        MethodHandle csGet = lk.findGetter(Base.class, "cs", CharSequence.class);
        MethodHandle csSet = lk.findSetter(Base.class, "cs", CharSequence.class);
        MethodHandle sub2Get = lk.findGetter(Sub2.class, "v", int.class);
        MethodHandle sub4Get = lk.findGetter(Sub4.class, "v", int.class);

        Base[] ring = {new Base(), new Sub1(), new Sub2(), new Sub3(), new Sub4()};

        ClassLoader lb = new Isolating(L4W65FieldLinkSubclasses.class.getClassLoader());
        Class<?> baseB = lb.loadClass(BASE);
        Class<?> sub1B = lb.loadClass(SUB1);
        Class<?> sub2B = lb.loadClass(SUB2);
        Object fb = baseB.getConstructor().newInstance();
        Object fs1 = sub1B.getConstructor().newInstance();
        Object fs2 = sub2B.getConstructor().newInstance();
        MethodHandle baseBGet = lk.findGetter(baseB, "v", int.class);

        row("namesakes", () -> baseB.getName().equals(Base.class.getName()) + " " + (baseB != Base.class));
        row("poly-get", () -> {
            long s = 0;
            for (int i = 0; i < 5000; i++) {
                s += (int) get.invokeExact(ring[i % 5]);
            }
            return s;
        });
        row("poly-get-object", () -> {
            long s = 0;
            for (int i = 0; i < 5000; i++) {
                s += (int) getObj.invokeExact((Object) ring[i % 5]);
            }
            return s;
        });
        row("hidden-sub2", () -> (int) get.invokeExact(ring[2]) + "/" + (int) sub2Get.invoke(ring[2]));
        row("poly-set", () -> {
            for (int k = 0; k < 400; k++) {
                for (int i = 0; i < 5; i++) {
                    set.invokeExact(ring[i], i);
                }
            }
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < 5; i++) {
                if (i > 0) {
                    sb.append('+');
                }
                sb.append((int) get.invokeExact(ring[i]));
            }
            return sb;
        });
        row("hidden-after-set", () -> (int) get.invokeExact(ring[2]) + "/" + (int) sub2Get.invoke(ring[2])
                + "/" + (int) sub4Get.invoke(ring[4]));
        row("poly-cs", () -> {
            for (int k = 0; k < 400; k++) {
                for (int i = 0; i < 5; i++) {
                    CharSequence v = (i % 2 == 0) ? "x" : new StringBuilder("sb");
                    csSet.invokeExact(ring[i], v);
                }
            }
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < 5; i++) {
                if (i > 0) {
                    sb.append(',');
                }
                sb.append(((CharSequence) csGet.invokeExact(ring[i])).toString());
            }
            return sb;
        });
        long n = 0;
        for (int k = 0; k < 4000; k++) {
            for (int i = 0; i < 5; i++) {
                setObj.invokeExact((Object) ring[i], 3);
                n += (int) getObj.invokeExact((Object) ring[i]);
                n += (int) get.invokeExact(ring[i]);
            }
        }
        System.out.println("warm: " + n);
        row("after-poly-get", () -> {
            set.invokeExact(ring[3], 5);
            return (int) get.invokeExact(ring[3]);
        });
        row("foreign", () -> (int) getObj.invokeExact(fb));
        row("foreign-sub", () -> (int) getObj.invokeExact(fs1));
        row("foreign-set", () -> {
            setObj.invokeExact(fs2, 9);
            return "stored";
        });
        row("foreign-left", () -> (int) baseBGet.invoke(fs2));
        row("string", () -> (int) getObj.invokeExact((Object) "s"));
        // Wave 79 (lane L4): the setter twin of `string` (page
        // `i65-orch-compatible-warmed-field-getter-accepts-a-string-receiver`):
        // `Base`'s name is defined by two loaders here, so the by-name cast
        // could not judge, and the recorded owner now refuses by id.
        row("string-set", () -> {
            setObj.invokeExact((Object) "s", 1);
            return "stored";
        });
        row("null", () -> (int) getObj.invokeExact((Object) null));
        row("null-set", () -> {
            setObj.invokeExact((Object) null, 1);
            return "stored";
        });
        row("after-sub4", () -> {
            set.invokeExact(ring[4], 5);
            return (int) get.invokeExact(ring[4]);
        });
    }
}
