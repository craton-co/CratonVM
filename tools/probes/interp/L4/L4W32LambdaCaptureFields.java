// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 32 (orchestrator): a lambda proxy's captured
// fields are typed as the factory declares them
// (docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md,
// item 3, the field half).
//
// Before wave 32 CratonVM typed every reference capture `java.lang.Object`
// (one type character was kept per capture).
//
// Run: javac -d out L4W32LambdaCaptureFields.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W32LambdaCaptureFields
//
// Expected HotSpot 25 output (default and -Xint):
//   lambda: arg$1 java.lang.String, arg$2 int[], arg$3 java.util.List, arg$4 int, arg$5 long
//   bound: arg$1 java.lang.StringBuilder
//   none:
import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.List;
import java.util.function.Function;
import java.util.function.Supplier;

public class L4W32LambdaCaptureFields {
    static String fields(Object lambda) {
        Field[] fs = lambda.getClass().getDeclaredFields();
        Arrays.sort(fs, (a, b) -> a.getName().compareTo(b.getName()));
        StringBuilder b = new StringBuilder();
        for (Field f : fs) {
            if (b.length() > 0) {
                b.append(", ");
            }
            b.append(f.getName()).append(' ').append(f.getType().getTypeName());
        }
        return b.toString();
    }

    public static void main(String[] args) {
        String s = args.length > 99 ? "x" : "s";
        int[] a = {1};
        List<String> l = List.of("l");
        int i = a.length;
        long j = 7L;
        Supplier<String> lambda = () -> s + a[0] + l + i + j;
        StringBuilder sb = new StringBuilder();
        Function<String, StringBuilder> bound = sb::append;
        Supplier<String> none = () -> "none";
        System.out.println("lambda: " + fields(lambda));
        System.out.println("bound: " + fields(bound));
        System.out.println("none: " + fields(none));
        lambda.get();
    }
}
