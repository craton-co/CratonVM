// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java Class-File API (JEP 484, JDK 24) native method registrations.
//!
//! FLAGGED SyntheticStub: CratonVM does not actually implement JEP 484. The
//! accessors below fabricate canned model objects, but the *productive*
//! entry points — the ones whose wrong answers would silently corrupt a
//! caller (`ClassFile.parse`, `build`/`buildTo`/`buildModule`/`transformClass`,
//! `ClassBuilder.build`, `ClassTransform.transformClass`) — throw a clear
//! `UnsupportedOperationException` instead of returning a fixed ClassModel or
//! an empty `byte[]`. This honours the no-stubs policy: a real Class-File API
//! user fails loudly (and catchably, since it is a normal Java exception)
//! rather than receiving fabricated bytes. The whole surface stays tagged
//! [`NativeKind::SyntheticStub`].

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

/// Throw a clear `UnsupportedOperationException` from a Class-File API entry
/// point that CratonVM cannot honestly implement (real parsing / bytecode
/// generation). Catchable Java-side, so callers degrade gracefully instead
/// of consuming fabricated bytes.
fn classfile_unsupported(method: &str) -> MethodCallResult {
    Err(MethodCallFailed::from(RuntimeError::UnsupportedOperationException {
        message: format!(
            "java.lang.classfile.{method}: the JEP 484 Class-File API is not implemented by CratonVM"
        ),
    }))
}

/// Build a synthetic `Optional.empty()`. Several model accessors declare an
/// `Optional` return; handing back a bare null there NPEs at the call site the
/// moment the caller does the obligatory `isPresent()`/`orElse(...)`.
fn empty_optional(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
    ctx.set_field(opt, 0, Value::Object(None));
    Ok(opt)
}

// ---------------------------------------------------------------------------
// Class file version constants
// ---------------------------------------------------------------------------
const CLASSFILE_MAJOR_45: u16 = 45; // Java 1.1
const CLASSFILE_MAJOR_52: u16 = 52; // Java 8
const CLASSFILE_MAJOR_55: u16 = 55; // Java 11
const CLASSFILE_MAJOR_61: u16 = 61; // Java 17
const CLASSFILE_MAJOR_65: u16 = 65; // Java 21
const CLASSFILE_MAJOR_68: u16 = 68; // Java 24
const CLASSFILE_MAJOR_69: u16 = 69; // Java 25

// Access flags
const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_PROTECTED: u16 = 0x0004;
const ACC_STATIC: u16 = 0x0008;
const ACC_FINAL: u16 = 0x0010;
const ACC_SUPER: u16 = 0x0020;
const ACC_SYNCHRONIZED: u16 = 0x0020;
const ACC_VOLATILE: u16 = 0x0040;
const ACC_BRIDGE: u16 = 0x0040;
const ACC_TRANSIENT: u16 = 0x0080;
const ACC_VARARGS: u16 = 0x0080;
const ACC_NATIVE: u16 = 0x0100;
const ACC_INTERFACE: u16 = 0x0200;
const ACC_ABSTRACT: u16 = 0x0400;
const ACC_STRICT: u16 = 0x0800;
const ACC_SYNTHETIC: u16 = 0x1000;
const ACC_ANNOTATION: u16 = 0x2000;
const ACC_ENUM: u16 = 0x4000;
const ACC_MODULE: u16 = 0x8000;

// TypeKind enum simulation
const TK_BYTE: i32 = 0;
const TK_SHORT: i32 = 1;
const TK_INT: i32 = 2;
const TK_LONG: i32 = 3;
const TK_FLOAT: i32 = 4;
const TK_DOUBLE: i32 = 5;
const TK_CHAR: i32 = 6;
const TK_BOOLEAN: i32 = 7;
const TK_REFERENCE: i32 = 8;
const TK_VOID: i32 = 9;

// ---------------------------------------------------------------------------
// ClassFile — 2-field synthetic: [0]=options_mask, [1]=major_version
// ---------------------------------------------------------------------------

fn register_classfile(r: &mut NativeMethodRegistry) {
    let cf = "java/lang/classfile/ClassFile";

    // of() -> ClassFile (default options, latest version)
    r.register(cf, "of", "()Ljava/lang/classfile/ClassFile;", |ctx, _| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/ClassFile", 2)?;
        ctx.set_field(obj, 0, Value::Int(0));
        ctx.set_field(obj, 1, Value::Int(CLASSFILE_MAJOR_69 as i32));
        Ok(Some(Value::Object(Some(obj))))
    });

    // of(ClassFile$Option...) -> ClassFile
    r.register(
        cf,
        "of",
        "([Ljava/lang/classfile/ClassFile$Option;)Ljava/lang/classfile/ClassFile;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/ClassFile", 2)?;
            ctx.set_field(obj, 0, Value::Int(1)); // has options
            ctx.set_field(obj, 1, Value::Int(CLASSFILE_MAJOR_69 as i32));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // parse(byte[]) -> ClassModel
    // Real parsing is not implemented; a fabricated ClassModel would silently
    // misreport the class structure. Fail loudly (catchable) instead.
    r.register(
        cf,
        "parse",
        "([B)Ljava/lang/classfile/ClassModel;",
        |_ctx, _args| classfile_unsupported("ClassFile.parse(byte[])"),
    );

    // parse(Path) -> ClassModel
    r.register(
        cf,
        "parse",
        "(Ljava/nio/file/Path;)Ljava/lang/classfile/ClassModel;",
        |_ctx, _args| classfile_unsupported("ClassFile.parse(Path)"),
    );

    // build(ClassDesc, Consumer<ClassBuilder>) -> byte[]
    // Returning an empty byte[] fabricates a "successful" but invalid class.
    // Fail loudly instead so callers do not write a corrupt class file.
    r.register(
        cf,
        "build",
        "(Ljava/lang/constant/ClassDesc;Ljava/util/function/Consumer;)[B",
        |_ctx, _args| classfile_unsupported("ClassFile.build"),
    );

    // buildTo(Path, ClassDesc, Consumer<ClassBuilder>) -> void
    r.register(
        cf,
        "buildTo",
        "(Ljava/nio/file/Path;Ljava/lang/constant/ClassDesc;Ljava/util/function/Consumer;)V",
        |_ctx, _args| classfile_unsupported("ClassFile.buildTo"),
    );

    // buildModule(ModuleDesc, Consumer) -> byte[]
    r.register(
        cf,
        "buildModule",
        "(Ljava/lang/module/ModuleDescriptor;Ljava/util/function/Consumer;)[B",
        |_ctx, _args| classfile_unsupported("ClassFile.buildModule"),
    );

    // transformClass(ClassModel, ClassTransform) -> byte[]
    r.register(
        cf,
        "transformClass",
        "(Ljava/lang/classfile/ClassModel;Ljava/lang/classfile/ClassTransform;)[B",
        |_ctx, _args| classfile_unsupported("ClassFile.transformClass"),
    );

    // latestMajorVersion() -> int
    r.register(cf, "latestMajorVersion", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(CLASSFILE_MAJOR_69 as i32)))
    });

    // latestMinorVersion() -> int
    // KEEP: spec-correct constant. Every class file format from 45.3 onward
    // uses minor version 0 (only the preview-feature marker 65535 differs, and
    // that is not "latest"), so the real JDK also returns 0 here.
    r.register(cf, "latestMinorVersion", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
}

// ---------------------------------------------------------------------------
// ClassModel — 6-field synthetic
// ---------------------------------------------------------------------------

fn register_class_model(r: &mut NativeMethodRegistry) {
    let cm = "java/lang/classfile/ClassModel";

    r.register(cm, "majorVersion", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0);
        Ok(Some(v))
    });

    r.register(cm, "minorVersion", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 1);
        Ok(Some(v))
    });

    r.register(
        cm,
        "flags",
        "()Ljava/lang/classfile/AccessFlags;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let flags = ctx.get_field(this, 2);
            let af = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/AccessFlags", 1)?;
            ctx.set_field(af, 0, flags);
            Ok(Some(Value::Object(Some(af))))
        },
    );

    r.register(
        cm,
        "thisClass",
        "()Ljava/lang/classfile/constantpool/ClassEntry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 3);
            let entry = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/classfile/constantpool/ClassEntry",
                1,
            )?;
            ctx.set_field(entry, 0, idx);
            Ok(Some(Value::Object(Some(entry))))
        },
    );

    r.register(cm, "superclass", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 4);
        let entry =
            try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/constantpool/ClassEntry", 1)?;
        ctx.set_field(entry, 0, idx);
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(Some(entry)));
        Ok(Some(Value::Object(Some(opt))))
    });

    r.register(cm, "interfaces", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
        ctx.set_field(list, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    r.register(cm, "fields", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
        ctx.set_field(list, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    r.register(cm, "methods", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
        ctx.set_field(list, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    r.register(cm, "attributes", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
        ctx.set_field(list, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    r.register(
        cm,
        "constantPool",
        "()Ljava/lang/classfile/constantpool/ConstantPool;",
        |ctx, _args| {
            let cp = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/classfile/constantpool/ConstantPool",
                2,
            )?;
            ctx.set_field(cp, 0, Value::Int(0));
            ctx.set_field(cp, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(cp))))
        },
    );

    // A class file describes a module iff ACC_MODULE (0x8000) is set in its
    // access flags. Read the model's real flags word (field 2, the same slot
    // `flags()` above exposes) instead of answering a blanket false.
    r.register(cm, "isModuleInfo", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flags = match ctx.get_field(this, 2) {
            Value::Int(n) => n,
            _ => 0,
        };
        let is_module = (flags & i32::from(ACC_MODULE)) != 0;
        Ok(Some(Value::Int(if is_module { 1 } else { 0 })))
    });

    r.register(cm, "className", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("UnknownClass");
        Ok(Some(Value::Object(Some(s))))
    });

    r.register(
        cm,
        "superclassEntry",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 4);
            let entry = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/classfile/constantpool/ClassEntry",
                1,
            )?;
            ctx.set_field(entry, 0, idx);
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(Some(entry)));
            Ok(Some(Value::Object(Some(opt))))
        },
    );
}

// ---------------------------------------------------------------------------
// MethodModel — 4-field synthetic
// ---------------------------------------------------------------------------

fn register_method_model(r: &mut NativeMethodRegistry) {
    let mm = "java/lang/classfile/MethodModel";

    r.register(
        mm,
        "flags",
        "()Ljava/lang/classfile/AccessFlags;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let flags = ctx.get_field(this, 0);
            let af = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/AccessFlags", 1)?;
            ctx.set_field(af, 0, flags);
            Ok(Some(Value::Object(Some(af))))
        },
    );

    r.register(
        mm,
        "methodName",
        "()Ljava/lang/classfile/constantpool/Utf8Entry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 1);
            let entry = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/classfile/constantpool/Utf8Entry",
                1,
            )?;
            ctx.set_field(entry, 0, idx);
            Ok(Some(Value::Object(Some(entry))))
        },
    );

    r.register(
        mm,
        "methodType",
        "()Ljava/lang/classfile/constantpool/Utf8Entry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 2);
            let entry = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/classfile/constantpool/Utf8Entry",
                1,
            )?;
            ctx.set_field(entry, 0, idx);
            Ok(Some(Value::Object(Some(entry))))
        },
    );

    r.register(
        mm,
        "methodTypeSymbol",
        "()Ljava/lang/constant/MethodTypeDesc;",
        |ctx, _args| {
            let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/constant/MethodTypeDesc", 1)?;
            ctx.set_field(desc, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    r.register(mm, "code", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code_len = ctx.get_field(this, 3);
        let has_code = match code_len {
            Value::Int(n) => n > 0,
            _ => false,
        };
        if has_code {
            let code = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/CodeModel", 4)?;
            ctx.set_field(code, 0, Value::Int(0)); // max_stack
            ctx.set_field(code, 1, Value::Int(0)); // max_locals
            ctx.set_field(code, 2, code_len);
            ctx.set_field(code, 3, Value::Int(0)); // exception_handler_count
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(Some(code)));
            Ok(Some(Value::Object(Some(opt))))
        } else {
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(opt))))
        }
    });

    r.register(mm, "attributes", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
        ctx.set_field(list, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    // MethodModel.parent() -> Optional<ClassModel>. These synthetic models are
    // fabricated standalone (no ClassModel owns them), so the honest answer is
    // Optional.empty() — never a bare null, which the declared type forbids.
    r.register(mm, "parent", "()Ljava/util/Optional;", |ctx, _args| {
        let opt = empty_optional(ctx)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    
}

// ---------------------------------------------------------------------------
// FieldModel — 3-field synthetic
// ---------------------------------------------------------------------------

fn register_field_model(r: &mut NativeMethodRegistry) {
    let fm = "java/lang/classfile/FieldModel";

    r.register(
        fm,
        "flags",
        "()Ljava/lang/classfile/AccessFlags;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let flags = ctx.get_field(this, 0);
            let af = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/AccessFlags", 1)?;
            ctx.set_field(af, 0, flags);
            Ok(Some(Value::Object(Some(af))))
        },
    );

    r.register(
        fm,
        "fieldName",
        "()Ljava/lang/classfile/constantpool/Utf8Entry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 1);
            let entry = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/classfile/constantpool/Utf8Entry",
                1,
            )?;
            ctx.set_field(entry, 0, idx);
            Ok(Some(Value::Object(Some(entry))))
        },
    );

    r.register(
        fm,
        "fieldType",
        "()Ljava/lang/classfile/constantpool/Utf8Entry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 2);
            let entry = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/classfile/constantpool/Utf8Entry",
                1,
            )?;
            ctx.set_field(entry, 0, idx);
            Ok(Some(Value::Object(Some(entry))))
        },
    );

    r.register(
        fm,
        "fieldTypeSymbol",
        "()Ljava/lang/constant/ClassDesc;",
        |ctx, _args| {
            let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/constant/ClassDesc", 1)?;
            ctx.set_field(desc, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    r.register(fm, "attributes", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
        ctx.set_field(list, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    // FieldModel.parent() -> Optional<ClassModel>; unattached synthetic model,
    // so Optional.empty() rather than a bare null (see MethodModel.parent).
    r.register(fm, "parent", "()Ljava/util/Optional;", |ctx, _args| {
        let opt = empty_optional(ctx)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    
}

// ---------------------------------------------------------------------------
// CodeModel — 4-field synthetic
// ---------------------------------------------------------------------------

fn register_code_model(r: &mut NativeMethodRegistry) {
    let code = "java/lang/classfile/CodeModel";

    r.register(code, "maxStack", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    r.register(code, "maxLocals", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    r.register(code, "codeLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });

    r.register(
        code,
        "exceptionHandlers",
        "()Ljava/util/List;",
        |ctx, _args| {
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
            ctx.set_field(list, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(list))))
        },
    );

    r.register(code, "elements", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 1)?;
        ctx.set_field(list, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    // CodeModel.parent() -> Optional<MethodModel>; unattached synthetic model,
    // so Optional.empty() rather than a bare null (see MethodModel.parent).
    r.register(code, "parent", "()Ljava/util/Optional;", |ctx, _args| {
        let opt = empty_optional(ctx)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    
}

// ---------------------------------------------------------------------------
// ClassBuilder — 3-field synthetic
// ---------------------------------------------------------------------------

fn register_class_builder(r: &mut NativeMethodRegistry) {
    let cb = "java/lang/classfile/ClassBuilder";

    r.register(
        cb,
        "withFlags",
        "(I)Ljava/lang/classfile/ClassBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let flags = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            ctx.set_field(this, 1, Value::Int(flags));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    r.register(
        cb,
        "withSuperclass",
        "(Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/ClassBuilder;",
        |_ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(this))))
        },
    );

    r.register(
        cb,
        "withInterfaceSymbols",
        "(Ljava/util/List;)Ljava/lang/classfile/ClassBuilder;",
        |_ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // withField(String, ClassDesc, Consumer<FieldBuilder>)
    r.register(cb, "withField", "(Ljava/lang/String;Ljava/lang/constant/ClassDesc;Ljava/util/function/Consumer;)Ljava/lang/classfile/ClassBuilder;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, 2) {
            Value::Int(n) => n,
            _ => 0,
        };
        ctx.set_field(this, 2, Value::Int(count + 1));
        Ok(Some(Value::Object(Some(this))))
    });

    // withField(String, ClassDesc, int)
    r.register(
        cb,
        "withField",
        "(Ljava/lang/String;Ljava/lang/constant/ClassDesc;I)Ljava/lang/classfile/ClassBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Int(count + 1));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // withMethod(String, MethodTypeDesc, int, Consumer<MethodBuilder>)
    r.register(cb, "withMethod", "(Ljava/lang/String;Ljava/lang/constant/MethodTypeDesc;ILjava/util/function/Consumer;)Ljava/lang/classfile/ClassBuilder;", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Object(Some(this))))
    });

    // withMethodBody(String, MethodTypeDesc, int, Consumer<CodeBuilder>)
    r.register(cb, "withMethodBody", "(Ljava/lang/String;Ljava/lang/constant/MethodTypeDesc;ILjava/util/function/Consumer;)Ljava/lang/classfile/ClassBuilder;", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Object(Some(this))))
    });

    // with(ClassElement)
    r.register(
        cb,
        "with",
        "(Ljava/lang/classfile/ClassElement;)Ljava/lang/classfile/ClassBuilder;",
        |_ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // build() -> byte[]
    // An empty byte[] is an invalid class file; fail loudly instead.
    r.register(cb, "build", "()[B", |_ctx, _args| {
        classfile_unsupported("ClassBuilder.build")
    });
}

// ---------------------------------------------------------------------------
// CodeBuilder — 3-field synthetic: [0]=instruction_count, [1]=label_count,
//                                   [2]=block_depth
// ---------------------------------------------------------------------------

fn cb_inc_insn(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let count = match ctx.get_field(this, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    ctx.set_field(this, 0, Value::Int(count + 1));
}

fn register_code_builder(r: &mut NativeMethodRegistry) {
    let cb = "java/lang/classfile/CodeBuilder";

    // --- Load / Store instructions ---
    for (name, desc) in &[
        ("aload", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("astore", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("iload", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("istore", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("lload", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("lstore", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("fload", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("fstore", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("dload", "(I)Ljava/lang/classfile/CodeBuilder;"),
        ("dstore", "(I)Ljava/lang/classfile/CodeBuilder;"),
    ] {
        r.register(cb, name, desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        });
    }

    // iconst(int)
    r.register(
        cb,
        "iconst",
        "(I)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // lconst(long)
    r.register(
        cb,
        "lconst",
        "(J)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // Zero-arg instructions returning CodeBuilder
    for name in &[
        "aconst_null",
        "iadd",
        "isub",
        "imul",
        "idiv",
        "irem",
        "ineg",
        "ladd",
        "lsub",
        "lmul",
        "ldiv",
        "arraylength",
        "athrow",
        "ireturn",
        "lreturn",
        "areturn",
        "return_",
        "nop",
        "pop",
        "dup",
        "swap",
        "monitorenter",
        "monitorexit",
    ] {
        r.register(
            cb,
            name,
            "()Ljava/lang/classfile/CodeBuilder;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                cb_inc_insn(ctx, this);
                Ok(Some(Value::Object(Some(this))))
            },
        );
    }

    // bipush(int), sipush(int)
    r.register(
        cb,
        "bipush",
        "(I)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        cb,
        "sipush",
        "(I)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // ldc(ConstantDesc)
    r.register(
        cb,
        "ldc",
        "(Ljava/lang/constant/ConstantDesc;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // Invoke instructions — all take (ClassDesc, String, MethodTypeDesc)
    for name in &[
        "invokevirtual",
        "invokestatic",
        "invokespecial",
        "invokeinterface",
    ] {
        r.register(cb, name,
            "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;Ljava/lang/constant/MethodTypeDesc;)Ljava/lang/classfile/CodeBuilder;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                cb_inc_insn(ctx, this);
                Ok(Some(Value::Object(Some(this))))
            },
        );
    }

    // new_(ClassDesc)
    r.register(
        cb,
        "new_",
        "(Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // newarray(TypeKind)
    r.register(
        cb,
        "newarray",
        "(Ljava/lang/classfile/TypeKind;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // anewarray(ClassDesc)
    r.register(
        cb,
        "anewarray",
        "(Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // checkcast(ClassDesc), instanceof_(ClassDesc)
    r.register(
        cb,
        "checkcast",
        "(Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        cb,
        "instanceof_",
        "(Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // Field access: getfield, putfield, getstatic, putstatic (ClassDesc, String, ClassDesc)
    for name in &["getfield", "putfield", "getstatic", "putstatic"] {
        r.register(cb, name,
            "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/CodeBuilder;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                cb_inc_insn(ctx, this);
                Ok(Some(Value::Object(Some(this))))
            },
        );
    }

    // Branch instructions taking a Label
    for name in &["ifeq", "ifne", "if_icmpeq", "if_icmpne", "goto_"] {
        r.register(
            cb,
            name,
            "(Ljava/lang/classfile/Label;)Ljava/lang/classfile/CodeBuilder;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                cb_inc_insn(ctx, this);
                Ok(Some(Value::Object(Some(this))))
            },
        );
    }

    // newLabel() -> Label
    r.register(
        cb,
        "newLabel",
        "()Ljava/lang/classfile/Label;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let label_count = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, 1, Value::Int(label_count + 1));
            let label = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/Label", 1)?;
            ctx.set_field(label, 0, Value::Int(label_count));
            Ok(Some(Value::Object(Some(label))))
        },
    );

    // labelBinding(Label) -> CodeBuilder
    r.register(
        cb,
        "labelBinding",
        "(Ljava/lang/classfile/Label;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            cb_inc_insn(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // block(Consumer<CodeBuilder>) -> CodeBuilder
    r.register(
        cb,
        "block",
        "(Ljava/util/function/Consumer;)Ljava/lang/classfile/CodeBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let depth = match ctx.get_field(this, 2) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Int(depth + 1));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // lineNumber(int) -> CodeBuilder
    r.register(
        cb,
        "lineNumber",
        "(I)Ljava/lang/classfile/CodeBuilder;",
        |_ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // localVariable(int, String, ClassDesc, Label, Label) -> CodeBuilder
    r.register(cb, "localVariable",
        "(ILjava/lang/String;Ljava/lang/constant/ClassDesc;Ljava/lang/classfile/Label;Ljava/lang/classfile/Label;)Ljava/lang/classfile/CodeBuilder;",
        |_ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(this))))
        },
    );
}

// ---------------------------------------------------------------------------
// ClassTransform — 1-field synthetic: [0]=transform_type
// ---------------------------------------------------------------------------

fn register_class_transform(r: &mut NativeMethodRegistry) {
    let ct = "java/lang/classfile/ClassTransform";

    // ofStateful(Supplier) -> ClassTransform
    r.register(
        ct,
        "ofStateful",
        "(Ljava/util/function/Supplier;)Ljava/lang/classfile/ClassTransform;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/ClassTransform", 1)?;
            ctx.set_field(obj, 0, Value::Int(2)); // mapping
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // dropping(Predicate) -> ClassTransform
    r.register(
        ct,
        "dropping",
        "(Ljava/util/function/Predicate;)Ljava/lang/classfile/ClassTransform;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/ClassTransform", 1)?;
            ctx.set_field(obj, 0, Value::Int(1)); // dropping
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ACCEPT_ALL (static field sim) -> ClassTransform
    r.register(
        ct,
        "ACCEPT_ALL",
        "()Ljava/lang/classfile/ClassTransform;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/ClassTransform", 1)?;
            ctx.set_field(obj, 0, Value::Int(0)); // identity
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // transformClass(ClassModel) -> byte[]
    // An empty byte[] is an invalid class file; fail loudly instead.
    r.register(
        ct,
        "transformClass",
        "(Ljava/lang/classfile/ClassModel;)[B",
        |_ctx, _args| classfile_unsupported("ClassTransform.transformClass"),
    );
}

// ---------------------------------------------------------------------------
// CodeTransform — 1-field synthetic
// ---------------------------------------------------------------------------

fn register_code_transform(r: &mut NativeMethodRegistry) {
    let ct = "java/lang/classfile/CodeTransform";

    r.register(
        ct,
        "ofStateful",
        "(Ljava/util/function/Supplier;)Ljava/lang/classfile/CodeTransform;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/CodeTransform", 1)?;
            ctx.set_field(obj, 0, Value::Int(2));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        ct,
        "andThen",
        "(Ljava/lang/classfile/CodeTransform;)Ljava/lang/classfile/CodeTransform;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/CodeTransform", 1)?;
            ctx.set_field(obj, 0, Value::Int(2));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        ct,
        "ACCEPT_ALL",
        "()Ljava/lang/classfile/CodeTransform;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/CodeTransform", 1)?;
            ctx.set_field(obj, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
}

// ---------------------------------------------------------------------------
// Attribute — 2-field synthetic: [0]=attribute_name_idx, [1]=attribute_length
// ---------------------------------------------------------------------------

fn register_attribute(r: &mut NativeMethodRegistry) {
    let attr = "java/lang/classfile/Attribute";

    r.register(
        attr,
        "attributeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let _idx = ctx.get_field(this, 0);
            let s = ctx.create_string("UnknownAttribute");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    r.register(
        attr,
        "attributeMapper",
        "()Ljava/lang/classfile/AttributeMapper;",
        |ctx, _args| {
            let mapper =
                try_alloc_concurrent_synthetic(ctx, "java/lang/classfile/AttributeMapper", 1)?;
            ctx.set_field(mapper, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(mapper))))
        },
    );
}

// ---------------------------------------------------------------------------
// ConstantPool — 2-field synthetic: [0]=entry_count, [1]=bootstrap_method_count
// ---------------------------------------------------------------------------

fn register_constant_pool(r: &mut NativeMethodRegistry) {
    let cp = "java/lang/classfile/constantpool/ConstantPool";

    r.register(cp, "entryCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    r.register(cp, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    r.register(cp, "bootstrapMethodCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
}

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

pub(crate) fn register_classfile_api_natives(r: &mut NativeMethodRegistry) {
    // FLAGGED SyntheticStub: every JEP 484 Class-File API native registered
    // below returns a STUB ClassFile / ClassModel / *Builder object rather
    // than performing real class-file parsing or generation (e.g. `parse`
    // fabricates a fixed ClassModel, `build`/`buildTo`/`transformClass`
    // return empty byte[]). These are placeholders, so they stay tagged
    // SyntheticStub.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    register_classfile(r);
    register_class_model(r);
    register_method_model(r);
    register_field_model(r);
    register_code_model(r);
    register_class_builder(r);
    register_code_builder(r);
    register_class_transform(r);
    register_code_transform(r);
    register_attribute(r);
    register_constant_pool(r);
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod classfile_api_tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn make_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_classfile_api_natives(&mut r);
        r
    }

    // --- Version constants ---

    #[test]
    fn test_version_constants() {
        assert_eq!(CLASSFILE_MAJOR_45, 45);
        assert_eq!(CLASSFILE_MAJOR_52, 52);
        assert_eq!(CLASSFILE_MAJOR_55, 55);
        assert_eq!(CLASSFILE_MAJOR_61, 61);
        assert_eq!(CLASSFILE_MAJOR_65, 65);
        assert_eq!(CLASSFILE_MAJOR_68, 68);
        assert_eq!(CLASSFILE_MAJOR_69, 69);
    }

    #[test]
    fn test_access_flag_constants() {
        assert_eq!(ACC_PUBLIC, 0x0001);
        assert_eq!(ACC_PRIVATE, 0x0002);
        assert_eq!(ACC_PROTECTED, 0x0004);
        assert_eq!(ACC_STATIC, 0x0008);
        assert_eq!(ACC_FINAL, 0x0010);
        assert_eq!(ACC_SUPER, ACC_SYNCHRONIZED); // both 0x0020
        assert_eq!(ACC_VOLATILE, ACC_BRIDGE); // both 0x0040
        assert_eq!(ACC_TRANSIENT, ACC_VARARGS); // both 0x0080
        assert_eq!(ACC_NATIVE, 0x0100);
        assert_eq!(ACC_INTERFACE, 0x0200);
        assert_eq!(ACC_ABSTRACT, 0x0400);
        assert_eq!(ACC_STRICT, 0x0800);
        assert_eq!(ACC_SYNTHETIC, 0x1000);
        assert_eq!(ACC_ANNOTATION, 0x2000);
        assert_eq!(ACC_ENUM, 0x4000);
        assert_eq!(ACC_MODULE, 0x8000);
    }

    #[test]
    fn test_type_kind_constants() {
        assert_eq!(TK_BYTE, 0);
        assert_eq!(TK_SHORT, 1);
        assert_eq!(TK_INT, 2);
        assert_eq!(TK_LONG, 3);
        assert_eq!(TK_FLOAT, 4);
        assert_eq!(TK_DOUBLE, 5);
        assert_eq!(TK_CHAR, 6);
        assert_eq!(TK_BOOLEAN, 7);
        assert_eq!(TK_REFERENCE, 8);
        assert_eq!(TK_VOID, 9);
    }

    // --- ClassFile registration ---

    #[test]
    fn test_classfile_of_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassFile",
                "of",
                "()Ljava/lang/classfile/ClassFile;"
            )
            .is_some());
    }

    #[test]
    fn test_classfile_of_options_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassFile",
                "of",
                "([Ljava/lang/classfile/ClassFile$Option;)Ljava/lang/classfile/ClassFile;"
            )
            .is_some());
    }

    #[test]
    fn test_classfile_parse_bytes_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassFile",
                "parse",
                "([B)Ljava/lang/classfile/ClassModel;"
            )
            .is_some());
    }

    #[test]
    fn test_classfile_parse_path_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassFile",
                "parse",
                "(Ljava/nio/file/Path;)Ljava/lang/classfile/ClassModel;"
            )
            .is_some());
    }

    #[test]
    fn test_classfile_build_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassFile",
                "build",
                "(Ljava/lang/constant/ClassDesc;Ljava/util/function/Consumer;)[B"
            )
            .is_some());
    }

    #[test]
    fn test_classfile_latest_major_version_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/ClassFile", "latestMajorVersion", "()I")
            .is_some());
    }

    #[test]
    fn test_classfile_latest_minor_version_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/ClassFile", "latestMinorVersion", "()I")
            .is_some());
    }

    #[test]
    fn test_classfile_transform_class_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassFile",
                "transformClass",
                "(Ljava/lang/classfile/ClassModel;Ljava/lang/classfile/ClassTransform;)[B"
            )
            .is_some());
    }

    // --- ClassModel registration ---

    #[test]
    fn test_class_model_major_version_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/ClassModel", "majorVersion", "()I")
            .is_some());
    }

    #[test]
    fn test_class_model_minor_version_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/ClassModel", "minorVersion", "()I")
            .is_some());
    }

    #[test]
    fn test_class_model_flags_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassModel",
                "flags",
                "()Ljava/lang/classfile/AccessFlags;"
            )
            .is_some());
    }

    #[test]
    fn test_class_model_this_class_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassModel",
                "thisClass",
                "()Ljava/lang/classfile/constantpool/ClassEntry;"
            )
            .is_some());
    }

    #[test]
    fn test_class_model_superclass_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassModel",
                "superclass",
                "()Ljava/util/Optional;"
            )
            .is_some());
    }

    #[test]
    fn test_class_model_is_module_info_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/ClassModel", "isModuleInfo", "()Z")
            .is_some());
    }

    #[test]
    fn test_class_model_constant_pool_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassModel",
                "constantPool",
                "()Ljava/lang/classfile/constantpool/ConstantPool;"
            )
            .is_some());
    }

    // --- MethodModel registration ---

    #[test]
    fn test_method_model_flags_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/MethodModel",
                "flags",
                "()Ljava/lang/classfile/AccessFlags;"
            )
            .is_some());
    }

    #[test]
    fn test_method_model_method_name_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/MethodModel",
                "methodName",
                "()Ljava/lang/classfile/constantpool/Utf8Entry;"
            )
            .is_some());
    }

    #[test]
    fn test_method_model_code_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/MethodModel",
                "code",
                "()Ljava/util/Optional;"
            )
            .is_some());
    }

    // --- FieldModel registration ---

    #[test]
    fn test_field_model_flags_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/FieldModel",
                "flags",
                "()Ljava/lang/classfile/AccessFlags;"
            )
            .is_some());
    }

    #[test]
    fn test_field_model_field_name_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/FieldModel",
                "fieldName",
                "()Ljava/lang/classfile/constantpool/Utf8Entry;"
            )
            .is_some());
    }

    #[test]
    fn test_field_model_attributes_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/FieldModel",
                "attributes",
                "()Ljava/util/List;"
            )
            .is_some());
    }

    // --- CodeModel registration ---

    #[test]
    fn test_code_model_max_stack_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/CodeModel", "maxStack", "()I")
            .is_some());
    }

    #[test]
    fn test_code_model_max_locals_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/CodeModel", "maxLocals", "()I")
            .is_some());
    }

    #[test]
    fn test_code_model_code_length_registered() {
        let r = make_registry();
        assert!(r
            .find("java/lang/classfile/CodeModel", "codeLength", "()I")
            .is_some());
    }

    #[test]
    fn test_code_model_exception_handlers_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeModel",
                "exceptionHandlers",
                "()Ljava/util/List;"
            )
            .is_some());
    }

    // --- ClassBuilder registration ---

    #[test]
    fn test_class_builder_with_flags_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassBuilder",
                "withFlags",
                "(I)Ljava/lang/classfile/ClassBuilder;"
            )
            .is_some());
    }

    #[test]
    fn test_class_builder_with_field_consumer_registered() {
        let r = make_registry();
        assert!(r.find("java/lang/classfile/ClassBuilder", "withField",
            "(Ljava/lang/String;Ljava/lang/constant/ClassDesc;Ljava/util/function/Consumer;)Ljava/lang/classfile/ClassBuilder;").is_some());
    }

    #[test]
    fn test_class_builder_with_method_registered() {
        let r = make_registry();
        assert!(r.find("java/lang/classfile/ClassBuilder", "withMethod",
            "(Ljava/lang/String;Ljava/lang/constant/MethodTypeDesc;ILjava/util/function/Consumer;)Ljava/lang/classfile/ClassBuilder;").is_some());
    }

    // --- CodeBuilder registration ---

    #[test]
    fn test_code_builder_aload_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeBuilder",
                "aload",
                "(I)Ljava/lang/classfile/CodeBuilder;"
            )
            .is_some());
    }

    #[test]
    fn test_code_builder_iconst_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeBuilder",
                "iconst",
                "(I)Ljava/lang/classfile/CodeBuilder;"
            )
            .is_some());
    }

    #[test]
    fn test_code_builder_iadd_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeBuilder",
                "iadd",
                "()Ljava/lang/classfile/CodeBuilder;"
            )
            .is_some());
    }

    #[test]
    fn test_code_builder_invokevirtual_registered() {
        let r = make_registry();
        assert!(r.find("java/lang/classfile/CodeBuilder", "invokevirtual",
            "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;Ljava/lang/constant/MethodTypeDesc;)Ljava/lang/classfile/CodeBuilder;").is_some());
    }

    #[test]
    fn test_code_builder_new_label_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeBuilder",
                "newLabel",
                "()Ljava/lang/classfile/Label;"
            )
            .is_some());
    }

    #[test]
    fn test_code_builder_goto_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeBuilder",
                "goto_",
                "(Ljava/lang/classfile/Label;)Ljava/lang/classfile/CodeBuilder;"
            )
            .is_some());
    }

    #[test]
    fn test_code_builder_return_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeBuilder",
                "return_",
                "()Ljava/lang/classfile/CodeBuilder;"
            )
            .is_some());
    }

    #[test]
    fn test_code_builder_getfield_registered() {
        let r = make_registry();
        assert!(r.find("java/lang/classfile/CodeBuilder", "getfield",
            "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;Ljava/lang/constant/ClassDesc;)Ljava/lang/classfile/CodeBuilder;").is_some());
    }

    #[test]
    fn test_code_builder_block_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeBuilder",
                "block",
                "(Ljava/util/function/Consumer;)Ljava/lang/classfile/CodeBuilder;"
            )
            .is_some());
    }

    // --- ClassTransform registration ---

    #[test]
    fn test_class_transform_of_stateful_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassTransform",
                "ofStateful",
                "(Ljava/util/function/Supplier;)Ljava/lang/classfile/ClassTransform;"
            )
            .is_some());
    }

    #[test]
    fn test_class_transform_dropping_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassTransform",
                "dropping",
                "(Ljava/util/function/Predicate;)Ljava/lang/classfile/ClassTransform;"
            )
            .is_some());
    }

    #[test]
    fn test_class_transform_accept_all_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/ClassTransform",
                "ACCEPT_ALL",
                "()Ljava/lang/classfile/ClassTransform;"
            )
            .is_some());
    }

    // --- CodeTransform registration ---

    #[test]
    fn test_code_transform_of_stateful_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeTransform",
                "ofStateful",
                "(Ljava/util/function/Supplier;)Ljava/lang/classfile/CodeTransform;"
            )
            .is_some());
    }

    #[test]
    fn test_code_transform_and_then_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeTransform",
                "andThen",
                "(Ljava/lang/classfile/CodeTransform;)Ljava/lang/classfile/CodeTransform;"
            )
            .is_some());
    }

    #[test]
    fn test_code_transform_accept_all_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/CodeTransform",
                "ACCEPT_ALL",
                "()Ljava/lang/classfile/CodeTransform;"
            )
            .is_some());
    }

    // --- Attribute registration ---

    #[test]
    fn test_attribute_name_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/Attribute",
                "attributeName",
                "()Ljava/lang/String;"
            )
            .is_some());
    }

    #[test]
    fn test_attribute_mapper_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/Attribute",
                "attributeMapper",
                "()Ljava/lang/classfile/AttributeMapper;"
            )
            .is_some());
    }

    // --- ConstantPool registration ---

    #[test]
    fn test_constant_pool_entry_count_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/constantpool/ConstantPool",
                "entryCount",
                "()I"
            )
            .is_some());
    }

    #[test]
    fn test_constant_pool_size_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/constantpool/ConstantPool",
                "size",
                "()I"
            )
            .is_some());
    }

    #[test]
    fn test_constant_pool_bootstrap_method_count_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "java/lang/classfile/constantpool/ConstantPool",
                "bootstrapMethodCount",
                "()I"
            )
            .is_some());
    }
}
