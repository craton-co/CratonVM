// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 35 (orchestrator): a lambda proxy declares one
// private constructor over its captures, as HotSpot's spun class does
// (docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md,
// item 3, the constructor half; wave 32 did the fields).
//
// Before wave 35 CratonVM reported no constructor at all.
//
// Run: javac -d out L4W35LambdaProxyConstructor.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W35LambdaProxyConstructor
//
// Expected HotSpot 25 output (default and -Xint):
//   capturing: 1 private (java.lang.String,int[],long) public=0
//   none: 1 private () public=0
import java.lang.reflect.Constructor;
import java.lang.reflect.Modifier;
import java.util.function.Supplier;

public class L4W35LambdaProxyConstructor {
    static String describe(Object lambda) {
        Constructor<?>[] all = lambda.getClass().getDeclaredConstructors();
        StringBuilder b = new StringBuilder().append(all.length);
        for (Constructor<?> c : all) {
            b.append(' ').append(Modifier.toString(c.getModifiers())).append(" (");
            Class<?>[] ps = c.getParameterTypes();
            for (int i = 0; i < ps.length; i++) {
                if (i > 0) {
                    b.append(',');
                }
                b.append(ps[i].getTypeName());
            }
            b.append(')');
        }
        return b.append(" public=").append(lambda.getClass().getConstructors().length).toString();
    }

    public static void main(String[] args) {
        String s = args.length > 99 ? "x" : "s";
        int[] a = {1};
        long j = 7L;
        Supplier<String> capturing = () -> s + a[0] + j;
        Supplier<String> none = () -> "none";
        System.out.println("capturing: " + describe(capturing));
        System.out.println("none: " + describe(none));
        capturing.get();
    }
}
