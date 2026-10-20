// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 120, lane L3 (proposal
// `i55-L1-proposal-lambda-proxies-answer-the-tool-interfaces-from-one-record`;
// `CRATONVM_REFLECT_LAMBDA_PROXY_FACTS=1`, default off): JNI on a lambda's
// class. The C half (`L3W120JniLambdaNewObject.c`) asks
// `GetMethodID(lambdaClass, "<init>", "(Ljava/lang/String;I)V")` and `get()`,
// builds instances with `NewObject` and `NewObjectA` from new captures, calls
// `get()` on them, and asks `AllocObject`. HotSpot answers all of it with the
// hidden class's own constructor and fresh instances.
//
// Build the library next to the classes (see the .c file's header), then:
//     java --enable-native-access=ALL-UNNAMED -Djava.library.path=. -cp . L3W120JniLambdaNewObject
//     cratonvm --jdk-only|--compatible [--nojit] -Djava.library.path=. -cp . L3W120JniLambdaNewObject
//     CRATONVM_REFLECT_LAMBDA_PROXY_FACTS=1 cratonvm ... (the switch arm)
// Without the library it prints "no library". (No HOTSPOT_EXPECTED block: the
// battery builds no C.) HotSpot 25.0.3 (Windows, MSVC build) prints:
//     lambda: cap3
//     init=found get=found NewObject.get=other7 sameClass=true NewObjectA.get=other8 alloc=obj
// CratonVM without the switch, by reading (not run): `init=null` after a
// `NoSuchMethodError`, and `alloc=null`.
import java.util.function.Supplier;

public class L3W120JniLambdaNewObject {
    static native String probe(Class<?> lambdaClass, String capture);

    public static void main(String[] args) {
        try {
            System.loadLibrary("l3w120jni");
        } catch (UnsatisfiedLinkError e) {
            System.out.println("no library");
            return;
        }
        String cap = "cap";
        int n = 3;
        Supplier<String> s = () -> cap + n;
        System.out.println("lambda: " + s.get());
        System.out.println(probe(s.getClass(), "other"));
    }
}
