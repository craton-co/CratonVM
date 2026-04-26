//! Legacy per-method Thread state overrides.
//!
//! These used to be registered from `register_essential_natives` to paper
//! over VM-created Thread objects whose `holder:FieldHolder` field was
//! null, causing every `Thread.getState() / getPriority() / isDaemon()`
//! call to NPE in real-JDK mode.
//!
//! B1 fixed the underlying issue: `NativeContextImpl::current_thread_object`
//! now populates `holder` with a real `java.lang.Thread$FieldHolder`
//! instance whose `group/priority/daemon` fields are initialized, so
//! the real-JDK bytecode path works without overriding individual
//! methods.  The registration call has been disabled in
//! `lib::register_essential_natives`; the function body below is
//! preserved so a future regression that re-nulls `holder` can
//! re-enable it as an emergency patch.
//!
//! C42: Register a `TableFilter.prepare()V` override that ensures
//! `this.index` is non-null before the original bytecode dereferences
//! it. Our `Optimizer.optimize` pipeline leaves `index` unset for simple
//! single-table WHERE clauses (the plan lookup returns null), which in
//! turn trips `TableFilter.prepare pc=52` with a NullPointerException.
//! The override bootstraps `index` via `table.getScanIndex(session)`
//! when needed and reimplements the rest of the method's logic so
//! downstream execution proceeds normally (the scan index correctly
//! reports `getColumnIndex(col) = -1`, causing the offending
//! index-conditions to be pruned — which matches real-JDK behaviour).

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::Value;

/// Register Thread.threadState / Thread.getState overrides so the real-JDK
/// bytecode path does not dereference the null `holder` FieldHolder.
///
/// In JDK 21+, `Thread.getState()` delegates to `Thread.threadState()`
/// which reads `this.holder.threadStatus` and calls `VM.toThreadState(int)`.
/// We short-circuit both to return a RUNNABLE `Thread$State` enum
/// constant — the correct answer for the current thread.
#[allow(dead_code)]
pub(crate) fn register_apps_h2_overrides(registry: &mut NativeMethodRegistry) {
    let thread_state_runnable = |ctx: &mut dyn NativeContext, _args: &[Value]| {
        let class_name = "java/lang/Thread$State";
        let obj = match ctx.ensure_class_initialized(class_name) {
            Ok(cid) => {
                let real = ctx.class_num_total_fields(cid);
                let n = real.max(2);
                ctx.alloc_object(cid, n)
            }
            Err(_) => ctx.alloc_object(rustjvm_types::ClassId::new(0), 2),
        };
        let name = ctx.create_string("RUNNABLE");
        ctx.set_field(obj, 0, Value::Object(Some(name)));
        ctx.set_field(obj, 1, Value::Int(1));
        Ok(Some(Value::Object(Some(obj))))
    };

    registry.register(
        "java/lang/Thread",
        "getState",
        "()Ljava/lang/Thread$State;",
        thread_state_runnable,
    );
    registry.register(
        "java/lang/Thread",
        "threadState",
        "()Ljava/lang/Thread$State;",
        thread_state_runnable,
    );

    // Thread.getPriority() reads `this.holder.priority`.  Return NORM_PRIORITY.
    registry.register(
        "java/lang/Thread",
        "getPriority",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(5))),
    );

    // Thread.isDaemon() reads `this.holder.daemon`.
    registry.register(
        "java/lang/Thread",
        "isDaemon",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );

    registry.register(
        "java/lang/Thread",
        "setDaemon",
        "(Z)V",
        |_ctx, _args| Ok(None),
    );
}

/// C42: Register a corrective native for `org.h2.table.TableFilter.prepare()V`.
///
/// The real bytecode at pc=44..52 dereferences `this.index.getColumnIndex(col)`
/// to decide whether an index-condition should be pruned. In our VM the
/// plan-optimisation pipeline sometimes leaves `index` null for single-table
/// queries with a WHERE clause, so the original implementation NPE's. We
/// reproduce the method's logic, but lazily bootstrap `index` with the
/// table's scan index first — which matches what real-JDK's Optimizer
/// would have done via `setPlanItem`.
pub fn register_h2_table_filter_prepare(registry: &mut NativeMethodRegistry) {
    registry.register(
        "org/h2/table/TableFilter",
        "prepare",
        "()V",
        table_filter_prepare,
    );
}

fn table_filter_prepare(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    table_filter_prepare_on(ctx, this)?;
    Ok(None)
}

fn table_filter_prepare_on(
    ctx: &mut dyn NativeContext,
    this: rustjvm_types::ObjectRef,
) -> rustjvm_types::error::MethodCallResult {
    // Ensure `this.index` is non-null. If the optimiser never populated
    // the plan item for this filter, the index stays at null and the
    // rest of the method trips an NPE.
    let index_val = ctx.get_field_by_name(this, "index");
    if matches!(index_val, Value::Object(None)) {
        let session = ctx.get_field_by_name(this, "session");
        let table_val = ctx.get_field_by_name(this, "table");
        if let Value::Object(Some(table)) = table_val {
            let scan = ctx.invoke_virtual(
                table,
                "getScanIndex",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/index/Index;",
                &[session],
            )?;
            if let Some(Value::Object(Some(_))) = scan {
                // Propagate via the Java setter so masks/other side-effects
                // (if any in subclasses) get the normal treatment.
                ctx.invoke_virtual(
                    this,
                    "setIndex",
                    "(Lorg/h2/index/Index;)V",
                    &[scan.unwrap()],
                )?;
            }
        }
    }

    // Re-read index in case the Java setter wrote it. If it's still null
    // we skip the pruning loop; downstream will likely still fail but at
    // least we won't NPE here.
    let index_opt = match ctx.get_field_by_name(this, "index") {
        Value::Object(Some(idx)) => Some(idx),
        _ => None,
    };

    // Walk indexConditions and drop conditions whose column is not
    // covered by the current index (mirrors the original bytecode).
    let conds_val = ctx.get_field_by_name(this, "indexConditions");
    if let Value::Object(Some(conds)) = conds_val {
        let mut i: i32 = 0;
        loop {
            let size_res = ctx.invoke_virtual(conds, "size", "()I", &[])?;
            let size = match size_res {
                Some(Value::Int(n)) => n,
                _ => break,
            };
            if i >= size {
                break;
            }
            let cond_val = ctx.invoke_virtual(
                conds,
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Int(i)],
            )?;
            let cond = match cond_val {
                Some(Value::Object(Some(o))) => o,
                _ => {
                    i += 1;
                    continue;
                }
            };
            let always_false =
                ctx.invoke_virtual(cond, "isAlwaysFalse", "()Z", &[])?;
            if !matches!(always_false, Some(Value::Int(0))) {
                i += 1;
                continue;
            }
            let col_val = ctx.invoke_virtual(
                cond,
                "getColumn",
                "()Lorg/h2/table/Column;",
                &[],
            )?;
            let col = match col_val {
                Some(Value::Object(Some(o))) => o,
                _ => {
                    i += 1;
                    continue;
                }
            };
            let col_id = ctx.invoke_virtual(col, "getColumnId", "()I", &[])?;
            let col_id_i = match col_id {
                Some(Value::Int(n)) => n,
                _ => {
                    i += 1;
                    continue;
                }
            };
            if col_id_i < 0 {
                i += 1;
                continue;
            }
            let mut should_remove = false;
            if let Some(index) = index_opt {
                let idx_col = ctx.invoke_virtual(
                    index,
                    "getColumnIndex",
                    "(Lorg/h2/table/Column;)I",
                    &[Value::Object(Some(col))],
                )?;
                if let Some(Value::Int(n)) = idx_col {
                    if n < 0 {
                        should_remove = true;
                    }
                }
            }
            if should_remove {
                ctx.invoke_virtual(
                    conds,
                    "remove",
                    "(I)Ljava/lang/Object;",
                    &[Value::Int(i)],
                )?;
                // stay at same index (size shrank)
                continue;
            }
            i += 1;
        }
    }

    // Recurse into nestedJoin / join (with the same self-join guard as
    // the original) by reinvoking the native path.
    if let Value::Object(Some(nested)) = ctx.get_field_by_name(this, "nestedJoin") {
        if nested != this {
            table_filter_prepare_on(ctx, nested)?;
        }
    }
    if let Value::Object(Some(join)) = ctx.get_field_by_name(this, "join") {
        if join != this {
            table_filter_prepare_on(ctx, join)?;
        }
    }

    // Optimise filter/join conditions (the remaining tail of the
    // original bytecode).
    let session = ctx.get_field_by_name(this, "session");
    if let Value::Object(Some(cond)) = ctx.get_field_by_name(this, "filterCondition") {
        let opt = ctx.invoke_virtual(
            cond,
            "optimizeCondition",
            "(Lorg/h2/engine/SessionLocal;)Lorg/h2/expression/Expression;",
            &[session],
        )?;
        if let Some(v) = opt {
            ctx.set_field_by_name(this, "filterCondition", v);
        }
    }
    if let Value::Object(Some(cond)) = ctx.get_field_by_name(this, "joinCondition") {
        let opt = ctx.invoke_virtual(
            cond,
            "optimizeCondition",
            "(Lorg/h2/engine/SessionLocal;)Lorg/h2/expression/Expression;",
            &[session],
        )?;
        if let Some(v) = opt {
            ctx.set_field_by_name(this, "joinCondition", v);
        }
    }

    Ok(None)
}
