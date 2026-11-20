use super::*;

impl DirectHelperTable {
    /// nio21 wave 19, lane L3: the `ThreadLocal.get()` expansion's layout from
    /// [`Self::thread_local_ir_layout`], or `None` (not wired, no VM, or the
    /// VM declined). Asked once per compile that reaches such a site.
    pub fn thread_local_ir_layout_words(&self) -> Option<[u64; THREAD_LOCAL_IR_LAYOUT_WORDS]> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: the VM fills this field from `jit_thread_local_ir_layout`, an
        // `extern "C" fn(i64, i64, i64) -> i64` with process lifetime; a test
        // fills it from a function of the same signature.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; THREAD_LOCAL_IR_LAYOUT_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`, alive for every
        // compilation it runs; `out` is `THREAD_LOCAL_IR_LAYOUT_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                THREAD_LOCAL_IR_LAYOUT_WORDS as i64,
            )
        };
        (got == THREAD_LOCAL_IR_LAYOUT_WORDS as i64).then_some(out)
    }

    /// Query the VM for `ByteBuffer.wrap` / `Charset.decode` IR intrinsic layout
    /// words, or `None` if not wired or classes are not yet initialized.
    pub fn nio_charset_ir_layout_words(&self) -> Option<[u64; NIO_CHARSET_IR_LAYOUT_QUERY_WORDS]> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; NIO_CHARSET_IR_LAYOUT_QUERY_WORDS];
        // SAFETY: `ctx` is the `SharedVm`, `out` has `NIO_CHARSET_IR_LAYOUT_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                NIO_CHARSET_IR_LAYOUT_QUERY_WORDS as i64,
            )
        };
        (got == NIO_CHARSET_IR_LAYOUT_QUERY_WORDS as i64).then_some(out)
    }

    /// JIT round 21 (lane refs2): `Reference.referent`'s field index from the
    /// same producer, asked with [`REFERENCE_REFERENT_QUERY_WORDS`], or `None`
    /// (not wired, no VM, or the VM declined: not `--real-jdk`). Unlike
    /// [`Self::thread_local_ir_layout_words`] it does not wait for the
    /// `ThreadLocal` classes to be loaded.
    pub fn reference_referent_index(&self) -> Option<u32> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENCE_REFERENT_QUERY_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENCE_REFERENT_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENCE_REFERENT_QUERY_WORDS as i64,
            )
        };
        if got != REFERENCE_REFERENT_QUERY_WORDS as i64 {
            return None;
        }
        u32::try_from(out[0]).ok()
    }

    /// JIT round 21 (lane tlsub, TL321-4): which `Thread` map
    /// `ThreadLocal.get` reads for a receiver of class `class_id`, and whether
    /// that class also leaves `set(Object)` to `ThreadLocal`, from the same
    /// producer asked with [`THREAD_LOCAL_RECEIVER_QUERY_WORDS`]; `None` (not
    /// wired, no VM, the VM declined: not `--real-jdk`, the classes not loaded,
    /// an unknown id). Compile time only.
    pub fn thread_local_receiver_kind(&self, class_id: u32) -> Option<(u8, bool)> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 || class_id == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut io = [u64::from(class_id), 0u64];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `io` is
        // `THREAD_LOCAL_RECEIVER_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                io.as_mut_ptr() as i64,
                THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64,
            )
        };
        if got != THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64 {
            return None;
        }
        Some((u8::try_from(io[0]).ok()?, io[1] == 1))
    }

    /// TLGET21-7: is the expansion's layout pending -- served by this VM, but
    /// not yet resolvable because a class it bakes (`ThreadLocalMap`, its
    /// `Entry`) has not been loaded? Asked with
    /// [`THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS`]; `false` when not wired, no
    /// VM, the VM declined, or the layout is there.
    pub fn thread_local_layout_pending(&self) -> bool {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return false;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS as i64,
            )
        };
        got == 1 && out[0] == 1    }

    /// JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`):
    /// the `WeakReference.get()` / `SoftReference.get()` expansion's words
    /// from the same producer, asked with [`REFERENCE_GET_LAYOUT_QUERY_WORDS`]
    /// (see `crate::ir::ReferenceGetIrLayout::from_words` for the meaning),
    /// or `None` (not wired, no VM, or the VM declined). Compile time only.
    pub fn reference_get_layout_words(&self) -> Option<[u64; REFERENCE_GET_LAYOUT_QUERY_WORDS]> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENCE_GET_LAYOUT_QUERY_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENCE_GET_LAYOUT_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENCE_GET_LAYOUT_QUERY_WORDS as i64,
            )
        };
        (got == REFERENCE_GET_LAYOUT_QUERY_WORDS as i64).then_some(out)    }

    /// t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`):
    /// `(index, 1 + compact displacement)` of `Reference.referent` from the
    /// same producer, asked with [`REFERENCE_REFERENT_ROW_QUERY_WORDS`]; the
    /// second word is `0` when the VM names no row. `None` when not wired,
    /// without a VM, or where the VM declines.
    pub fn reference_referent_row(&self) -> Option<(u32, u32)> {
        let out = self.reference_referent_row_words()?;
        Some((u32::try_from(out[0]).ok()?, u32::try_from(out[1]).ok()?))
    }

    /// j27 wave 5 (lane REFS, `CRATONVM_JIT_IR_REFERS_TO_ZGC_ROW`): the
    /// referent-row answer's COMPARE-ONLY row (word 3,
    /// [`REFERENCE_REFERENT_ROW_ANSWER_WORDS`]): `1 + compact displacement`
    /// for a read that owes no keep-alive, `0` when the VM names none. `None`
    /// where [`Self::reference_referent_row`] is.
    pub fn reference_referent_compare_row(&self) -> Option<u32> {
        let out = self.reference_referent_row_words()?;
        u32::try_from(out[3]).ok()
    }

    /// The [`REFERENCE_REFERENT_ROW_QUERY_WORDS`] answer, taken only when the
    /// producer fills exactly [`REFERENCE_REFERENT_ROW_ANSWER_WORDS`].
    fn reference_referent_row_words(&self) -> Option<[u64; REFERENCE_REFERENT_ROW_QUERY_WORDS]> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENCE_REFERENT_ROW_QUERY_WORDS];
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENCE_REFERENT_ROW_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENCE_REFERENT_ROW_QUERY_WORDS as i64,
            )
        };
        (got == REFERENCE_REFERENT_ROW_ANSWER_WORDS as i64).then_some(out)    }

    /// j27 wave 5 (lane REFS): may this VM's heap ever arm the ZGC referent
    /// read gate (`VmHeap::zgc_referent_gate_may_arm`), asked with
    /// [`REFERENT_GATE_MAY_ARM_QUERY_WORDS`]? Then every compiled referent
    /// read owes the checked helper, which runs the gate. `false` when not
    /// wired, without a VM, or where the VM declines (the pre-j27 answer).
    /// Compile time only.
    pub fn referent_gate_may_arm(&self) -> bool {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return false;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENT_GATE_MAY_ARM_QUERY_WORDS];
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENT_GATE_MAY_ARM_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENT_GATE_MAY_ARM_QUERY_WORDS as i64,
            )
        };
        got == 1 && out[0] == 1    }}
