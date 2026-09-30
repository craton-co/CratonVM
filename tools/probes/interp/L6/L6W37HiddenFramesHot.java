// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L6: hidden frames in COMPILED code
// (docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md,
// item 4). Waves 34-35 left hidden frames (`Method::is_hidden`: a method of a
// hidden class, or one annotated `@jdk.internal.vm.annotation.Hidden`) out of
// Throwable traces and default StackWalker walks, but only for interpreter
// frames (`L4W34HiddenFrames`, `L4W35StackWalkerHidden` are the interpreted
// shapes). Here every hidden method is hot: it runs 40,000 times, throwing (or
// walking) on every 64th call so the throwing branch is warm in the compiled
// body, and the row reports the LAST such trace.
//
// Shapes (the hidden frame in brackets is the one that must not show):
//
//   hidden-inline  `Op.apply` of a `defineHiddenClass` class, a two-bytecode
//                  body a compiled caller inlines: leaf < [Hid.apply] < drive
//   hidden-loop    the same with a loop in `apply`, so it is less likely to be
//                  inlined (a compiled activation of its own)
//   lambda         a lambda body: leaf < lambda$... < [proxy.apply] < drive
//   scoped-value   `ScopedValue.where(k, v).call(op)`: `Carrier.runWith` is
//                  `@Hidden` (and `@ForceInline`, so it is inlined into
//                  `Carrier.call` when compiled)
//   mh             `MethodHandle.invokeExact` of a hidden class's static
//                  method: the LambdaForm frames and the hidden method hide
//
// Each shape is reported three ways:
//   <shape> throw:        the Throwable's frames, innermost first, up to `hot`
//   <shape> walk:         a default StackWalker walk from `leaf`, up to `hot`
//   <shape> walk-hidden:  does a SHOW_HIDDEN_FRAMES walk show a hidden frame
//                         (a class whose name has `/0x`, or Carrier.runWith)?
//
// Positive control (CratonVM, --jdk-only): `CRATONVM_DBG_STTRACE=1` prints
// `STTRACE_DBG_HIDDEN kind=compiled ...` / `kind=inlined ...` for each
// compiled or inlined frame a throwable capture drops, and
// `STTRACE_DBG_HIDDEN kind=trap-splice ...` for one dropped from a compiled
// implicit exception's spliced snapshot.
//
// `--compatible` keeps every hidden frame (by design): the throw and walk rows
// then name the hidden frames.
//
// Known difference (wave-37 host run, default and --nojit): CratonVM prints
// `lambda walk-hidden: false`. Its native lambda dispatch runs the body with
// no proxy frame, so there is no `$$Lambda/0x…` frame for a SHOW_HIDDEN_FRAMES
// walk to show: docs/internal/fixed-bugs/interpreter-L6-lambda-proxy-frames-do-not-exist-RETIRED-20261004.md
// (retired as by design in wave 40: a hidden-frame display detail).
// The other 14 rows matched.
//
// Run: javac -d out L6W37HiddenFramesHot.java && cratonvm --java-home <jdk25> [--nojit] -cp out L6W37HiddenFramesHot
//
// Expected HotSpot 25 output (default and -Xint):
//   hidden-inline throw: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   hidden-inline walk: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   hidden-inline walk-hidden: true
//   hidden-loop throw: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   hidden-loop walk: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   hidden-loop walk-hidden: true
//   lambda throw: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.lambda$lambdaOp$0 < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   lambda walk: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.lambda$lambdaOp$0 < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   lambda walk-hidden: true
//   scoped-value throw: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.lambda$scopedOp$1 < ScopedValueContainer.callWithoutScope < ScopedValueContainer.call < ScopedValue$Carrier.call < L6W37HiddenFramesHot.lambda$scopedOp$0 < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   scoped-value walk: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot.lambda$scopedOp$1 < ScopedValueContainer.callWithoutScope < ScopedValueContainer.call < ScopedValue$Carrier.call < L6W37HiddenFramesHot.lambda$scopedOp$0 < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   scoped-value walk-hidden: true
//   mh throw: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot$MhOp.apply < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   mh walk: L6W37HiddenFramesHot.leaf < L6W37HiddenFramesHot$MhOp.apply < L6W37HiddenFramesHot.drive < L6W37HiddenFramesHot.hot
//   mh walk-hidden: true
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;
import java.util.stream.Stream;

public class L6W37HiddenFramesHot {
    public interface Op {
        int apply(int x);
    }

    static final ScopedValue<String> KEY = ScopedValue.newInstance();
    static final ClassDesc SELF = ClassDesc.of("L6W37HiddenFramesHot");
    static final ClassDesc OP = ClassDesc.of("L6W37HiddenFramesHot$Op");
    static final MethodTypeDesc INT_INT = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);

    /** 0: throw; 1: default walk; 2: SHOW_HIDDEN_FRAMES walk. */
    static int mode;
    static String walked;

    static String simple(String cls) {
        int slash = cls.indexOf('/');
        if (slash >= 0) {
            cls = cls.substring(0, slash) + "/<hidden>";
        }
        return cls.substring(cls.lastIndexOf('.') + 1);
    }

    static String names(Stream<String[]> frames) {
        List<String> out = new ArrayList<>();
        for (String[] f : (Iterable<String[]>) frames::iterator) {
            out.add(simple(f[0]) + "." + f[1]);
            if (f[1].equals("hot")) {
                break;
            }
        }
        return String.join(" < ", out);
    }

    static boolean isHiddenName(String cls, String method) {
        return cls.contains("/0x")
                || (cls.equals("java.lang.ScopedValue$Carrier") && method.equals("runWith"));
    }

    public static int leaf(int x) {
        if (x < 0) {
            switch (mode) {
                case 0 -> throw new IllegalStateException("boom");
                case 1 -> walked = StackWalker.getInstance().walk(
                        s -> names(s.map(f -> new String[] {f.getClassName(), f.getMethodName()})));
                default -> {
                    boolean shown = StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES).walk(
                            s -> s.anyMatch(f -> isHiddenName(f.getClassName(), f.getMethodName())));
                    walked = Boolean.toString(shown);
                }
            }
            return 0;
        }
        return x + 1;
    }

    static int drive(Op op, int x) {
        return op.apply(x);
    }

    static String hot(Op op, int m) {
        mode = m;
        String last = "none";
        for (int i = 0; i < 40_000; i++) {
            boolean odd = (i & 63) == 63;
            try {
                drive(op, odd ? -1 : i);
                if (odd && m != 0) {
                    last = walked;
                }
            } catch (IllegalStateException e) {
                last = names(Stream.of(e.getStackTrace())
                        .map(f -> new String[] {f.getClassName(), f.getMethodName()}));
            }
        }
        return last;
    }

    /** A hidden class implementing Op; `loop` puts a small loop in `apply`. */
    static Op hiddenOp(boolean loop) throws Throwable {
        byte[] b = ClassFile.of().build(ClassDesc.of("L6W37HiddenFramesHot$Hid"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(OP);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .return_());
            cb.withMethodBody("apply", INT_INT, ClassFile.ACC_PUBLIC, code -> {
                if (!loop) {
                    code.iload(1).invokestatic(SELF, "leaf", INT_INT).ireturn();
                    return;
                }
                // int s = 0; for (int i = 0; i < 2; i++) s += leaf(x); return s;
                code.iconst_0().istore(2).iconst_0().istore(3);
                var head = code.newLabel();
                var done = code.newLabel();
                code.labelBinding(head)
                        .iload(3).iconst_2().if_icmpge(done)
                        .iload(2).iload(1).invokestatic(SELF, "leaf", INT_INT).iadd().istore(2)
                        .iinc(3, 1).goto_(head)
                        .labelBinding(done)
                        .iload(2).ireturn();
            });
        });
        Class<?> h = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
        return (Op) h.getConstructor().newInstance();
    }

    static Op lambdaOp() {
        return x -> leaf(x);
    }

    static Op scopedOp() {
        return x -> ScopedValue.where(KEY, "v").call(() -> leaf(x));
    }

    static final class MhOp implements Op {
        final MethodHandle go;

        MhOp(MethodHandle go) {
            this.go = go;
        }

        @Override
        public int apply(int x) {
            try {
                return (int) go.invokeExact(x);
            } catch (RuntimeException | Error e) {
                throw e;
            } catch (Throwable t) {
                throw new AssertionError(t);
            }
        }
    }

    static Op mhOp() throws Throwable {
        byte[] b = ClassFile.of().build(ClassDesc.of("L6W37HiddenFramesHot$Go"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("go", INT_INT, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.iload(0).invokestatic(SELF, "leaf", INT_INT).ireturn());
        });
        Class<?> h = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
        return new MhOp(MethodHandles.lookup().findStatic(h, "go", MethodType.methodType(int.class, int.class)));
    }

    static void row(String shape, Op op) {
        System.out.println(shape + " throw: " + hot(op, 0));
        System.out.println(shape + " walk: " + hot(op, 1));
        System.out.println(shape + " walk-hidden: " + hot(op, 2));
    }

    public static void main(String[] args) throws Throwable {
        row("hidden-inline", hiddenOp(false));
        row("hidden-loop", hiddenOp(true));
        row("lambda", lambdaOp());
        row("scoped-value", scopedOp());
        row("mh", mhOp());
    }
}
