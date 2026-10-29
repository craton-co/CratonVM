// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
public class ForeignDowncallPolicyProbe {
    public static void main(String[] args) throws Throwable {
        System.out.println("nativeAccess.before=" + ForeignDowncallPolicyProbe.class.getModule().isNativeAccessEnabled());
        Linker linker = Linker.nativeLinker();
        MemorySegment symbol = linker.defaultLookup().find("abs").orElseThrow();
        MethodHandle abs;
        try {
            abs = linker.downcallHandle(symbol, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        } catch (IllegalCallerException denied) {
            System.out.println("LINK-DENIED");
            return;
        }
        System.out.println("nativeAccess.after=" + ForeignDowncallPolicyProbe.class.getModule().isNativeAccessEnabled());
        MethodHandle again = linker.downcallHandle(symbol, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        for (int n = 0; n < 5000; n++) {
            if ((int)abs.invokeExact(-n) != n || (int)again.invokeExact(-n) != n) throw new AssertionError();
        }
        System.out.println("FOREIGN-DOWNCALL-OK");
    }
}
