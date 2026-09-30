// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L3 -- `Instrumentation.setNativeMethodPrefix`
// (JVMTI `SetNativeMethodPrefix`): an agent wraps the native
// `java.util.zip.Adler32.update(II)I` by renaming it `$$w44$$update` (still
// native) and adding a Java `update(II)I` that records it ran and calls the
// renamed native. The renamed native has no implementation under its own name;
// the VM retries without the prefix and links it to `update`'s.
// docs/internal/fixed-bugs/interpreter-L3-native-method-prefix-is-never-supported-FIXED-20261008.md
//
// Rows:
//   supported      -- isNativeMethodPrefixSupported();
//   set-prefix     -- setNativeMethodPrefix(transformer, "$$w44$$") in premain;
//   set-unknown    -- setNativeMethodPrefix with a transformer never added;
//   checksum       -- Adler32 of "abc" byte by byte (update(int) -> update(II)I):
//                     `ok` when it is 38600999, or when the renamed native did
//                     not link (see below); anything else is printed as it is;
//   transformed    -- whether the transformer rewrote Adler32 (at its load, in `checksum`);
//   wrapper-ran    -- whether the Java wrapper ran (a system property it sets);
//   bulk-checksum  -- Adler32 of "abc" through update(byte[]) (updateBytes, not wrapped).
//
// HotSpot 25 prints (agent jar below; the same with -Xint):
//     supported: true
//     set-prefix: ok
//     set-unknown: java.lang.IllegalArgumentException: transformer not registered in setNativeMethodPrefix
//     checksum: ok
//     transformed: true
//     wrapper-ran: yes
//     bulk-checksum: 38600999
// Whether HotSpot links the renamed native differs by platform: JDK 25.0.3 on
// the Windows box linked it (the checksum was 38600999); JDK 25.0.4 on the
// Linux host threw `java.lang.UnsatisfiedLinkError: 'int
// java.util.zip.Adler32.$$w44$$update(int, int)'` from the wrapper (wave 45's
// host run). The `checksum` row prints `ok` for both (an
// `UnsatisfiedLinkError` naming the prefixed method, or the right value), so
// the probe prints the same on both; CratonVM always links it (the positive
// control below says through which door).
//
// CratonVM base `5248262b7` (read from the code): `supported: false`,
// `set-prefix: java.lang.UnsupportedOperationException: setNativeMethodPrefix
// is not supported in this environment`, `set-unknown` the same UOE; the
// transform then still renames the native, and the renamed native has no
// link. Wave 44 made the prefix supported, but the wave-45 host run showed
// `wrapper-ran: no` in all four modes (`--jdk-only` / `--compatible`, JIT /
// `--nojit`) on base `69568bea6`: the registered bridges on `Adler32`
// (`zip_streams.rs`'s `<init>` / `update(I)V` / `getValue` over a synthetic
// 1-field layout, `zip_real.rs`'s `update(II)I`) answered the calls in front
// of the TRANSFORMED class's bytecode (`invoke::resolve_step1_native`: a
// `Bridge` still wins over real bytecode unless the class was redefined), so
// neither the transformed `update(I)V` nor the wrapper ran. Since wave 45 a
// class a load-time transformer rewrote cedes its registered natives to the
// woven bytecode, as a redefined class does
// (`ClassManager::cede_native_shadows_to_woven_bytecode`); every row is
// expected to match in all four modes (read from the code; the host run
// decides). `transformed: false` on CratonVM would mean Adler32 was loaded
// before `premain` there, and the probe then shows nothing.
//
// Positive control: `CRATONVM_DBG_RETRANSFORM=1` prints
// `[instrument] load-time transform of java/util/zip/Adler32: its registered natives cede to the woven bytecode`
// once, then
// `[instrument] native prefix link: java/util/zip/Adler32.$$w44$$update(II)I -> update (registry)`
// once per call of the renamed native. The base prints the first line never,
// and the second never either (the wrapper that calls the renamed native did
// not run).
//
// SETUP (a manifest with the capability):
//     javac -d out tools/probes/interp/L3/L3W44NativeMethodPrefix.java
//     printf 'Premain-Class: L3W44NativeMethodPrefix$Agent\nCan-Set-Native-Method-Prefix: true\n' > m.txt
//     jar cfm probe.jar m.txt -C out .
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W44NativeMethodPrefix
// Without the agent both VMs print "no agent".
import java.lang.classfile.ClassFile;
import java.lang.classfile.ClassModel;
import java.lang.classfile.ClassTransform;
import java.lang.classfile.MethodModel;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.AccessFlag;
import java.security.ProtectionDomain;
import java.util.zip.Adler32;

public class L3W44NativeMethodPrefix {
    static final String PREFIX = "$$w44$$";
    static volatile Instrumentation inst;
    static volatile String setPrefix = "not run";
    static volatile String setUnknown = "not run";
    static volatile boolean transformed;

    static final class Wrapper implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader l, String name, Class<?> c, ProtectionDomain d, byte[] bytes) {
            if (!"java/util/zip/Adler32".equals(name)) {
                return null;
            }
            try {
                byte[] out = wrap(bytes);
                transformed = true;
                return out;
            } catch (Throwable t) {
                System.out.println("transform failed: " + t);
                return null;
            }
        }
    }

    static final class Unused implements ClassFileTransformer {
    }

    /** Rename the native `update(II)I` to PREFIX + "update" and add a Java wrapper. */
    static byte[] wrap(byte[] bytes) {
        ClassFile cf = ClassFile.of();
        ClassModel model = cf.parse(bytes);
        ClassDesc self = model.thisClass().asSymbol();
        MethodTypeDesc ii = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int, ConstantDescs.CD_int);
        MethodTypeDesc setProperty = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_String,
                ConstantDescs.CD_String);
        return cf.transformClass(model, (builder, element) -> {
            if (element instanceof MethodModel m
                    && m.methodName().equalsString("update")
                    && m.methodType().equalsString("(II)I")) {
                builder.withMethod(PREFIX + "update", ii, m.flags().flagsMask(), mb -> { });
                int flags = m.flags().flagsMask() & ~AccessFlag.NATIVE.mask();
                builder.withMethodBody("update", ii, flags, code -> code
                        .ldc("w44.wrapped")
                        .ldc("yes")
                        .invokestatic(ClassDesc.of("java.lang.System"), "setProperty", setProperty)
                        .pop()
                        .iload(0)
                        .iload(1)
                        .invokestatic(self, PREFIX + "update", ii)
                        .ireturn());
            } else {
                builder.with(element);
            }
        });
    }

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
            Wrapper wrapper = new Wrapper();
            instrumentation.addTransformer(wrapper);
            try {
                instrumentation.setNativeMethodPrefix(wrapper, PREFIX);
                setPrefix = "ok";
            } catch (Throwable t) {
                setPrefix = t.getClass().getName() + ": " + t.getMessage();
            }
            try {
                instrumentation.setNativeMethodPrefix(new Unused(), PREFIX);
                setUnknown = "ok";
            } catch (Throwable t) {
                setUnknown = t.getClass().getName() + ": " + t.getMessage();
            }
        }
    }

    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            out = String.valueOf(call.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        if (inst == null) {
            System.out.println("no agent");
            return;
        }
        row("supported", () -> inst.isNativeMethodPrefixSupported());
        row("set-prefix", () -> setPrefix);
        row("set-unknown", () -> setUnknown);
        row("checksum", () -> {
            Adler32 a = new Adler32();
            try {
                for (byte b : "abc".getBytes()) {
                    a.update(b);
                }
            } catch (UnsatisfiedLinkError e) {
                // The platform's HotSpot did not link the renamed native: the
                // wrapper ran and called it (see the header).
                String m = String.valueOf(e.getMessage());
                return m.contains(PREFIX + "update") ? "ok" : "UnsatisfiedLinkError: " + m;
            }
            long v = a.getValue();
            return v == 38600999L ? "ok" : String.valueOf(v);
        });
        row("transformed", () -> transformed);
        row("wrapper-ran", () -> System.getProperty("w44.wrapped", "no"));
        row("bulk-checksum", () -> {
            Adler32 a = new Adler32();
            a.update("abc".getBytes());
            return a.getValue();
        });
    }
}
