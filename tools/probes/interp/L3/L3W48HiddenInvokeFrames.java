// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L3 -- the hidden method-handle frames a
// `SHOW_HIDDEN_FRAMES` walk lists for a `MethodHandle.invokeExact` /
// `invoke` call and a `Method.invoke` call, which this VM serves as natives
// (docs/internal/fixed-bugs/interpreter-i46-L3-proposal-one-table-of-the-jdk-frames-a-native-stands-for-DONE-20261224.md,
// stage 1; `stackwalker::hidden_invoke_standin_frames`).
//
// Each row walks from a static target with SHOW_HIDDEN_FRAMES (and
// RETAIN_CLASS_REFERENCE, for the descriptor) down to the probe's `via*`
// caller and prints every frame as `Class.method descriptor @bci`. The
// `LambdaForm$MH/0x…` frames HotSpot spins while it links the call are left
// out of the rows: this VM never spins them, so it has no name to give them
// (the section comment in `stackwalker.rs` says which frames are listed and
// why). `default` rows walk without the option (no hidden frame may show);
// `throw` rows print a Throwable's frames (never hidden ones).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// mh-void: P.target ()V @0 < DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;)V @9 < P.viaMh (Ljava/lang/invoke/MethodHandle;)V @1
// mh-int: P.targetInt (I)I @0 < DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;I)I @10 < P.viaMhInt (Ljava/lang/invoke/MethodHandle;)I @3
// mh-object: P.targetObject (Ljava/lang/String;)Ljava/lang/Object; @0 < DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @10 < Invokers$Holder.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @19 < P.viaMhObject (Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object; @3
// mh-generic: P.target ()V @0 < DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;)V @9 < P.viaMhGeneric (Ljava/lang/invoke/MethodHandle;)V @1
// mh-default: P.target ()V @0 < P.viaMh
// reflect-0: P.target ()V @0 < DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;)V @9 < Invokers$Holder.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @19 < DirectMethodHandleAccessor.invokeImpl (Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object; @41 < DirectMethodHandleAccessor.invoke (Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object; @23 < Method.invoke (Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object; @102 < P.viaReflect (Ljava/lang/reflect/Method;[Ljava/lang/Object;)V @3
// reflect-1: P.targetInt (I)I @0 < DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;I)I @10 < Invokers$Holder.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @20 < DirectMethodHandleAccessor.invokeImpl (Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object; @55 < DirectMethodHandleAccessor.invoke (Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object; @23 < Method.invoke (Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object; @102 < P.viaReflect (Ljava/lang/reflect/Method;[Ljava/lang/Object;)V @3
// reflect-default: P.target ()V @0 < DirectMethodHandleAccessor.invoke < Method.invoke < P.viaReflect
// ctor-0: P$Box.<init> ()V @4 < DirectMethodHandle$Holder.newInvokeSpecial (Ljava/lang/Object;)Ljava/lang/Object; @15 < Invokers$Holder.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @18 < DirectConstructorHandleAccessor.invokeImpl ([Ljava/lang/Object;)Ljava/lang/Object; @40 < DirectConstructorHandleAccessor.newInstance ([Ljava/lang/Object;)Ljava/lang/Object; @60 < Constructor.newInstanceWithCaller ([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object; @41 < Constructor.newInstance ([Ljava/lang/Object;)Ljava/lang/Object; @30 < P.viaCtor (Ljava/lang/reflect/Constructor;[Ljava/lang/Object;)Ljava/lang/Object; @2
// ctor-1: P$Box.<init> (I)V @4 < DirectMethodHandle$Holder.newInvokeSpecial (Ljava/lang/Object;I)Ljava/lang/Object; @16 < Invokers$Holder.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @19 < DirectConstructorHandleAccessor.invokeImpl ([Ljava/lang/Object;)Ljava/lang/Object; @53 < DirectConstructorHandleAccessor.newInstance ([Ljava/lang/Object;)Ljava/lang/Object; @60 < Constructor.newInstanceWithCaller ([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object; @41 < Constructor.newInstance ([Ljava/lang/Object;)Ljava/lang/Object; @30 < P.viaCtor (Ljava/lang/reflect/Constructor;[Ljava/lang/Object;)Ljava/lang/Object; @2
// mh-ctor: P$Box.<init> (I)V @4 < DirectMethodHandle$Holder.newInvokeSpecial (Ljava/lang/Object;I)Ljava/lang/Object; @16 < P.viaMhCtor (Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object; @2
// throw: P.thrower < P.viaMhThrow
// HOTSPOT_EXPECTED_END
//
// The `mh-void`, `mh-int` and `mh-generic` sites have no pregenerated
// invoker (HotSpot runs a spun `LambdaForm$MH/0x….invokeExact_MT` there,
// left out); `mh-object`'s `(Object,Object,Object)Object` invoker is in
// `Invokers$Holder`. The bcis of the holder frames are read from the image's
// own bytes, as every stand-in frame's is.
//
// CratonVM before wave 48: every `mh-*` and `reflect-*` row but the
// `default` ones listed no holder, invoker or `invokeImpl` frame.
// Positive control: `CRATONVM_DBG_STTRACE=1` prints
// `STTRACE_DBG_STANDIN kind=hidden-invoke at=<index> <frame>` for each frame
// a walk lists this way. `CRATONVM_STANDIN_HIDDEN_INVOKE_FRAMES=0` restores
// the old rows.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W48HiddenInvokeFrames`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.List;

public class L3W48HiddenInvokeFrames {
    static final StackWalker HIDDEN = StackWalker.getInstance(
            EnumSet.of(StackWalker.Option.SHOW_HIDDEN_FRAMES, StackWalker.Option.RETAIN_CLASS_REFERENCE));
    static final StackWalker DEFAULT = StackWalker.getInstance(
            EnumSet.of(StackWalker.Option.SHOW_REFLECT_FRAMES, StackWalker.Option.RETAIN_CLASS_REFERENCE));

    static String result;
    static boolean hidden = true;

    static String shorten(String cls) {
        String s = cls.replace("L3W48HiddenInvokeFrames", "P");
        int dot = s.lastIndexOf('.');
        return dot >= 0 ? s.substring(dot + 1) : s;
    }

    static void walk() {
        StackWalker w = hidden ? HIDDEN : DEFAULT;
        result = w.walk(s -> {
            List<String> out = new ArrayList<>();
            boolean started = false;
            for (StackWalker.StackFrame f : (Iterable<StackWalker.StackFrame>) s::iterator) {
                String cls = shorten(f.getClassName());
                String m = f.getMethodName();
                if (!started) {
                    started = m.startsWith("target") || m.equals("<init>");
                    if (!started) {
                        continue;
                    }
                }
                if (cls.startsWith("LambdaForm$")) {
                    continue;
                }
                String line = cls + "." + m;
                if (hidden) {
                    line += " " + f.getDescriptor() + " @" + f.getByteCodeIndex();
                } else if (m.startsWith("target") || m.equals("<init>")) {
                    line += " " + f.getMethodType().toMethodDescriptorString() + " @" + f.getByteCodeIndex();
                }
                out.add(line);
                if (m.startsWith("via")) {
                    break;
                }
            }
            return String.join(" < ", out);
        });
    }

    public static void target() {
        walk();
    }

    public static int targetInt(int x) {
        walk();
        return x;
    }

    public static Object targetObject(String s) {
        walk();
        return s;
    }

    public static final class Box {
        public Box() {
            walk();
        }

        public Box(int x) {
            walk();
        }
    }

    static Object viaCtor(java.lang.reflect.Constructor<?> c, Object[] args) throws Throwable {
        return c.newInstance(args);
    }

    static Object viaMhCtor(MethodHandle mh) throws Throwable {
        return (Box) mh.invokeExact(5);
    }

    public static void thrower() {
        throw new IllegalStateException("x");
    }

    static void viaMh(MethodHandle mh) throws Throwable {
        mh.invokeExact();
    }

    static int viaMhInt(MethodHandle mh) throws Throwable {
        return (int) mh.invokeExact(7);
    }

    static Object viaMhObject(MethodHandle mh) throws Throwable {
        return (Object) mh.invokeExact("s");
    }

    static void viaMhGeneric(MethodHandle mh) throws Throwable {
        mh.invoke();
    }

    static void viaMhThrow(MethodHandle mh) throws Throwable {
        mh.invokeExact();
    }

    static void viaReflect(Method m, Object[] args) throws Throwable {
        m.invoke(null, args);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> p = L3W48HiddenInvokeFrames.class;
        MethodHandle target = l.findStatic(p, "target", MethodType.methodType(void.class));
        MethodHandle targetInt = l.findStatic(p, "targetInt", MethodType.methodType(int.class, int.class));
        MethodHandle targetObject =
                l.findStatic(p, "targetObject", MethodType.methodType(Object.class, String.class));
        MethodHandle thrower = l.findStatic(p, "thrower", MethodType.methodType(void.class));

        viaMh(target);
        System.out.println("mh-void: " + result);
        viaMhInt(targetInt);
        System.out.println("mh-int: " + result);
        viaMhObject(targetObject);
        System.out.println("mh-object: " + result);
        viaMhGeneric(target);
        System.out.println("mh-generic: " + result);
        hidden = false;
        viaMh(target);
        System.out.println("mh-default: " + result);
        hidden = true;

        viaReflect(p.getMethod("target"), new Object[0]);
        System.out.println("reflect-0: " + result);
        viaReflect(p.getMethod("targetInt", int.class), new Object[] {3});
        System.out.println("reflect-1: " + result);
        hidden = false;
        viaReflect(p.getMethod("target"), new Object[0]);
        System.out.println("reflect-default: " + result);
        hidden = true;
        viaCtor(Box.class.getConstructor(), new Object[0]);
        System.out.println("ctor-0: " + result);
        viaCtor(Box.class.getConstructor(int.class), new Object[] {1});
        System.out.println("ctor-1: " + result);
        viaMhCtor(l.findConstructor(Box.class, MethodType.methodType(void.class, int.class)));
        System.out.println("mh-ctor: " + result);
        hidden = true;

        try {
            viaMhThrow(thrower);
        } catch (IllegalStateException e) {
            List<String> out = new ArrayList<>();
            for (StackTraceElement el : e.getStackTrace()) {
                out.add(shorten(el.getClassName()) + "." + el.getMethodName());
                if (el.getMethodName().startsWith("via")) {
                    break;
                }
            }
            System.out.println("throw: " + String.join(" < ", out));
        }
    }
}
