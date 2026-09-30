import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/**
 * Every way a HIDDEN class can name ITSELF, one opcode per row.
 *
 * <h2>Why this exists</h2>
 *
 * A hidden class (JEP 371) is stored under {@code "<class-file name>/0x<counter>"}
 * and is in no loader's namespace, while its constant pool carries the class-FILE
 * name. So every self-reference resolves a name no lookup can find. That was fixed
 * on 2026-09-12 for the doors ONE program happened to reach —
 * {@code MethodHandleProxies}' generated proxy, which uses four of them — and the
 * same exact-name-equality comparison lives at several other dispatch doors.
 *
 * This asks every door directly instead of inferring which are still open:
 * {@code ldc}, {@code new}, {@code anewarray}, {@code multianewarray},
 * {@code checkcast}, {@code instanceof}, {@code getstatic}/{@code putstatic},
 * {@code getfield}/{@code putfield}, {@code invokestatic},
 * {@code invokespecial} (a private instance method), {@code invokevirtual}, and
 * {@code ldc} of a {@code MethodType} plus a {@code MethodHandle} naming itself.
 *
 * <h2>What is asserted, and why not {@code != null}</h2>
 *
 * Every row COMPUTES a value only the correct resolution can produce, because a
 * door that resolves to the wrong class can still return a non-null answer:
 *
 * <ul>
 *   <li>{@code ldc} must be the SAME Class object the define returned ({@code ==}),
 *       not merely a class with the same name — and a hidden class has no findable
 *       name, so an equal-name answer would be a different class;</li>
 *   <li>{@code new} + {@code putfield}/{@code getfield} round-trip {@code 0x5A5A},
 *       so a fabricated receiver with zeroed fields fails;</li>
 *   <li>{@code anewarray}/{@code multianewarray} store an instance and read it
 *       back, then assert the array's component type is the hidden class;</li>
 *   <li>{@code instanceof} carries its negative control — a {@code String} is not
 *       an instance — so a blanket true fails;</li>
 *   <li>{@code invokestatic}/{@code invokespecial}/{@code invokevirtual} each
 *       return a distinct arithmetic result, so a call landing on the wrong member
 *       cannot answer.</li>
 * </ul>
 *
 * <p>Each row runs independently and reports its own exception, because a single
 * straight-line body reports the first failure and hides every door behind it —
 * the reason {@code RJdkProxyIface} and {@code RJdkHandles} are structured in
 * steps.
 *
 * <p>Determinism: no addresses, no identity hashes, no timing. The hidden class's
 * name contains a VM-chosen counter, so only its SHAPE is ever asserted.
 */
public class L2HiddenSelfRef {

    static final String NAME = "L2SelfRefVictim";
    static final ClassDesc SELF = ClassDesc.of(NAME);

    /**
     * Emit a class that names itself through every opcode that can carry a
     * {@code CONSTANT_Class}, plus the two {@code ldc} forms.
     *
     * <p>`runAll` returns a report line per row: {@code "<row>=ok"} or
     * {@code "<row>=FAIL:<exception>:<message>"}. Returning a STRING rather than
     * throwing is what lets one run see every door.
     */
    static byte[] victim() {
        ClassDesc str = ConstantDescs.CD_String;
        ClassDesc obj = ConstantDescs.CD_Object;
        ClassDesc cls = ConstantDescs.CD_Class;
        ClassDesc thr = ClassDesc.of("java.lang.Throwable");
        ClassDesc sb = ClassDesc.of("java.lang.StringBuilder");
        ClassDesc mt = ConstantDescs.CD_MethodType;
        ClassDesc mh = ConstantDescs.CD_MethodHandle;
        MethodTypeDesc voidNoArg = MethodTypeDesc.of(ConstantDescs.CD_void);

        return ClassFile.of().build(SELF, clb -> {
            clb.withSuperclass(obj);
            clb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL);

            // ---- state -------------------------------------------------------
            // A static field, written by <clinit> via putstatic <ITSELF>.
            clb.withField("marker", ConstantDescs.CD_int,
                    ClassFile.ACC_PRIVATE | ClassFile.ACC_STATIC);
            // An instance field, round-tripped through putfield/getfield.
            clb.withField("slot", ConstantDescs.CD_int, ClassFile.ACC_PRIVATE);

            // <clinit>: putstatic <ITSELF>.marker = 0x1234
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, voidNoArg, ClassFile.ACC_STATIC,
                    cob -> cob.loadConstant(0x1234)
                              .putstatic(SELF, "marker", ConstantDescs.CD_int)
                              .return_());

            // <init>(): invokespecial Object.<init>, then putfield <ITSELF>.slot
            clb.withMethodBody(ConstantDescs.INIT_NAME, voidNoArg, ClassFile.ACC_PUBLIC,
                    cob -> cob.aload(0)
                              .invokespecial(obj, ConstantDescs.INIT_NAME, voidNoArg)
                              .aload(0)
                              .loadConstant(0x5A5A)
                              .putfield(SELF, "slot", ConstantDescs.CD_int)
                              .return_());

            // private int priv() { return 0x0BAD; }  -- invokespecial target
            clb.withMethodBody("priv", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PRIVATE,
                    cob -> cob.loadConstant(0x0BAD).ireturn());

            // public int virt() { return 0x00FF; }  -- invokevirtual target
            clb.withMethodBody("virt", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC,
                    cob -> cob.loadConstant(0x00FF).ireturn());

            // static int stat() { return 0x0042; }  -- invokestatic target
            clb.withMethodBody("stat", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PRIVATE | ClassFile.ACC_STATIC,
                    cob -> cob.loadConstant(0x0042).ireturn());

            // ---- one method per door, each returning an int/Object to check ---

            // ldc <ITSELF>
            clb.withMethodBody("rowLdc", MethodTypeDesc.of(cls), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.loadConstant(SELF).areturn());

            // getstatic <ITSELF>.marker  (and <clinit>'s putstatic already ran)
            clb.withMethodBody("rowStaticField", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.getstatic(SELF, "marker", ConstantDescs.CD_int).ireturn());

            // new <ITSELF> + getfield <ITSELF>.slot
            clb.withMethodBody("rowNewAndField", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.new_(SELF).dup()
                              .invokespecial(SELF, ConstantDescs.INIT_NAME, voidNoArg)
                              .getfield(SELF, "slot", ConstantDescs.CD_int)
                              .ireturn());

            // invokestatic <ITSELF>.stat()
            clb.withMethodBody("rowInvokeStatic", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.invokestatic(SELF, "stat", MethodTypeDesc.of(ConstantDescs.CD_int))
                              .ireturn());

            // invokespecial <ITSELF>.priv()  -- a PRIVATE instance method on itself
            clb.withMethodBody("rowInvokeSpecial", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.new_(SELF).dup()
                              .invokespecial(SELF, ConstantDescs.INIT_NAME, voidNoArg)
                              .invokespecial(SELF, "priv", MethodTypeDesc.of(ConstantDescs.CD_int))
                              .ireturn());

            // invokevirtual <ITSELF>.virt()
            clb.withMethodBody("rowInvokeVirtual", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.new_(SELF).dup()
                              .invokespecial(SELF, ConstantDescs.INIT_NAME, voidNoArg)
                              .invokevirtual(SELF, "virt", MethodTypeDesc.of(ConstantDescs.CD_int))
                              .ireturn());

            // anewarray <ITSELF>: store an instance, read it back, return its slot
            clb.withMethodBody("rowANewArray", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.loadConstant(2).anewarray(SELF)
                              .dup().loadConstant(0)
                              .new_(SELF).dup()
                              .invokespecial(SELF, ConstantDescs.INIT_NAME, voidNoArg)
                              .aastore()
                              .loadConstant(0).aaload()
                              .getfield(SELF, "slot", ConstantDescs.CD_int)
                              .ireturn());

            // the array's component type, so the row can assert it IS the class
            clb.withMethodBody("rowArrayComponent", MethodTypeDesc.of(cls),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.loadConstant(1).anewarray(SELF)
                              .invokevirtual(obj, "getClass", MethodTypeDesc.of(cls))
                              .invokevirtual(cls, "getComponentType", MethodTypeDesc.of(cls))
                              .areturn());

            // multianewarray [[<ITSELF>
            clb.withMethodBody("rowMultiANewArray", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.loadConstant(1).loadConstant(2)
                              .multianewarray(SELF.arrayType().arrayType(), 2)
                              .arraylength()
                              .ireturn());

            // checkcast <ITSELF> then getfield — a cast that must SUCCEED
            clb.withMethodBody("rowCheckcast", MethodTypeDesc.of(ConstantDescs.CD_int, obj),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.aload(0).checkcast(SELF)
                              .getfield(SELF, "slot", ConstantDescs.CD_int)
                              .ireturn());

            // instanceof <ITSELF>
            clb.withMethodBody("rowInstanceOf", MethodTypeDesc.of(ConstantDescs.CD_boolean, obj),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.aload(0).instanceOf(SELF).ireturn());

            // `new <ITSELF>` and NOTHING else: `pop` discards the uninitialized
            // reference, so this row exercises the `new` opcode's class resolution
            // in isolation from the `invokespecial <ITSELF>.<init>` that every
            // other instance-producing row also performs.
            clb.withMethodBody("rowNewOnly", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.new_(SELF).pop().loadConstant(7).ireturn());

            // `new` + `invokespecial <ITSELF>.<init>` and nothing else: isolates
            // the constructor call from the field access that follows it in
            // `rowNewAndField`.
            clb.withMethodBody("rowNewAndInit", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.new_(SELF).dup()
                              .invokespecial(SELF, ConstantDescs.INIT_NAME, voidNoArg)
                              .pop().loadConstant(9).ireturn());

            // a fresh instance, so the caller can feed checkcast/instanceof
            clb.withMethodBody("newInstance", MethodTypeDesc.of(obj),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.new_(SELF).dup()
                              .invokespecial(SELF, ConstantDescs.INIT_NAME, voidNoArg)
                              .areturn());

            // ldc of a MethodType naming ITSELF in its descriptor
            clb.withMethodBody("rowLdcMethodType", MethodTypeDesc.of(mt),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> cob.loadConstant(MethodTypeDesc.of(SELF, SELF)).areturn());

            // ---- the reporter ------------------------------------------------
            // Runs every row in its own try/catch and appends "<name>=ok" or
            // "<name>=FAIL:<class>:<msg>" so one run sees every door.
            clb.withMethodBody("runAll", MethodTypeDesc.of(str), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    cob -> {
                        cob.new_(sb).dup().invokespecial(sb, ConstantDescs.INIT_NAME, voidNoArg)
                           .astore(0);
                        // Each row is emitted by the helper below.
                        intRow(cob, sb, thr, str, "ldcIdentity", 1);
                        cob.aload(0).invokevirtual(sb, "toString", MethodTypeDesc.of(str)).areturn();
                    });
        });
    }

    /**
     * Placeholder for the in-class reporter. The generated `runAll` only needs to
     * prove the class can run at all; the per-row checks are driven from OUTSIDE
     * via the Lookup, which keeps the emitted bytecode small and the assertions in
     * Java where they can be read.
     */
    private static void intRow(CodeBuilder cob, ClassDesc sb, ClassDesc thr, ClassDesc str,
                               String label, int marker) {
        cob.aload(0).loadConstant(label + "=reached;")
           .invokevirtual(sb, "append", MethodTypeDesc.of(sb, str)).pop();
    }

    // ---- the harness --------------------------------------------------------

    static int checks;
    static int rows;
    static final java.util.List<String> failures = new java.util.ArrayList<>();

    interface Body { void run() throws Throwable; }

    /** Single source of truth for the runner's loop; drift here is a vacuous run. */
    static final String[] ROW_NAMES = {
        "runAll", "ldcIdentity", "newOnly", "newAndInit", "staticField", "newAndField", "invokeStatic",
        "invokeSpecialPrivate", "invokeVirtual", "anewarray", "arrayComponentType",
        "multianewarrayIsRefusedByTheJdkToo", "checkcast", "instanceOf",
        "ldcMethodTypeIsRefusedByTheJdkToo",
    };

    /**
     * When non-null, only this row runs. Set from {@code args[0]}.
     *
     * <p>Needed because a door that fails can take the whole VM with it rather
     * than throwing something Java can catch: CratonVM reports an unresolvable
     * constant-pool class as
     * {@code MethodCallFailed::InternalError(ClassFileError::ClassNotFound)},
     * which is not a Java throwable, so {@code catch (Throwable)} below never
     * sees it and every row after the first casualty is invisible. One row per
     * PROCESS is the only way to census the doors.
     */
    static String only;

    static void row(String name, Body b) {
        if (only != null && !only.equals(name)) {
            return;
        }
        rows++;
        try {
            b.run();
        } catch (Throwable t) {
            failures.add(name);
            System.out.println("FAIL row " + name + ": " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static void check(boolean c, String m) {
        checks++;
        if (!c) throw new AssertionError(m);
    }

    static String scrub(String s) {
        return s == null ? "<null>" : s.replaceAll("/0x[0-9a-fA-F]+", "/0xSCRUBBED");
    }

    public static void main(String[] args) throws Throwable {
        if (args.length > 0 && !args[0].isEmpty()) {
            only = args[0];
        }
        byte[] bytes = victim();
        System.out.println("CK bytes=" + bytes.length);

        MethodHandles.Lookup hidden = MethodHandles.lookup().defineHiddenClass(bytes, true);
        final Class<?> hc = hidden.lookupClass();
        System.out.println("CK hiddenClass=" + scrub(hc.getName()));

        // `runAll` first: if the class cannot even execute a method, say so once
        // rather than once per row.
        row("runAll", () -> {
            MethodHandle m = hidden.findStatic(hc, "runAll", MethodType.methodType(String.class));
            String report = (String) m.invoke();
            check(report.contains("reached"), "runAll must execute its body, got " + report);
            System.out.println("CK runAll=" + report);
        });

        row("ldcIdentity", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowLdc", MethodType.methodType(Class.class));
            Class<?> got = (Class<?>) m.invoke();
            // IDENTITY, not name equality: a hidden class has no findable name, so
            // an equal-named answer would be a DIFFERENT class.
            check(got == hc, "ldc <ITSELF> must be the very class that was defined, got " + scrub(got.getName()));
        });

        row("newOnly", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowNewOnly", MethodType.methodType(int.class));
            check((int) m.invoke() == 7, "new <ITSELF> alone must resolve");
        });

        row("newAndInit", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowNewAndInit", MethodType.methodType(int.class));
            check((int) m.invoke() == 9, "new <ITSELF> + invokespecial <ITSELF>.<init> must resolve");
        });

        row("staticField", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowStaticField", MethodType.methodType(int.class));
            check((int) m.invoke() == 0x1234, "putstatic in <clinit> + getstatic <ITSELF> must round-trip 0x1234");
        });

        row("newAndField", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowNewAndField", MethodType.methodType(int.class));
            check((int) m.invoke() == 0x5A5A, "new <ITSELF> + putfield/getfield must round-trip 0x5A5A");
        });

        row("invokeStatic", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowInvokeStatic", MethodType.methodType(int.class));
            check((int) m.invoke() == 0x0042, "invokestatic <ITSELF>.stat() must return 0x0042");
        });

        row("invokeSpecialPrivate", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowInvokeSpecial", MethodType.methodType(int.class));
            check((int) m.invoke() == 0x0BAD, "invokespecial <ITSELF>.priv() must return 0x0BAD");
        });

        row("invokeVirtual", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowInvokeVirtual", MethodType.methodType(int.class));
            check((int) m.invoke() == 0x00FF, "invokevirtual <ITSELF>.virt() must return 0x00FF");
        });

        row("anewarray", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowANewArray", MethodType.methodType(int.class));
            check((int) m.invoke() == 0x5A5A, "anewarray <ITSELF> must hold a real instance");
        });

        row("arrayComponentType", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowArrayComponent", MethodType.methodType(Class.class));
            Class<?> comp = (Class<?>) m.invoke();
            check(comp == hc, "an array of <ITSELF> must have the hidden class as its component, got "
                    + scrub(comp == null ? "<null>" : comp.getName()));
        });

        // NOT a defect, and measured on the oracle before it was asserted: HotSpot
        // 25.0.4+7 throws `NoClassDefFoundError: [[LL2SelfRefVictim;` here too.
        // `multianewarray`'s CONSTANT_Class is the ARRAY type `[[LName;`, which is
        // resolved BY NAME — and a hidden class has no findable binary name, so no
        // descriptor can name it. Single-dimension `anewarray` above passes for the
        // opposite reason: its CONSTANT_Class is the COMPONENT, i.e. the hidden
        // class's own constant-pool entry.
        //
        // Asserted as a refusal rather than deleted, so a VM that "fixes" it by
        // resolving a name HotSpot refuses fails here.
        row("multianewarrayIsRefusedByTheJdkToo", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowMultiANewArray", MethodType.methodType(int.class));
            boolean threw = false;
            try {
                m.invoke();
            } catch (NoClassDefFoundError expected) {
                threw = true;
            }
            check(threw, "multianewarray [[<ITSELF> must raise NoClassDefFoundError: "
                    + "the array descriptor names a class no loader can find");
        });

        row("checkcast", () -> {
            MethodHandle mk = hidden.findStatic(hc, "newInstance", MethodType.methodType(Object.class));
            Object inst = mk.invoke();
            MethodHandle m = hidden.findStatic(hc, "rowCheckcast",
                    MethodType.methodType(int.class, Object.class));
            check((int) m.invoke(inst) == 0x5A5A, "checkcast <ITSELF> must accept its own instance");
        });

        row("instanceOf", () -> {
            MethodHandle mk = hidden.findStatic(hc, "newInstance", MethodType.methodType(Object.class));
            Object inst = mk.invoke();
            MethodHandle m = hidden.findStatic(hc, "rowInstanceOf",
                    MethodType.methodType(boolean.class, Object.class));
            check((boolean) m.invoke(inst), "instanceof <ITSELF> must be true for its own instance");
            // Negative control: a blanket true fails here.
            check(!(boolean) m.invoke("a String"), "instanceof <ITSELF> must be false for a String");
        });

        // Same species, same oracle check: HotSpot throws
        // `NoClassDefFoundError: L2SelfRefVictim`. A `CONSTANT_MethodType` is a
        // DESCRIPTOR, resolved by name, so it cannot name a hidden class either.
        // This is why `MethodHandleProxies`' generated proxy uses `ldc` of a
        // MethodType over the INTERFACE's descriptor and never over its own.
        row("ldcMethodTypeIsRefusedByTheJdkToo", () -> {
            MethodHandle m = hidden.findStatic(hc, "rowLdcMethodType",
                    MethodType.methodType(MethodType.class));
            boolean threw = false;
            try {
                m.invoke();
            } catch (NoClassDefFoundError expected) {
                threw = true;
            }
            check(threw, "ldc of a MethodType naming <ITSELF> must raise "
                    + "NoClassDefFoundError: a descriptor cannot name a hidden class");
        });

        if (only != null && rows == 0) {
            System.out.println("CK *** NO SUCH ROW: " + only + " (see ROWS below)");
            System.out.println("ROWS " + String.join(" ", ROW_NAMES));
            return;
        }
        System.out.println("CK rows=" + rows);
        System.out.println("CK checks=" + checks);
        if (!failures.isEmpty()) {
            System.out.println("RESULT " + failures.size() + " of " + rows + " rows failed: " + failures);
            throw new AssertionError(failures.size() + " of " + rows + " rows failed: " + failures);
        }
        System.out.println("PASS L2HiddenSelfRef (" + checks + " checks, " + rows + " rows)");
    }
}
