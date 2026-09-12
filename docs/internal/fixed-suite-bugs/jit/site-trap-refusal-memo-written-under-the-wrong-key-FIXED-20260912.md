# FIXED: the site-trap policy wrote the IR refusal memo under a key no reader asks for

**Status: FIXED 2026-09-12.** Found while fixing the 2026-09-12 JIT review
finding #55 (verdicts keyed by class identity).

## The defect

When an IR site trap fires, `despeculate_trapped_method`
(`vm/src/jit/helpers.rs`) claims the one-time policy decision and marks the
method refused in `ir_evidence`'s refusal memo. The next compile should then
skip the IR attempt for the method that just trapped.

The writer passed the bare name hash:

```rust
let h = cratonvm_jit::ir_method_memo_hash(class_name, method_name, descriptor);
cratonvm_jit::ir_evidence::note_method_refused(h);
```

The only production reader, the compile door in `try_compile_inner`
(`jit/src/lib.rs`), asks under a different key: `ir_refusal_memo_key` of that
hash, the declaring class id and the redefine epoch. So the write was never
read back. After a site trap the method was offered to the IR pipeline again,
built, and could trap again.

## The fix

- **One writer for the key.** `cratonvm_jit::note_ir_method_refused(class,
  method, descriptor, declaring_class_id)` computes the compile door's key and
  writes it.
- **The class id reaches the writer.** `despeculate_trapped_method` takes the
  trapped method's `declaring_class_id`:
  - the callee-resume sink passes the id its resolution of the stashed method
    found (`cached.declaring_class_id`);
  - the interpreter's tier-up sink passes the executing method's class id.
- The site-trap decision set (`claim_site_trap_decision`) keeps the bare hash.
  It is its own set, read only by the same function.

## Regression coverage

`jit/src/lib.rs`: `a_site_trap_refusal_is_read_back_under_the_compile_doors_key`.
A refusal written through `note_ir_method_refused` is found under the compile
door's key, and not under the key of a same-named class with another id.
