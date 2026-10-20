// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L3 -- the lambda proxy's frame in a
// `StackWalker.Option.SHOW_HIDDEN_FRAMES` walk. HotSpot runs a lambda's body
// below the spun proxy class's SAM method (`Outer$$Lambda/0x….apply`), a
// hidden frame only a walk that shows hidden frames lists. This VM's lambda
// dispatch runs the body with no proxy frame
// (docs/internal/fixed-bugs/interpreter-L6-lambda-proxy-frames-do-not-exist-RETIRED-20261004.md);
// since wave 50 a `SHOW_HIDDEN_FRAMES` walk lists one where the caller stands
// at an `invokeinterface` of the SAM and the frame above it is the
// implementation method of exactly one proxy of that SAM
// (`CRATONVM_STANDIN_LAMBDA_PROXY_FRAMES`).
// `tools/probes/interp/L6/L6W37HiddenFramesHot.java`'s `lambda walk-hidden`
// row is the hot shape.
//
// Each row walks from inside a functional object's body and prints the
// frames up to the row's driver `run`, as
// `Class.method@bci` (a hidden class's address shown as `<hidden>`), then for
// the proxy frame its descriptor, line, file and whether it is hidden by
// default (a default walk's frame count against the hidden walk's).
// The first row is repeated as the last row.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// plain-lambda:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames.lambda$main$0@3 < L3W50LambdaProxyFrames$$Lambda/<hidden>.applyAsInt@1 < drive | proxy (I)I line=-1 file=null hiddenClass=true sam=IntUnaryOperator | plain=2 hidden=3
// capturing-lambda:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames.lambda$main$1@3 < L3W50LambdaProxyFrames$$Lambda/<hidden>.applyAsInt@5 < drive | proxy (I)I line=-1 file=null hiddenClass=true sam=IntUnaryOperator | plain=2 hidden=3
// this-capturing-lambda:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames.lambda$capturing$0@3 < L3W50LambdaProxyFrames$$Lambda/<hidden>.applyAsInt@5 < drive | proxy (I)I line=-1 file=null hiddenClass=true sam=IntUnaryOperator | plain=2 hidden=3
// static-method-ref:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames.staticTarget@3 < L3W50LambdaProxyFrames$$Lambda/<hidden>.applyAsInt@1 < drive | proxy (I)I line=-1 file=null hiddenClass=true sam=IntUnaryOperator | plain=2 hidden=3
// bound-method-ref:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames.instanceTarget@3 < L3W50LambdaProxyFrames$$Lambda/<hidden>.applyAsInt@5 < drive | proxy (I)I line=-1 file=null hiddenClass=true sam=IntUnaryOperator | plain=2 hidden=3
// ctor-ref:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames$Made.<init>@7 < L3W50LambdaProxyFrames$$Lambda/<hidden>.get@4 < drive | proxy ()Ljava/lang/Object; line=-1 file=null hiddenClass=true sam=Supplier | plain=2 hidden=3
// runnable-lambda:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames.lambda$main$2@3 < L3W50LambdaProxyFrames$$Lambda/<hidden>.run@0 < drive | proxy ()V line=-1 file=null hiddenClass=true sam=Runnable | plain=2 hidden=3
// plain-lambda-again:
//   L3W50LambdaProxyFrames.here@8 < L3W50LambdaProxyFrames.lambda$main$3@3 < L3W50LambdaProxyFrames$$Lambda/<hidden>.applyAsInt@1 < drive | proxy (I)I line=-1 file=null hiddenClass=true sam=IntUnaryOperator | plain=2 hidden=3
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 50 (both modes): no `$$Lambda` frame, so each row
// lacks it, prints `proxy none` and `hidden=2`. With the switch on, the
// stand-in frame names the proxy class (`getDeclaringClass()` is the proxy's
// mirror; its id is >= 0x8000_0000, which the walker carrier's class-id slot
// now reads back), the SAM's erased descriptor, line -1, no file, and the bci
// of the implementation call in the body HotSpot spins
// (`stackwalker::lambda_forwarding_call_bci`). Positive control:
// `CRATONVM_DBG_STTRACE=1` prints `STTRACE_DBG_STANDIN kind=lambda-proxy ...`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W50LambdaProxyFrames`.
import java.lang.StackWalker.StackFrame;
import java.util.List;
import java.util.function.IntUnaryOperator;
import java.util.function.Supplier;
import java.util.stream.Collectors;

public class L3W50LambdaProxyFrames {
    static final StackWalker HIDDEN = StackWalker.getInstance(
            java.util.Set.of(StackWalker.Option.SHOW_HIDDEN_FRAMES, StackWalker.Option.RETAIN_CLASS_REFERENCE));
    static final StackWalker PLAIN = StackWalker.getInstance();

    static String simple(String cls) {
        int slash = cls.indexOf("/0x");
        if (slash >= 0) {
            cls = cls.substring(0, slash) + "/<hidden>";
        }
        return cls.substring(cls.lastIndexOf('.') + 1);
    }

    static String describe(List<StackFrame> fs) {
        StringBuilder sb = new StringBuilder();
        String proxy = "none";
        for (StackFrame f : fs) {
            if (f.getMethodName().equals("drive")) {
                sb.append("drive");
                break;
            }
            sb.append(simple(f.getClassName())).append('.').append(f.getMethodName())
                    .append('@').append(f.getByteCodeIndex()).append(" < ");
            if (f.getClassName().contains("$$Lambda")) {
                proxy = f.getDescriptor() + " line=" + f.getLineNumber() + " file=" + f.getFileName()
                        + " hiddenClass=" + f.getDeclaringClass().isHidden()
                        + " sam=" + f.getDeclaringClass().getInterfaces()[0].getSimpleName();
            }
        }
        return sb + " | proxy " + proxy;
    }

    static String here() {
        List<StackFrame> h = HIDDEN.walk(s -> s.collect(Collectors.toList()));
        long plain = PLAIN.walk(s -> s.takeWhile(f -> !f.getMethodName().equals("drive")).count());
        long hidden = h.stream().takeWhile(f -> !f.getMethodName().equals("drive")).count();
        return describe(h) + " | plain=" + plain + " hidden=" + hidden;
    }

    static int staticTarget(int x) {
        System.out.println("  " + here());
        return x;
    }

    int instanceTarget(int x) {
        System.out.println("  " + here());
        return x + field;
    }

    int field = 1;

    static class Made {
        Made() {
            System.out.println("  " + here());
        }
    }

    static int drive(IntUnaryOperator op) {
        return op.applyAsInt(1);
    }

    static Object drive(Supplier<?> s) {
        return s.get();
    }

    static Object drive(Runnable r) {
        r.run();
        return null;
    }

    public static void main(String[] args) {
        int k = 5;
        L3W50LambdaProxyFrames self = new L3W50LambdaProxyFrames();
        System.out.println("plain-lambda:");
        drive(x -> { System.out.println("  " + here()); return x; });
        System.out.println("capturing-lambda:");
        drive(x -> { System.out.println("  " + here()); return x + k; });
        System.out.println("this-capturing-lambda:");
        self.capturing();
        System.out.println("static-method-ref:");
        drive(L3W50LambdaProxyFrames::staticTarget);
        System.out.println("bound-method-ref:");
        drive(self::instanceTarget);
        System.out.println("ctor-ref:");
        drive((Supplier<Made>) Made::new);
        System.out.println("runnable-lambda:");
        drive((Runnable) () -> System.out.println("  " + here()));
        System.out.println("plain-lambda-again:");
        drive(x -> { System.out.println("  " + here()); return x; });
    }

    void capturing() {
        drive(x -> { System.out.println("  " + here()); return x + field; });
    }
}
