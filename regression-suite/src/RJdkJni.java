import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.zip.CRC32;
import java.util.zip.Checksum;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

/**
 * JDK-only corpus: JNI -- native library load, registered and symbol-bound
 * natives, exceptions, references.
 *
 * SCOPE NOTE (also recorded in regression-suite/jdk-only-coverage.txt): this
 * suite has no C toolchain step, so it cannot ship a purpose-built JNI library.
 * Building one would also be per-platform, which is exactly the kind of
 * nondeterminism the suite forbids. What this vector CAN do -- and what
 * actually matters for {@code --jdk-only}, where {@code ACC_NATIVE} methods
 * must bind to a real {@code NativeKind::Bridge} rather than a fabricated stub
 * -- is drive the JDK's OWN JNI-implemented natives end to end:
 *
 *   * {@code java.util.zip} (Deflater/Inflater/CRC32) is JNI over libzip, with
 *     an explicit {@code initIDs}/{@code RegisterNatives} pattern and a native
 *     handle whose lifecycle ({@code end()}, double-end, use-after-end) is
 *     observable from Java;
 *   * {@code System.loadLibrary} of a library that IS in the JDK image, and of
 *     one that is not (a real {@code UnsatisfiedLinkError});
 *   * calling an {@code ACC_NATIVE} method reflectively;
 *   * exceptions thrown out of native frames, including the argument checks a
 *     JNI method performs before touching the native handle.
 *
 * Determinism: library paths and {@code UnsatisfiedLinkError} messages embed
 * {@code java.library.path} and are NEVER printed -- only the exception TYPE.
 */
public class RJdkJni {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** ACC_NATIVE methods must be discoverable and correctly flagged. */
    static void nativeMethodMetadata() throws Exception {
        // Object.hashCode and System.currentTimeMillis are ACC_NATIVE on every
        // real JDK image; a fabricated java.lang.Object would not be.
        Method hashCode = Object.class.getDeclaredMethod("hashCode");
        check(Modifier.isNative(hashCode.getModifiers()),
                "Object.hashCode must be ACC_NATIVE on a real boot image");
        Method millis = System.class.getDeclaredMethod("currentTimeMillis");
        check(Modifier.isNative(millis.getModifiers()), "System.currentTimeMillis is native");
        check(Modifier.isStatic(millis.getModifiers()), "and static");

        List<String> nativeNames = new ArrayList<>();
        for (Method m : System.class.getDeclaredMethods()) {
            if (Modifier.isNative(m.getModifiers())) {
                nativeNames.add(m.getName());
            }
        }
        Collections.sort(nativeNames);
        check(nativeNames.contains("currentTimeMillis"), "System native list: " + nativeNames);
        check(nativeNames.contains("nanoTime"), "nanoTime must be native");
        check(nativeNames.contains("identityHashCode"), "identityHashCode must be native");
        check(nativeNames.contains("arraycopy"), "arraycopy must be native");

        // Reflective invocation of a native method must reach the binding.
        Object o = new Object();
        int viaReflection = (Integer) hashCode.invoke(o);
        check(viaReflection == o.hashCode(), "reflective native invoke must agree with direct");
        // `> 0` was satisfied by a reflective path that answered a constant, or
        // dropped through to a stub, without ever reaching the binding this
        // line is named for. Bracketed by two DIRECT calls to the same native
        // instead: relational, computed by the test, and it can only widen on a
        // slower host. Not a wall-clock window -- no fixed duration appears.
        long before = System.currentTimeMillis();
        long t = (Long) millis.invoke(null);
        long after = System.currentTimeMillis();
        check(t >= before && t <= after,
                "reflective static native invoke returned " + t + ", outside ["
                        + before + "," + after + "]");

        // arraycopy is a native with strict argument checks that must raise the
        // spec'd exception rather than corrupting memory.
        int[] src = { 1, 2, 3, 4 };
        int[] dst = new int[4];
        System.arraycopy(src, 0, dst, 0, 4);
        check(Arrays.equals(src, dst), "arraycopy");
        boolean threw = false;
        try {
            System.arraycopy(src, 0, dst, 0, 5);
        } catch (IndexOutOfBoundsException expected) {
            threw = true;
        }
        check(threw, "arraycopy overrun must raise IndexOutOfBoundsException");
        threw = false;
        try {
            System.arraycopy(src, 0, null, 0, 1);
        } catch (NullPointerException expected) {
            threw = true;
        }
        check(threw, "arraycopy with a null destination must NPE");
        threw = false;
        try {
            System.arraycopy(src, 0, new long[4], 0, 1);
        } catch (ArrayStoreException expected) {
            threw = true;
        }
        check(threw, "arraycopy between incompatible arrays must raise ArrayStoreException");
        System.out.println("CK RJdkJni nativeMeta systemNatives>=4=true reflectHash=true");
    }

    /** libzip: a JNI library with per-object native handles. */
    static void zipNatives() throws Exception {
        // CRC32 KATs -- fixed by the standard, so safe to print.
        CRC32 crc = new CRC32();
        check(crc.getValue() == 0L, "initial CRC");
        crc.update("123456789".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        check(crc.getValue() == 0xCBF43926L, "CRC32 of 123456789: " + crc.getValue());
        crc.reset();
        check(crc.getValue() == 0L, "CRC reset");
        Checksum adler = new java.util.zip.Adler32();
        adler.update("123456789".getBytes(java.nio.charset.StandardCharsets.UTF_8), 0, 9);
        check(adler.getValue() == 0x091E01DEL, "Adler32: " + adler.getValue());

        // Deflater/Inflater own a native handle each. Round-trip at a FIXED
        // compression level so the compressed length is deterministic.
        byte[] input = new byte[4096];
        for (int i = 0; i < input.length; i++) {
            input[i] = (byte) (i % 251);
        }
        Deflater def = new Deflater(Deflater.BEST_COMPRESSION);
        byte[] compressed = new byte[8192];
        int clen;
        try {
            def.setInput(input);
            def.finish();
            clen = def.deflate(compressed);
            check(def.finished(), "deflater finished");
            check(clen > 0 && clen < input.length, "compressed length: " + clen);
            check(def.getBytesRead() == input.length, "getBytesRead");
            check(def.getBytesWritten() == clen, "getBytesWritten");
        } finally {
            def.end();
        }

        Inflater inf = new Inflater();
        byte[] out = new byte[input.length];
        try {
            inf.setInput(compressed, 0, clen);
            int ilen = inf.inflate(out);
            check(ilen == input.length, "inflated length: " + ilen);
            check(Arrays.equals(out, input), "deflate/inflate round-trip");
            check(inf.finished(), "inflater finished");
        } finally {
            inf.end();
        }

        // Corrupt input must raise DataFormatException FROM the native frame.
        Inflater bad = new Inflater();
        boolean threw = false;
        try {
            bad.setInput(new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 });
            bad.inflate(new byte[64]);
        } catch (DataFormatException expected) {
            threw = true;
        } finally {
            bad.end();
        }
        check(threw, "corrupt deflate input must raise DataFormatException");

        // Use-after-end must be a clean Java-level error, not a crash. The JDK
        // does not pin WHICH unchecked exception, so accept the family.
        Deflater ended = new Deflater();
        ended.end();
        ended.end();   // double end must be a no-op, not a double free
        threw = false;
        try {
            ended.setInput(new byte[] { 1 });
            ended.deflate(new byte[16]);
        } catch (NullPointerException | IllegalStateException expected) {
            threw = true;
        }
        check(threw, "use-after-end must raise a Java exception, not crash the VM");
        System.out.println("CK RJdkJni crc32=" + 0xCBF43926L + " adler=" + adler.getValue()
                + " roundTrip=true");
    }

    static void libraryLoading() {
        // A library that ships INSIDE the JDK image must load, and loading it
        // twice must be idempotent.
        String loaded = "none";
        try {
            System.loadLibrary("zip");
            System.loadLibrary("zip");
            loaded = "zip";
        } catch (UnsatisfiedLinkError e) {
            // Some images fold libzip into libjava; net is the fallback probe.
            try {
                System.loadLibrary("net");
                loaded = "net";
            } catch (UnsatisfiedLinkError e2) {
                loaded = "none";
            }
        }
        check(loaded.equals("zip") || loaded.equals("net"),
                "a JDK-shipped native library must be loadable, got: " + loaded);
        // ...and WHICH one, which the line above cannot see. "zip" is exactly
        // the answer a too-wide VM allowlist manufactures: main() runs
        // zipNatives() first, so java.base has already boot-loaded zip into the
        // BOOT loader by the time this runs, and the JDK refuses the same
        // library file to a second class loader -- HotSpot 25 therefore throws
        // here and falls through to the net probe. A VM answering "zip" is
        // reporting success for a load the JDK forbids. Asserted rather than
        // left to the cross-VM CK diff, because that diff is SKIPPED whenever
        // no HotSpot is present on the host (run.sh says so), which is the
        // configuration this line has to survive.
        //
        // It is also the trip-wire for the other direction: recording the boot
        // loader's own libraries (jdk/internal/loader/BootLoader.loadLibrary is
        // a no-op today) would claim "net" first, turning this into "none". See
        // docs/known-issues/jdk-only/W5-1-loadlibrary-allowlist-too-wide.md and
        // section 2.6 of that directory's README.
        check(loaded.equals("net"),
                "System.loadLibrary must FAIL for zip once java.util.zip has boot-loaded it"
                        + " and fall through to net, got: " + loaded);

        // A library that does not exist must be a real UnsatisfiedLinkError. Its
        // MESSAGE embeds java.library.path, so only the type is asserted.
        boolean threw = false;
        try {
            System.loadLibrary("cratonvm_no_such_library_20260731");
        } catch (UnsatisfiedLinkError expected) {
            threw = true;
        }
        check(threw, "loading a missing library must raise UnsatisfiedLinkError");

        // System.load() with an absolute path that does not exist likewise.
        threw = false;
        try {
            System.load(new java.io.File("cratonvm-no-such.so").getAbsolutePath());
        } catch (UnsatisfiedLinkError expected) {
            threw = true;
        }
        check(threw, "System.load of a missing file must raise UnsatisfiedLinkError");

        // ------------------------------------------------------------------
        // The Runtime road, and the assertion that the NAME reaches the native.
        //
        // Runtime.load0/loadLibrary0 are INSTANCE methods, so a native body
        // sees args[0] = the Runtime receiver, args[1] = the fromClass mirror
        // and args[2] = the library name. A native that reads args[1] gets the
        // Class, whose string form is empty, and then EVERY load fails with an
        // empty name. Asserting only "it threw" cannot see that -- the wrong
        // index throws too -- so what is asserted here is that the error NAMES
        // the library the caller asked for. The message text still is not
        // printed (it embeds java.library.path); only these predicates are.
        String rtMissing = "cratonvm_no_such_library_20260812_runtime";
        String rtMsg = null;
        try {
            Runtime.getRuntime().loadLibrary(rtMissing);
        } catch (UnsatisfiedLinkError expected) {
            rtMsg = String.valueOf(expected.getMessage());
        }
        check(rtMsg != null,
                "Runtime.loadLibrary of a missing library must raise UnsatisfiedLinkError");
        check(rtMsg.contains(rtMissing),
                "Runtime.loadLibrary's UnsatisfiedLinkError must name the library asked for");

        // Same, one road down: Runtime.load(String) of an absolute path. Both
        // HotSpot's "Can't load library: <path>" and this VM's "no <path> in
        // java.library.path" contain the path, which is what is asserted; the
        // two SHAPES differ and that divergence is recorded separately.
        String rtPath = new java.io.File("cratonvm-no-such-runtime.so").getAbsolutePath();
        String rtLoadMsg = null;
        try {
            Runtime.getRuntime().load(rtPath);
        } catch (UnsatisfiedLinkError expected) {
            rtLoadMsg = String.valueOf(expected.getMessage());
        }
        check(rtLoadMsg != null,
                "Runtime.load of a missing file must raise UnsatisfiedLinkError");
        check(rtLoadMsg.contains(rtPath),
                "Runtime.load's UnsatisfiedLinkError must name the file asked for");

        // And the positive direction, which no "it threw" assertion can reach:
        // whatever System.loadLibrary just loaded must also load through
        // Runtime. Same VM, same class loader, same file, so the JDK answers
        // out of this loader's own cache -- the cross-loader
        // UnsatisfiedLinkError is not in range here.
        boolean rtLoaded = true;
        try {
            Runtime.getRuntime().loadLibrary(loaded);
        } catch (UnsatisfiedLinkError e) {
            rtLoaded = false;
        }
        check(rtLoaded, "Runtime.loadLibrary must load what System.loadLibrary loaded: " + loaded);

        // mapLibraryName is pure and platform-shaped.
        String mapped = System.mapLibraryName("foo");
        check(mapped.equals("foo.dll") || mapped.equals("libfoo.so")
                || mapped.equals("libfoo.dylib"), "mapLibraryName: " + mapped);
        System.out.println("CK RJdkJni loadedLibrary=" + loaded + " mapped=" + mapped);
    }

    /** An unbound native declared by user code must be an UnsatisfiedLinkError. */
    static native int neverBound(int x);

    static void unboundNative() {
        boolean threw = false;
        try {
            neverBound(1);
        } catch (UnsatisfiedLinkError expected) {
            threw = true;
        }
        check(threw, "calling an unbound native must raise UnsatisfiedLinkError");

        // ...and reflectively, wrapped.
        threw = false;
        try {
            Method m = RJdkJni.class.getDeclaredMethod("neverBound", int.class);
            check(Modifier.isNative(m.getModifiers()), "the declaration is ACC_NATIVE");
            m.setAccessible(true);
            m.invoke(null, 1);
        } catch (java.lang.reflect.InvocationTargetException e) {
            threw = e.getCause() instanceof UnsatisfiedLinkError;
        } catch (ReflectiveOperationException e) {
            threw = false;
        }
        check(threw, "reflective call of an unbound native must wrap UnsatisfiedLinkError");
        System.out.println("CK RJdkJni unboundNative=UnsatisfiedLinkError");
    }

    /** References across a native boundary must survive GC and stay identical. */
    static void referenceIdentity() {
        Object[] held = new Object[512];
        for (int i = 0; i < held.length; i++) {
            held[i] = new int[16];
        }
        int[] identity = new int[held.length];
        for (int i = 0; i < held.length; i++) {
            identity[i] = System.identityHashCode(held[i]);
        }
        // Churn the heap so a collection is likely, then re-check identity.
        for (int i = 0; i < 20000; i++) {
            Object junk = new byte[64];
            if (junk.hashCode() == 0) {
                held[0] = junk;   // never taken; defeats dead-code elimination
            }
        }
        System.gc();
        boolean stable = true;
        for (int i = 0; i < held.length; i++) {
            stable &= System.identityHashCode(held[i]) == identity[i];
            stable &= ((int[]) held[i]).length == 16;
        }
        check(stable, "identity hash codes must survive a collection");
        check(held[0] != held[1], "distinct objects stay distinct");
        System.out.println("CK RJdkJni referenceIdentityStable=" + stable);
    }

    public static void main(String[] args) throws Exception {
        nativeMethodMetadata();
        zipNatives();
        libraryLoading();
        unboundNative();
        referenceIdentity();
        System.out.println("CK RJdkJni checks=" + checks);
        System.out.println("PASS RJdkJni (" + checks + " checks)");
    }
}
