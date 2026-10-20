// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L4 -- a `findSpecial` handle dispatches
// on its class by id (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`, opt-in; first stage
// of `interpreter-i63-L4-proposal-virtual-door-dispatches-by-owner-id-KEPT-OFF-20261224.md`).
//
// Two child-first loaders each define `Base`, `Mid extends Base` and
// `Sub extends Mid`; each loader's `Base.tag` is set to its own letter, and
// `Base.who()` / `Mid.who()` print it, so a row names the copy whose body
// ran. `findSpecial` / `unreflectSpecial` handles (`super.who()` from `Sub`)
// are looked up on each loader's classes. HotSpot runs the handle's own
// loader's body: every row of loader A ends in `A`, of loader B in `B`.
// Before this stage the door's `SPECIAL` arm re-resolved the dispatch class
// by NAME through the loader-blind global map, so ONE of the two loaders'
// rows ran the other copy (which one depends on which definition the map
// kept) -- the positive control under `--compatible` with the switch off.
// `findSpecial(Base, who, Sub)` restarts at `Sub`'s direct superclass
// (`Mid`), as `super.who()` in `Sub` does; the `virtual-*` rows are controls.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// namesakes: true true
// A special-mid: mid-A
// A special-base-restart: mid-A
// A unreflect-special-mid: mid-A
// A special-mid-exact: mid-A
// A special-mid-astype: mid-A
// A special-mid-bound: mid-A
// A special-mid-iwa: mid-A
// A virtual-sub: sub
// A special-mid-on-mid: java.lang.ClassCastException: Cannot cast L4W64SpecialByOwnerId$Mid to L4W64SpecialByOwnerId$Sub
// B special-mid: mid-B
// B special-base-restart: mid-B
// B unreflect-special-mid: mid-B
// B special-mid-exact: mid-B
// B special-mid-astype: mid-B
// B special-mid-bound: mid-B
// B special-mid-iwa: mid-B
// B virtual-sub: sub
// B special-mid-on-mid: java.lang.ClassCastException: Cannot cast L4W64SpecialByOwnerId$Mid to L4W64SpecialByOwnerId$Sub
// warm: 2000
// HOTSPOT_EXPECTED_END
//
// Switches: `CRATONVM_MH_SPECIAL_BY_OWNER_ID=1` (both modes: under `--jdk-only`
// the JDK handle's `linkToSpecial` dispatches by `MemberName.clazz`'s id under
// it; off, it printed `mid-?` there, the by-name third copy). Under
// `--compatible` the switch also casts the receiver to the `specialCaller` by
// id (row `special-mid-on-mid`).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W64SpecialByOwnerId`.
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W64SpecialByOwnerId {
    public static class Base {
        public static String tag = "?";

        public String who() {
            return "base-" + tag;
        }
    }

    public static class Mid extends Base {
        @Override
        public String who() {
            return "mid-" + tag;
        }
    }

    public static class Sub extends Mid {
        @Override
        public String who() {
            return "sub";
        }
    }

    static final String BASE = "L4W64SpecialByOwnerId$Base";
    static final String MID = "L4W64SpecialByOwnerId$Mid";
    static final String SUB = "L4W64SpecialByOwnerId$Sub";

    /** Defines `Base`, `Mid` and `Sub` itself (child-first); delegates the rest. */
    static final class Isolating extends ClassLoader {
        Isolating(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(BASE) && !name.equals(MID) && !name.equals(SUB)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] bytes;
                    try (InputStream in = L4W64SpecialByOwnerId.class.getResourceAsStream(name + ".class")) {
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

    static final class Copy {
        final Class<?> base;
        final Class<?> mid;
        final Class<?> sub;

        Copy(String letter) throws Throwable {
            ClassLoader l = new Isolating(L4W64SpecialByOwnerId.class.getClassLoader());
            base = l.loadClass(BASE);
            mid = l.loadClass(MID);
            sub = l.loadClass(SUB);
            base.getField("tag").set(null, letter);
        }
    }

    static long rows(String letter, Copy c) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.privateLookupIn(c.sub, MethodHandles.lookup());
        MethodType mt = MethodType.methodType(String.class);
        MethodHandle mid = lk.findSpecial(c.mid, "who", mt, c.sub);
        MethodHandle restart = lk.findSpecial(c.base, "who", mt, c.sub);
        MethodHandle umid = lk.unreflectSpecial(c.mid.getMethod("who"), c.sub);
        MethodHandle exact = mid.asType(MethodType.methodType(String.class, Object.class));
        MethodHandle astype = mid.asType(MethodType.methodType(Object.class, Object.class));
        MethodHandle virt = lk.findVirtual(c.base, "who", mt);
        Object sub = c.sub.getConstructor().newInstance();
        Object midObj = c.mid.getConstructor().newInstance();
        MethodHandle bound = mid.bindTo(sub);
        row(letter + " special-mid", () -> (String) mid.invoke(sub));
        row(letter + " special-base-restart", () -> (String) restart.invoke(sub));
        row(letter + " unreflect-special-mid", () -> (String) umid.invoke(sub));
        row(letter + " special-mid-exact", () -> (String) exact.invokeExact(sub));
        row(letter + " special-mid-astype", () -> astype.invoke(sub));
        row(letter + " special-mid-bound", () -> (String) bound.invoke());
        row(letter + " special-mid-iwa", () -> mid.invokeWithArguments(sub));
        row(letter + " virtual-sub", () -> (String) virt.invoke(sub));
        row(letter + " special-mid-on-mid", () -> (String) mid.invoke(midObj));
        long n = 0;
        for (int i = 0; i < 1000; i++) {
            n += ((String) exact.invokeExact(sub)).length() == 5 ? 1 : 0;
        }
        return n;
    }

    public static void main(String[] args) throws Throwable {
        Copy a = new Copy("A");
        Copy b = new Copy("B");
        row("namesakes", () -> a.base.getName().equals(b.base.getName()) + " " + (a.base != b.base));
        long n = rows("A", a);
        n += rows("B", b);
        System.out.println("warm: " + n);
    }
}
