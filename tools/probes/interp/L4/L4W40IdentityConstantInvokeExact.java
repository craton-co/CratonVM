// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L4
// (`i39-L4-invokeexact-of-identity-and-constant-handles-does-not-check-the-type`):
// `invokeExact` of a `MethodHandles.identity` / `constant` / `zero` / `empty`
// handle compares the call site against the handle's type, like every other
// handle. CratonVM ran the call (rows id-mismatch, const-mismatch,
// zero-mismatch, astype-leaf-site, exact-invoker). `bindTo` on an identity or
// an `empty` handle drops the bound parameter from `type()` (CratonVM kept it:
// rows id-bound-type, empty-bound-type; and a second `bindTo` was accepted:
// id-bound-twice), and a bound `empty` handle still answers its zero (CratonVM
// replaced the constant with the bound value: empty-bound-call). The array
// accessors (`arrayElementGetter` / `arrayElementSetter` / `arrayLength`) were
// not judged either (array-*-mismatch), and a bound `arrayLength` kept its
// `(int[])int` type (array-length-bound-type).
//
// Run: javac -d out L4W40IdentityConstantInvokeExact.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W40IdentityConstantInvokeExact
//
// Expected HotSpot 25 output (default and -Xint; `--compatible` identical):
//   id-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type (Local)Local but found (Object)Object
//   const-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type ()L4W40Top$Name but found ()Object
//   zero-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()long
//   id-match: s
//   const-match: 5
//   id-bound-type: ()String
//   id-bound-call: x
//   id-bound-twice: java.lang.IllegalArgumentException: no leading reference parameter
//   empty-bound-type: ()int
//   empty-bound-call: 0
//   astype-site: 7
//   astype-leaf-site: java.lang.invoke.WrongMethodTypeException: handle's method type (Integer)Object but found (int)int
//   array-get-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int)int but found (Object,int)int
//   array-get-match: 5
//   array-set-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int,int)void but found (int[],int,long)void
//   array-length-mismatch: java.lang.invoke.WrongMethodTypeException: handle's method type (int[])int but found (int[])long
//   array-length-bound-type: ()int
//   array-length-bound-call: 3
//   exact-invoker: java.lang.invoke.WrongMethodTypeException: handle's method type (String)String but found (Object)Object
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

class L4W40Top$Name {
}

public class L4W40IdentityConstantInvokeExact {
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

    public static void main(String[] args) {
        class Local {
        }
        Object localInstance = new Local();
        row("id-mismatch", () -> {
            MethodHandle id = MethodHandles.identity(Local.class);
            Object r = (Object) id.invokeExact((Object) localInstance);
            return "returned " + (r == localInstance);
        });
        row("const-mismatch", () -> {
            MethodHandle k = MethodHandles.constant(L4W40Top$Name.class, new L4W40Top$Name());
            Object r = (Object) k.invokeExact();
            return "returned " + (r != null);
        });
        row("zero-mismatch", () -> {
            MethodHandle z = MethodHandles.zero(int.class);
            long r = (long) z.invokeExact();
            return "returned " + r;
        });
        row("id-match", () -> {
            MethodHandle id = MethodHandles.identity(String.class);
            String r = (String) id.invokeExact("s");
            return r;
        });
        row("const-match", () -> {
            MethodHandle k = MethodHandles.constant(int.class, 5);
            int r = (int) k.invokeExact();
            return r;
        });
        row("id-bound-type", () -> MethodHandles.identity(String.class).bindTo("x").type());
        row("id-bound-call", () -> {
            MethodHandle b = MethodHandles.identity(String.class).bindTo("x");
            String r = (String) b.invokeExact();
            return r;
        });
        row("id-bound-twice", () -> MethodHandles.identity(String.class).bindTo("x").bindTo("y").type());
        row("empty-bound-type",
                () -> MethodHandles.empty(MethodType.methodType(int.class, String.class)).bindTo("x").type());
        row("empty-bound-call", () -> {
            MethodHandle b = MethodHandles.empty(MethodType.methodType(int.class, String.class)).bindTo("x");
            int r = (int) b.invokeExact();
            return r;
        });
        row("astype-site", () -> {
            MethodHandle a = MethodHandles.identity(int.class)
                    .asType(MethodType.methodType(Object.class, Integer.class));
            Object r = (Object) a.invokeExact((Integer) 7);
            return r;
        });
        row("astype-leaf-site", () -> {
            MethodHandle a = MethodHandles.identity(int.class)
                    .asType(MethodType.methodType(Object.class, Integer.class));
            int r = (int) a.invokeExact(7);
            return r;
        });
        int[] arr = {4, 5, 6};
        row("array-get-mismatch", () -> {
            MethodHandle g = MethodHandles.arrayElementGetter(int[].class);
            int r = (int) g.invokeExact((Object) arr, 0);
            return r;
        });
        row("array-get-match", () -> {
            MethodHandle g = MethodHandles.arrayElementGetter(int[].class);
            int r = (int) g.invokeExact(arr, 1);
            return r;
        });
        row("array-set-mismatch", () -> {
            MethodHandle s = MethodHandles.arrayElementSetter(int[].class);
            s.invokeExact(arr, 0, 9L);
            return arr[0];
        });
        row("array-length-mismatch", () -> {
            MethodHandle n = MethodHandles.arrayLength(int[].class);
            long r = (long) n.invokeExact(arr);
            return r;
        });
        row("array-length-bound-type", () -> MethodHandles.arrayLength(int[].class).bindTo(arr).type());
        row("array-length-bound-call", () -> {
            MethodHandle n = MethodHandles.arrayLength(int[].class).bindTo(arr);
            int r = (int) n.invokeExact();
            return r;
        });
        row("exact-invoker", () -> {
            MethodHandle inv = MethodHandles.exactInvoker(MethodType.methodType(Object.class, Object.class));
            Object r = (Object) inv.invokeExact(MethodHandles.identity(String.class), (Object) "s");
            return r;
        });
    }
}
