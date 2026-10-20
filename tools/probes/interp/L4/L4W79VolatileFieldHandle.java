// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L4 -- a defect probe (no switch), pages
// `docs/internal/fixed-bugs/interpreter-i71-L4-field-setter-handle-on-a-volatile-field-stores-without-the-volatile-fence-FIXED-20261111.md`
// and `i71-L4-non-strict-invokeexact-serves-a-field-link-at-any-site-20261104.md`.
//
// A setter handle on a `volatile` field stores with the field's own (volatile)
// access on HotSpot, so a Dekker handshake written through setter and getter
// handles never sees both loads read 0. The `dekker-*` rows count that outcome
// over many rounds (warmed, so the field-accessor link serves the calls); each
// must print 0. On x86-64 the store's trailing StoreLoad fence is what keeps
// the count at 0; before wave 79 the door, the link and the real accessor
// stored plainly. The value rows check that the fenced paths still store and
// load the right values, and `exact-wrong-site` that an `invokeExact` at
// another type is refused and writes nothing (with
// `CRATONVM_MH_STRICT_INVOKEEXACT=0` the warmed link no longer serves that
// site either: run the probe with it and check the row's `v=` stays 0 or the
// door's own answer -- not a raw `long` in the `int` slot).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// set-get-instance: 41
// set-get-static: 42
// unreflect-set-get: 43
// set-get-long: 44
// set-get-bound: 45
// exact-wrong-site: java.lang.invoke.WrongMethodTypeException: handle's method type (Box,int)void but found (Box,long)void v=0
// dekker-instance: both-zero=0
// dekker-static: both-zero=0
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible|--jdk-only] [--nojit] -cp . L4W79VolatileFieldHandle [rounds]`
// (default 20000 rounds per Dekker row).
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.concurrent.atomic.AtomicInteger;

public class L4W79VolatileFieldHandle {
    public static class Box {
        public volatile int x;
        public volatile int y;
        public volatile long l;
        public int plain;
        public static volatile int SX;
        public static volatile int SY;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static final MethodHandles.Lookup LK = MethodHandles.lookup();

    /** Two threads, each storing 1 to its own field then loading the other's. */
    static int dekker(int rounds, boolean statics) throws Throwable {
        final MethodHandle setX, setY, getX, getY;
        if (statics) {
            setX = LK.findStaticSetter(Box.class, "SX", int.class);
            setY = LK.findStaticSetter(Box.class, "SY", int.class);
            getX = LK.findStaticGetter(Box.class, "SX", int.class);
            getY = LK.findStaticGetter(Box.class, "SY", int.class);
        } else {
            setX = LK.findSetter(Box.class, "x", int.class);
            setY = LK.findSetter(Box.class, "y", int.class);
            getX = LK.findGetter(Box.class, "x", int.class);
            getY = LK.findGetter(Box.class, "y", int.class);
        }
        final Box[] boxes = new Box[rounds];
        for (int i = 0; i < rounds; i++) {
            boxes[i] = new Box();
        }
        final int[] r1 = new int[rounds];
        final int[] r2 = new int[rounds];
        final AtomicInteger go1 = new AtomicInteger(-1);
        final AtomicInteger go2 = new AtomicInteger(-1);
        final AtomicInteger done = new AtomicInteger(0);
        Thread t2 = new Thread(() -> {
            try {
                for (int i = 0; i < rounds; i++) {
                    while (go2.get() != i) {
                        Thread.onSpinWait();
                    }
                    Box b = boxes[i];
                    if (statics) {
                        setY.invokeExact(1);
                        r2[i] = (int) getX.invokeExact();
                    } else {
                        setY.invokeExact(b, 1);
                        r2[i] = (int) getX.invokeExact(b);
                    }
                    done.incrementAndGet();
                }
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        });
        t2.setDaemon(true);
        t2.start();
        int bothZero = 0;
        for (int i = 0; i < rounds; i++) {
            Box b = boxes[i];
            if (statics) {
                Box.SX = 0;
                Box.SY = 0;
            }
            go2.set(i);
            go1.set(i);
            if (statics) {
                setX.invokeExact(1);
                r1[i] = (int) getY.invokeExact();
            } else {
                setX.invokeExact(b, 1);
                r1[i] = (int) getY.invokeExact(b);
            }
            while (done.get() != i + 1) {
                Thread.onSpinWait();
            }
            if (r1[i] == 0 && r2[i] == 0) {
                bothZero++;
            }
        }
        t2.join();
        return bothZero;
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        row("set-get-instance", () -> {
            MethodHandle set = LK.findSetter(Box.class, "x", int.class);
            MethodHandle get = LK.findGetter(Box.class, "x", int.class);
            Box b = new Box();
            int v = 0;
            for (int i = 0; i < 50; i++) {
                set.invokeExact(b, 41);
                v = (int) get.invokeExact(b);
            }
            return v;
        });
        row("set-get-static", () -> {
            MethodHandle set = LK.findStaticSetter(Box.class, "SX", int.class);
            MethodHandle get = LK.findStaticGetter(Box.class, "SX", int.class);
            int v = 0;
            for (int i = 0; i < 50; i++) {
                set.invokeExact(42);
                v = (int) get.invokeExact();
            }
            return v;
        });
        row("unreflect-set-get", () -> {
            MethodHandle set = LK.unreflectSetter(Box.class.getField("y"));
            MethodHandle get = LK.unreflectGetter(Box.class.getField("y"));
            Box b = new Box();
            int v = 0;
            for (int i = 0; i < 50; i++) {
                set.invokeExact(b, 43);
                v = (int) get.invokeExact(b);
            }
            return v;
        });
        row("set-get-long", () -> {
            MethodHandle set = LK.findSetter(Box.class, "l", long.class);
            MethodHandle get = LK.findGetter(Box.class, "l", long.class);
            Box b = new Box();
            long v = 0;
            for (int i = 0; i < 50; i++) {
                set.invokeExact(b, 44L);
                v = (long) get.invokeExact(b);
            }
            return v;
        });
        row("set-get-bound", () -> {
            Box b = new Box();
            MethodHandle set = LK.findSetter(Box.class, "x", int.class).bindTo(b);
            MethodHandle get = LK.findGetter(Box.class, "x", int.class).bindTo(b);
            int v = 0;
            for (int i = 0; i < 50; i++) {
                set.invokeExact(45);
                v = (int) get.invokeExact();
            }
            return v;
        });
        row("exact-wrong-site", () -> {
            MethodHandle set = LK.findSetter(Box.class, "plain", int.class);
            Box b = new Box();
            for (int i = 0; i < 50; i++) {
                set.invokeExact(b, 0); // warm and link at the declared type
            }
            String out;
            try {
                set.invokeExact(b, 5L);
                out = "stored";
            } catch (Throwable t) {
                out = t.getClass().getName() + ": " + t.getMessage();
            }
            return out + " v=" + b.plain;
        });
        row("dekker-instance", () -> "both-zero=" + dekker(rounds, false));
        row("dekker-static", () -> "both-zero=" + dekker(rounds, true));
    }
}
