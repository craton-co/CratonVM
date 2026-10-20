// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L4 -- two default-off switches:
//   CRATONVM_MHN_RESOLVE_MEMO=1 (proposal
//     `i72-L4-proposal-mhn-resolve-memoises-its-member-answers-20261105`):
//     `MethodHandleNatives.resolve` of a field `MemberName`, `objectFieldOffset`,
//     `staticFieldOffset` and `staticFieldBase` answer a member they FOUND
//     before (same `clazz` / `name` / `type` objects and requested flags, one
//     collection epoch) from a per-thread memo. Only found members are kept:
//     an absent field's `NoSuchFieldException` is re-derived every time.
//   CRATONVM_MH_REAL_FIELD_RECORD=1 (proposal
//     `i72-L4-proposal-a-real-field-accessor-is-served-from-its-handle-record-20261105`):
//     `invokeExact` at the declared type (and `invokeBasic`) on a REAL field
//     accessor reads its shape from the handle's memoised record.
//
// Under `--jdk-only` the `Lookup` field finders are the JDK's own bytecode
// over those natives, and the handles are real `DirectMethodHandle$Accessor`
// / `$StaticAccessor` objects; under `--compatible` this model's carriers
// answer (control). Every row runs its finder and its call THREE times (the
// first fills the memo / record, the others are served by it) and prints
// `<answer> x3` when the three answers agree, else all three.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// get-int: 3 x3
// get-long: 5 x3
// get-ref: s x3
// get-volatile: 9 x3
// get-through-subclass: 3 x3
// static-get: 2 x3
// static-get-long: 8 x3
// static-through-subclass: 2 x3
// lazy-found-not-initialized: false x3
// init Lazy
// lazy-static: 11 x3
// set-int: 10 11 12
// set-long: 20 21 22
// static-set: 30 31 32
// unreflect-get: 5 x3
// varhandle-get: 3 x3
// missing: java.lang.NoSuchFieldException: no such field: L4W73FieldNativesMemo$Box.nope/int/getField x3
// wrong-type: java.lang.NoSuchFieldException: no such field: L4W73FieldNativesMemo$Box.v/long/getField x3
// static-as-instance: java.lang.IllegalAccessException: expected a non-static field: L4W73FieldNativesMemo$Box.S/int/getStatic, from class L4W73FieldNativesMemo x3
// instance-as-static: java.lang.IllegalAccessException: expected a static field: L4W73FieldNativesMemo$Box.v/int/getField, from class L4W73FieldNativesMemo x3
// found-after-missing: 3 x3
// null-receiver: java.lang.NullPointerException x3
// null-receiver-setter: java.lang.NullPointerException x3
// exact-wrong-site: java.lang.invoke.WrongMethodTypeException: handle's method type (Box)int but found (Box)Object x3
// HOTSPOT_EXPECTED_END
//
// Switch sets, `--compatible` and `--jdk-only` (each with `--nojit` too):
//   A: (none)
//   B: CRATONVM_MHN_RESOLVE_MEMO=1
//   C: CRATONVM_MH_REAL_FIELD_RECORD=1
//   D: CRATONVM_MHN_RESOLVE_MEMO=1 CRATONVM_MH_REAL_FIELD_RECORD=1
// Every row must be SAME in every set, in both modes.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W73FieldNativesMemo`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.List;

public class L4W73FieldNativesMemo {
    static boolean lazyInit;

    static int init(String who, int v) {
        System.out.println("init " + who);
        return v;
    }

    public static class Box {
        public static int S = 2;
        public static long SL = 8L;
        public int v = 3;
        public long w = 5L;
        public String s = "s";
        public volatile int vv = 9;
    }

    public static class Sub extends Box {
    }

    public static class Lazy {
        public static int L = init("Lazy", 11);

        static {
            lazyInit = true;
        }
    }

    interface Row {
        Object run(int i) throws Throwable;
    }

    static String one(Row r, int i) {
        String out;
        try {
            out = String.valueOf(r.run(i));
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        // The `from` tail names the unnamed module by its identity hash.
        int from = out.indexOf(" (unnamed module @");
        if (from >= 0) {
            out = out.substring(0, from);
        }
        return out;
    }

    static void row(String name, Row r) {
        List<String> outs = new ArrayList<>();
        for (int i = 0; i < 3; i++) {
            outs.add(one(r, i));
        }
        boolean same = outs.get(0).equals(outs.get(1)) && outs.get(1).equals(outs.get(2));
        System.out.println(name + ": " + (same ? outs.get(0) + " x3" : String.join(" ", outs)));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Box box = new Box();
        Sub sub = new Sub();
        row("get-int", i -> (int) l.findGetter(Box.class, "v", int.class).invokeExact(box));
        row("get-long", i -> (long) l.findGetter(Box.class, "w", long.class).invokeExact(box));
        row("get-ref", i -> (String) l.findGetter(Box.class, "s", String.class).invokeExact(box));
        row("get-volatile", i -> (int) l.findGetter(Box.class, "vv", int.class).invokeExact(box));
        row("get-through-subclass", i -> (int) l.findGetter(Sub.class, "v", int.class).invokeExact(sub));
        row("static-get", i -> (int) l.findStaticGetter(Box.class, "S", int.class).invokeExact());
        row("static-get-long", i -> (long) l.findStaticGetter(Box.class, "SL", long.class).invokeExact());
        row("static-through-subclass", i -> (int) l.findStaticGetter(Sub.class, "S", int.class).invokeExact());
        List<MethodHandle> lazy = new ArrayList<>();
        row("lazy-found-not-initialized", i -> {
            lazy.add(l.findStaticGetter(Lazy.class, "L", int.class));
            return lazyInit;
        });
        row("lazy-static", i -> (int) lazy.get(i).invokeExact());
        row("set-int", i -> {
            l.findSetter(Box.class, "v", int.class).invokeExact(box, 10 + i);
            return box.v;
        });
        box.v = 3;
        row("set-long", i -> {
            l.findSetter(Box.class, "w", long.class).invokeExact(box, 20L + i);
            return box.w;
        });
        box.w = 5L;
        row("static-set", i -> {
            l.findStaticSetter(Box.class, "S", int.class).invokeExact(30 + i);
            return Box.S;
        });
        Box.S = 2;
        row("unreflect-get", i -> (long) l.unreflectGetter(Box.class.getDeclaredField("w")).invokeExact(box));
        row("varhandle-get", i -> {
            VarHandle vh = l.findVarHandle(Box.class, "v", int.class);
            return (int) vh.get(box);
        });
        row("missing", i -> l.findGetter(Box.class, "nope", int.class));
        row("wrong-type", i -> l.findGetter(Box.class, "v", long.class));
        row("static-as-instance", i -> l.findGetter(Box.class, "S", int.class));
        row("instance-as-static", i -> l.findStaticGetter(Box.class, "v", int.class));
        row("found-after-missing", i -> (int) l.findGetter(Box.class, "v", int.class).invokeExact(box));
        MethodHandle get = l.findGetter(Box.class, "v", int.class);
        MethodHandle set = l.findSetter(Box.class, "v", int.class);
        row("null-receiver", i -> (int) get.invokeExact((Box) null));
        row("null-receiver-setter", i -> {
            set.invokeExact((Box) null, 1);
            return "stored";
        });
        row("exact-wrong-site", i -> (Object) get.invokeExact(box));
    }
}
