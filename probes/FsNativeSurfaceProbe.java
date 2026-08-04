// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Exercises the whole `java.io.FileSystem` native surface through the REAL JDK
// bytecode path, the way `java.io.File` reaches it once a test has redefined
// `java.io.File` (Mockito inline mock maker) and CratonVM's forced-native
// override for `File.exists()` / `File.getTotalSpace()` / ... stops applying.
//
// Every call below goes to the concrete `UnixFileSystem` / `WinNTFileSystem`
// wrapper, which is plain Java in JDK 22+ and delegates to a `*0`-suffixed
// JNI native. Those `*0` natives are what CratonVM has to register; calling
// `File.exists()` here would be answered by CratonVM's own `java/io/File`
// native and would prove nothing.
//
// Run with: --add-opens java.base/java.io=ALL-UNNAMED (HotSpot control).
// Output is one `KEY=value` line per operation so a CratonVM run and a HotSpot
// run can be diffed literally.

import java.io.File;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class FsNativeSurfaceProbe {

    private static Object FS;
    private static Class<?> FSC;
    private static int failures = 0;

    public static void main(String[] args) throws Exception {
        Field f = File.class.getDeclaredField("FS");
        f.setAccessible(true);
        FS = f.get(null);
        FSC = FS.getClass();
        System.out.println("FS_CLASS=" + FSC.getName());

        File dir = new File(System.getProperty("probe.dir", "fsprobe-tmp")).getAbsoluteFile();
        deleteTree(dir);
        if (!dir.mkdirs()) {
            System.out.println("SETUP_FAILED=cannot mkdir " + dir);
            System.exit(2);
        }

        File regular = new File(dir, "regular.txt");
        writeBytes(regular, 5);
        File subdir = new File(dir, "subdir");
        subdir.mkdir();
        File missing = new File(dir, "does-not-exist");
        File hidden = new File(dir, ".hidden");
        writeBytes(hidden, 3);

        // --- getBooleanAttributes0, via the public wrapper (ORs in BA_HIDDEN) ---
        p("BA_regular", call("getBooleanAttributes", new Class<?>[] { File.class }, regular));
        p("BA_dir", call("getBooleanAttributes", new Class<?>[] { File.class }, subdir));
        p("BA_missing", call("getBooleanAttributes", new Class<?>[] { File.class }, missing));
        p("BA_hidden", call("getBooleanAttributes", new Class<?>[] { File.class }, hidden));

        // --- hasBooleanAttributes: the exact frame in the reported stack ---
        Class<?>[] hasSig = new Class<?>[] { File.class, int.class };
        p("HAS_exists_regular", call("hasBooleanAttributes", hasSig, regular, 0x01));
        p("HAS_exists_missing", call("hasBooleanAttributes", hasSig, missing, 0x01));
        p("HAS_dir_subdir", call("hasBooleanAttributes", hasSig, subdir, 0x04));
        p("HAS_regular_regular", call("hasBooleanAttributes", hasSig, regular, 0x02));
        p("HAS_hidden_hidden", call("hasBooleanAttributes", hasSig, hidden, 0x08));
        p("HAS_hidden_regular", call("hasBooleanAttributes", hasSig, regular, 0x08));

        // --- checkAccess0: ACCESS_EXECUTE=1, ACCESS_WRITE=2, ACCESS_READ=4 ---
        Class<?>[] accSig = new Class<?>[] { File.class, int.class };
        p("ACC_read_regular", call("checkAccess", accSig, regular, 4));
        p("ACC_write_regular", call("checkAccess", accSig, regular, 2));
        p("ACC_read_missing", call("checkAccess", accSig, missing, 4));
        p("ACC_write_missing", call("checkAccess", accSig, missing, 2));

        // --- getLength0 / getLastModifiedTime0 ---
        p("LEN_regular", call("getLength", new Class<?>[] { File.class }, regular));
        p("LEN_missing", call("getLength", new Class<?>[] { File.class }, missing));
        Object mtime = call("getLastModifiedTime", new Class<?>[] { File.class }, regular);
        p("MTIME_regular_nonzero", nonZeroLong(mtime));
        p("MTIME_missing", call("getLastModifiedTime", new Class<?>[] { File.class }, missing));

        // --- list0 ---
        Object listed = call("list", new Class<?>[] { File.class }, dir);
        p("LIST_dir", sortedJoin(listed));
        // The descriptor says `[Ljava/lang/String;`, so the array's runtime
        // class has to be `String[]` — an `Object[]` would fail the implicit
        // checkcast in `File.normalizedList()`.
        p("LIST_dir_array_class", arrayClass(listed));
        p("LIST_missing", call("list", new Class<?>[] { File.class }, missing));
        // `null`, not an empty array — `File.list()` on a plain file must not
        // look like an empty directory.
        p("LIST_on_regular_file", call("list", new Class<?>[] { File.class }, regular));
        // The same question for the two `java.io.File` entry points, which
        // CratonVM answers with its own natives rather than this FileSystem.
        p("FILE_list_array_class", arrayClass(dir.list()));
        p("FILE_listFiles_array_class", arrayClass(dir.listFiles()));

        // --- createDirectory0 ---
        File newDir = new File(dir, "made");
        p("MKDIR_new", call("createDirectory", new Class<?>[] { File.class }, newDir));
        p("MKDIR_again", call("createDirectory", new Class<?>[] { File.class }, newDir));

        // --- createFileExclusively0 ---
        File excl = new File(dir, "excl.txt");
        p("EXCL_new", call("createFileExclusively", new Class<?>[] { String.class }, excl.getPath()));
        p("EXCL_again", call("createFileExclusively", new Class<?>[] { String.class }, excl.getPath()));

        // --- setLastModifiedTime0, then read back through getLastModifiedTime0 ---
        long stamp = 1_234_567_890_000L;
        p("SETMTIME_ok", call("setLastModifiedTime", new Class<?>[] { File.class, long.class }, regular, stamp));
        p("SETMTIME_readback", call("getLastModifiedTime", new Class<?>[] { File.class }, regular));

        // --- setReadOnly0, observed through checkAccess0(ACCESS_WRITE) ---
        File ro = new File(dir, "readonly.txt");
        writeBytes(ro, 2);
        p("RO_set", call("setReadOnly", new Class<?>[] { File.class }, ro));
        p("RO_write_after", call("checkAccess", accSig, ro, 2));
        p("RO_read_after", call("checkAccess", accSig, ro, 4));

        // --- setPermission0: put the write bit back, owner-only ---
        Class<?>[] permSig = new Class<?>[] { File.class, int.class, boolean.class, boolean.class };
        p("PERM_rewrite", call("setPermission", permSig, ro, 2, true, true));
        p("PERM_write_after", call("checkAccess", accSig, ro, 2));

        // --- getSpace0: SPACE_TOTAL=0, SPACE_FREE=1, SPACE_USABLE=2 ---
        Class<?>[] spaceSig = new Class<?>[] { File.class, int.class };
        p("SPACE_total_nonzero", nonZeroLong(call("getSpace", spaceSig, dir, 0)));
        p("SPACE_free_nonzero", nonZeroLong(call("getSpace", spaceSig, dir, 1)));
        p("SPACE_usable_nonzero", nonZeroLong(call("getSpace", spaceSig, dir, 2)));
        p("SPACE_total_missing_dir_is_zero", isZeroLong(call("getSpace", spaceSig,
                new File(dir, "no/such/mount"), 0)));

        // --- getNameMax0 ---
        p("NAMEMAX_positive", positiveInt(call("getNameMax", new Class<?>[] { String.class }, dir.getPath())));

        // --- canonicalize0 ---
        File weird = new File(dir, "subdir/../regular.txt");
        Object canon = call("canonicalize", new Class<?>[] { String.class }, weird.getPath());
        p("CANON_collapses", String.valueOf(canon).equals(regular.getPath()));

        // --- rename0 ---
        File renameSrc = new File(dir, "rn-src.txt");
        File renameDst = new File(dir, "rn-dst.txt");
        writeBytes(renameSrc, 1);
        p("RENAME_ok", call("rename", new Class<?>[] { File.class, File.class }, renameSrc, renameDst));
        p("RENAME_dst_exists", call("hasBooleanAttributes", hasSig, renameDst, 0x01));

        // --- delete0 ---
        p("DELETE_file", call("delete", new Class<?>[] { File.class }, renameDst));
        p("DELETE_again", call("delete", new Class<?>[] { File.class }, renameDst));
        p("DELETE_dir", call("delete", new Class<?>[] { File.class }, newDir));

        // --- listRoots0 / getDriveDirectory: WinNTFileSystem only ---
        if (FSC.getName().endsWith("WinNTFileSystem")) {
            Object roots = call("listRoots", new Class<?>[] {});
            p("ROOTS_count_positive", (roots instanceof File[]) && ((File[]) roots).length > 0);
            Object cwdDrive = call("getDriveDirectory", new Class<?>[] { int.class },
                    Character.toUpperCase(dir.getPath().charAt(0)) - 'A' + 1);
            // The JDK's native strips the `X:` prefix, so this is the current
            // directory on that drive as a bare, backslash-rooted path.
            p("DRIVEDIR_is_rooted", String.valueOf(cwdDrive).startsWith("\\"));
        }

        deleteTree(dir);
        System.out.println("PROBE_FAILURES=" + failures);
        System.out.println(failures == 0 ? "PROBE_RESULT=OK" : "PROBE_RESULT=INCOMPLETE");
    }

    /** Invokes a `FileSystem` method reflectively, reporting a throw as the value. */
    private static Object call(String name, Class<?>[] sig, Object... args) {
        try {
            // `hasBooleanAttributes` is overridden on `UnixFileSystem` but only
            // inherited from the abstract `java.io.FileSystem` on Windows, so a
            // `getDeclaredMethod` on the concrete class alone misses it there.
            Method m = null;
            for (Class<?> c = FSC; c != null && m == null; c = c.getSuperclass()) {
                try {
                    m = c.getDeclaredMethod(name, sig);
                } catch (NoSuchMethodException ignored) {
                    // keep walking up to `java.io.FileSystem`
                }
            }
            if (m == null) {
                throw new NoSuchMethodException(FSC.getName() + "." + name);
            }
            m.setAccessible(true);
            return m.invoke(FS, args);
        } catch (java.lang.reflect.InvocationTargetException e) {
            failures++;
            Throwable c = e.getCause();
            return "THREW:" + c.getClass().getName() + ":" + shortMsg(c.getMessage());
        } catch (Throwable t) {
            failures++;
            return "UNCALLABLE:" + t.getClass().getName() + ":" + shortMsg(t.getMessage());
        }
    }

    private static String shortMsg(String m) {
        if (m == null) {
            return "";
        }
        int nl = m.indexOf('\n');
        return nl < 0 ? m : m.substring(0, nl);
    }

    private static void p(String key, Object value) {
        System.out.println(key + "=" + value);
    }

    private static Object nonZeroLong(Object v) {
        return (v instanceof Long) ? Boolean.valueOf((Long) v != 0L) : v;
    }

    private static Object isZeroLong(Object v) {
        return (v instanceof Long) ? Boolean.valueOf((Long) v == 0L) : v;
    }

    private static Object positiveInt(Object v) {
        return (v instanceof Integer) ? Boolean.valueOf((Integer) v > 0) : v;
    }

    private static String arrayClass(Object arr) {
        return arr == null ? "null" : arr.getClass().getName();
    }

    private static String sortedJoin(Object arr) {
        if (!(arr instanceof String[])) {
            return String.valueOf(arr);
        }
        String[] a = ((String[]) arr).clone();
        java.util.Arrays.sort(a);
        return String.join(",", a);
    }

    private static void writeBytes(File f, int n) throws Exception {
        try (java.io.FileOutputStream out = new java.io.FileOutputStream(f)) {
            for (int i = 0; i < n; i++) {
                out.write('x');
            }
        }
    }

    private static void deleteTree(File f) {
        File[] kids = f.listFiles();
        if (kids != null) {
            for (File k : kids) {
                deleteTree(k);
            }
        }
        f.delete();
    }
}
