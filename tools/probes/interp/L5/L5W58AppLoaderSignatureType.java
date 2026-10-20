// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L5 -- a reflective listing of an
// APPLICATION-loader class whose public method names a missing type fails
// with HotSpot's `NoClassDefFoundError` (the internal name) caused by
// `ClassNotFoundException` (the binary name), a throwable the program can
// catch. With `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES` (lane L3's switch,
// default off) the resolver's VM error for plain absence escaped the listing
// as an internal error and killed `main` (`Error in thread "main" class file
// error: class not found: ...`); the L3 probe's holder is a user loader's,
// whose `ClassNotFoundException` was already Java.
//
// SETUP and run (`Gone` must be absent at run time):
//   javac -d out L5W58AppLoaderSignatureType.java
//   rm 'out/L5W58AppLoaderSignatureType$Holder$Gone.class'
//   java -cp out L5W58AppLoaderSignatureType
//   CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES=1 cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W58AppLoaderSignatureType
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   method-1=java.lang.NoClassDefFoundError: L5W58AppLoaderSignatureType$Holder$Gone cause=java.lang.ClassNotFoundException: L5W58AppLoaderSignatureType$Holder$Gone
//   methods-1=java.lang.NoClassDefFoundError: L5W58AppLoaderSignatureType$Holder$Gone cause=java.lang.ClassNotFoundException: L5W58AppLoaderSignatureType$Holder$Gone
//   fields-1=java.lang.NoClassDefFoundError: L5W58AppLoaderSignatureType$Holder$Gone cause=java.lang.ClassNotFoundException: L5W58AppLoaderSignatureType$Holder$Gone
//   ok-1=1
//   method-2=java.lang.NoClassDefFoundError: L5W58AppLoaderSignatureType$Holder$Gone cause=java.lang.ClassNotFoundException: L5W58AppLoaderSignatureType$Holder$Gone
//   methods-2=java.lang.NoClassDefFoundError: L5W58AppLoaderSignatureType$Holder$Gone cause=java.lang.ClassNotFoundException: L5W58AppLoaderSignatureType$Holder$Gone
//   fields-2=java.lang.NoClassDefFoundError: L5W58AppLoaderSignatureType$Holder$Gone cause=java.lang.ClassNotFoundException: L5W58AppLoaderSignatureType$Holder$Gone
//   ok-2=1
// (With the switch off CratonVM lists without failing: `method-N=public
// static void L5W58AppLoaderSignatureType$Holder.run()`, `methods-N=11`,
// `fields-N=1`; that is item 3 of the i47-L5 page, the switch's subject.)
//
// CratonVM with the switch, wave 58 (debug build, Windows, measured):
// `--jdk-only` prints HotSpot's `method-N` / `methods-N` but `fields-N=1`;
// `--compatible` prints the switch-off lines for all three
// (`docs/internal/fixed-bugs/interpreter-L5-the-signature-type-switch-misses-public-fields-and-compatible-listings-FIXED-20261023.md`).
// Before wave 58 the first row killed `main` under `--jdk-only`.
// Since wave 59 (lane L5): HotSpot's lines in both modes with the switch; the
// public listing doors (`getFields`, `getField`, `getMethods`, `getMethod`,
// served without `getDeclaredFields0` / `getDeclaredMethods0`) resolve each
// visited class's public members' types (`resolve_public_listing_rung`).

import java.util.concurrent.Callable;

public class L5W58AppLoaderSignatureType {
    public static class Holder {
        public static class Gone {
        }

        public static Gone gone;

        public static void use(Gone g) {
        }

        public static void run() {
        }
    }

    public static class Fine {
        public static void run() {
        }
    }

    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage() + " cause=" + t.getCause();
        }
    }

    public static void main(String[] args) {
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("method-" + tag + "=" + attempt(() -> Holder.class.getMethod("run")));
            System.out.println("methods-" + tag + "=" + attempt(() -> Holder.class.getMethods().length));
            System.out.println("fields-" + tag + "=" + attempt(() -> Holder.class.getFields().length));
            System.out.println("ok-" + tag + "=" + attempt(() -> Fine.class.getDeclaredMethods().length));
        }
    }
}
