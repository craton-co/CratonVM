# HIB-CV-22 — JDK dynamic-proxy primitive args boxed by VM tag, not descriptor → `boolean` arg becomes `Integer` → `Method.invoke` `cannot convert Integer to Z`

**Severity:** High — **17 CV-only failing classes** (the 2nd-largest CV-only cluster). Any test that drives JDBC through Hibernate's `JdbcSpies` dynamic proxy (a `Connection`/`Statement`/… proxy) where a `boolean` (or `char`/`byte`/`short`) argument is forwarded.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `vm/src/vm/vm_exec.rs`).
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

17 classes fail (HotSpot passes all) with:

```
java.lang.IllegalArgumentException: Method.invoke argument: cannot convert java/lang/Integer to Z
```

(plus a few wrapped as `Could not build SessionFactory: Method.invoke argument: cannot convert java/lang/Integer to Z`). Examples: the `insertordering.*` family (8), the `timestamp.Jdbc*TimeZoneTest` family (5), `SessionJdbcBatchTest`, `QueryTimeOutTest`, `StoreProcedureStatementsClosedTest`, `SchemaBasedDataSourceMultiTenancyTest`.

## Root cause

Caller chain captured via a new `CRATONVM_DBG_INVOKE_COERCE=1` diagnostic:

```
[DBG_INVOKE_COERCE] java/sql/Connection.setAutoCommit param[0]=Z params=["Z"] argTypes=["java/lang/Integer"]
  …
  org/hibernate/testing/jdbc/JdbcSpies$ConnectionHandler.invoke
  org/hibernate/testing/jdbc/JdbcSpies$SpyContext.call
```

`JdbcSpies` wraps the JDBC `Connection` in a `java.lang.reflect.Proxy`. When Hibernate calls `connection.setAutoCommit(false)`, CratonVM's proxy dispatch (`proxy_invoke_handler_shared` / `proxy_invoke_handler` in `vm/src/vm/vm_exec.rs`) builds the `Object[] args` for `InvocationHandler.invoke` by boxing each primitive argument with `proxy_box_value`. That helper boxes purely on the VM value tag: `boolean`, `char`, `byte`, `short`, and `int` all share the `Value::Int` representation, so it boxed the `false` argument as **`java.lang.Integer`**.

The handler then re-dispatches via `method.invoke(realConnection, args)`. `Connection.setAutoCommit` declares a `boolean` (`Z`) parameter, so `coerce_arg_strict` (correctly, per the `Method.invoke` contract) refused to convert an `Integer` to `Z` → `IllegalArgumentException`.

On HotSpot the proxy glue boxes per the method's declared parameter type, so `false` becomes `Boolean.FALSE` and the re-invoke unboxes cleanly. CratonVM's descriptor-blind boxing was the divergence.

## Fix

Add `proxy_box_value_for_desc(shared, value, pdesc)`: for a `Value::Int` it selects the wrapper from the formal parameter descriptor — `Z`→`Boolean`, `C`→`Character`, `B`→`Byte`, `S`→`Short`, `I`→`Integer` (Long/Float/Double and references are descriptor-independent, delegating to `proxy_box_value`). Both proxy arg-boxing loops (`proxy_invoke_handler_shared` and the older `proxy_invoke_handler`) now box per `param_descs[i]`, which they already parse from the method descriptor for the synthesized `Method` object.

A `CRATONVM_DBG_INVOKE_COERCE=1` diagnostic (dumps the target method, formal descriptors, actual arg runtime types, and innermost Java caller frames on a `Method.invoke` coercion mismatch) was added to `native_method_invoke`; kept as a tool.

## Verification

5 representative classes that previously failed, all now green vs HotSpot:
`JdbcTimestampDefaultTimeZoneTest` 1/1, `InsertOrderingDuplicateTest` 1/1,
`SessionJdbcBatchTest` 2/2, `QueryTimeOutTest` 6/6,
`StoreProcedureStatementsClosedTest` 1/1 — **0** `cannot convert … to Z`. Expected to clear all 17 classes (single shared root cause).
