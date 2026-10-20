// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 21, lane L5: what one class definition costs in
// the VM's hook plumbing (stage 2 of
// docs/internal/fixed-bugs/interpreter-L5-proposal-one-owner-token-for-every-classloading-hook-FIXED-20260926.md).
//
// Every class definition releases a class-manager write guard, and the guard's
// drop fires the deferred-`new` resweep and the profile-replay sweep. Before
// wave 21 the hook registry was a Mutex<Vec<Weak<SharedVm>>> that each scoped
// fire locked, upgraded and copied into a fresh Vec; now it is a keyed map
// probed once per fire (`vm_init::hook_vm_for_domain`), with no allocation.
// The resweep only reaches the registry when a deferred-`new` retry is held
// and the replay sweep only with CRATONVM_JIT_PROFILE_LOAD, so on a default
// run the difference is expected to be small; the bench is here to show the
// stage costs nothing on the definition path. Compare --compatible, JIT on,
// interleaved with the previous build, 5 runs, medians of ns/class.
//
// Each round defines CLASSES fresh classes `GenR_N extends Object` (a minimal
// hand-built class file, no methods) through a new loader, then resolves one
// of them by name.
//
// stdout is deterministic: one "checksum=" line, identical on HotSpot 25.
// stderr: ns per defined class.
public class L5W21ClassDefineBench {
    static final int CLASSES = 400;

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W21ClassDefineBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    /// A class file for `name extends java/lang/Object`, public, no members.
    static byte[] classFile(String name) {
        byte[] nameUtf = name.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        byte[] objUtf = "java/lang/Object".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
        try {
            out.writeInt(0xCAFEBABE);
            out.writeShort(0); // minor
            out.writeShort(52); // major: Java 8
            out.writeShort(5); // constant pool count
            out.writeByte(1); // #1 Utf8 name
            out.writeShort(nameUtf.length);
            out.write(nameUtf);
            out.writeByte(7); // #2 Class #1
            out.writeShort(1);
            out.writeByte(1); // #3 Utf8 java/lang/Object
            out.writeShort(objUtf.length);
            out.write(objUtf);
            out.writeByte(7); // #4 Class #3
            out.writeShort(3);
            out.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
            out.writeShort(2); // this_class
            out.writeShort(4); // super_class
            out.writeShort(0); // interfaces
            out.writeShort(0); // fields
            out.writeShort(0); // methods
            out.writeShort(0); // attributes
            out.flush();
        } catch (java.io.IOException e) {
            throw new AssertionError(e);
        }
        return bytes.toByteArray();
    }

    static long round(int r) throws Exception {
        Loader loader = new Loader();
        long sum = 0;
        for (int i = 0; i < CLASSES; i++) {
            String name = "Gen" + r + "_" + i;
            Class<?> c = loader.define(name, classFile(name));
            sum += c.getName().length() + (c.getSuperclass() == Object.class ? 1 : 0);
        }
        Class<?> again = Class.forName("Gen" + r + "_" + (CLASSES / 2), false, loader);
        return sum + again.getName().hashCode();
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 25;
        long checksum = 0;
        // Warm-up rounds, untimed.
        for (int r = 0; r < 5; r++) {
            checksum += round(r);
        }
        long start = System.nanoTime();
        for (int r = 5; r < 5 + rounds; r++) {
            checksum += round(r);
        }
        long elapsed = System.nanoTime() - start;
        System.out.println("checksum=" + checksum);
        System.err.printf("ns/class=%.1f over %d classes%n",
                (double) elapsed / ((long) rounds * CLASSES), (long) rounds * CLASSES);
    }
}
