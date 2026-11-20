    use super::*;
    use std::sync::Arc;

    const CALLER: u32 = 0x7a10;
    const CALLEE: u32 = 0x7a11;

    fn body(with_context: bool) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // RET
        if with_context {
            CompiledMethod::new_with_context(buf)
        } else {
            CompiledMethod::new(buf)
        }
    }

    /// An OSR body owning one late-bind cell for `t1j2/B.b()V`, published.
    /// `None` when this configuration refuses the publication.
    fn publish_osr_caller(cache: &JitCache) -> Option<Arc<JitCycleEdgeCell>> {
        let mut osr = body(true);
        osr.compiled_via_osr = true;
        let cell = JitCycleEdgeCell::new(
            "t1j2/B",
            "b",
            "()V",
            cratonvm_types::ClassId::new(CALLEE),
            true,
        );
        osr._jit_cycle_cells.push(Arc::clone(&cell));
        cache.put_osr(
            "t1j2/A".into(),
            "a".into(),
            "()V".into(),
            cratonvm_types::ClassId::new(CALLER),
            osr,
        );
        cache
            .get_osr("t1j2/A", "a", "()V", cratonvm_types::ClassId::new(CALLER))
            .map(|_| cell)
    }

    fn publish_callee(cache: &JitCache, with_context: bool) {
        cache.put(
            "t1j2/B".into(),
            "b".into(),
            "()V".into(),
            cratonvm_types::ClassId::new(CALLEE),
            body(with_context),
        );
    }

    fn callee_entry(cache: &JitCache) -> Option<u64> {
        cache
            .get("t1j2/B", "b", "()V", cratonvm_types::ClassId::new(CALLEE))
            .map(|b| b.entry_ptr() as u64)
    }

    /// The callee publishes AFTER the OSR body: the publication binds the
    /// site, and the callee's eviction sends it back to dispatch.
    #[test]
    fn put_osr_registers_a_late_cell_that_the_callee_publication_fills() {
        let cache = JitCache::new();
        let Some(cell) = publish_osr_caller(&cache) else {
            return;
        };
        assert_eq!(cell.current_entry(), 0, "no callee body yet: the site dispatches");
        publish_callee(&cache, true);
        let Some(b) = callee_entry(&cache) else {
            return;
        };
        assert_eq!(cell.current_entry(), b, "the callee's publication binds the OSR site");
        cache.remove("t1j2/B", "b", "()V", cratonvm_types::ClassId::new(CALLEE));
        assert_eq!(cell.current_entry(), 0, "the eviction sends the site back to dispatch");
        cache.clear_all();
    }

    /// The callee was published before the OSR body: `put_osr` fills at once.
    #[test]
    fn put_osr_fills_a_late_cell_whose_callee_is_already_published() {
        let cache = JitCache::new();
        publish_callee(&cache, true);
        let Some(b) = callee_entry(&cache) else {
            return;
        };
        let Some(cell) = publish_osr_caller(&cache) else {
            return;
        };
        assert_eq!(cell.current_entry(), b);
        cache.clear_all();
        assert_eq!(cell.current_entry(), 0, "clear_all clears the OSR body's cell too");
    }

    /// An unpublished body's cells (the VM's optimizing OSR bodies): the
    /// explicit registration fills them, a later callee publication refills
    /// them, and `clear_all` clears them.
    #[test]
    fn an_unpublished_body_late_cell_binds_after_registration() {
        let cache = JitCache::new();
        let mut osr = body(true);
        osr.compiled_via_osr = true;
        let cell = JitCycleEdgeCell::new(
            "t1j2/B",
            "b",
            "()V",
            cratonvm_types::ClassId::new(CALLEE),
            true,
        );
        osr._jit_cycle_cells.push(Arc::clone(&cell));
        let osr = Arc::new(osr);
        publish_callee(&cache, true);
        assert_eq!(cell.current_entry(), 0, "not registered: nothing fills it");
        cache.register_unpublished_body_late_cells(&osr);
        let Some(b1) = callee_entry(&cache) else {
            return;
        };
        assert_eq!(cell.current_entry(), b1);
        publish_callee(&cache, true);
        let Some(b2) = callee_entry(&cache) else {
            return;
        };
        assert_eq!(cell.current_entry(), b2, "a key-waiting cell follows the newest body");
        cache.clear_all();
        assert_eq!(cell.current_entry(), 0);
        drop(osr);
    }

    /// A callee body of the other entry ABI is never installed (the cell
    /// stages the VM context): the site keeps dispatching.
    #[test]
    fn a_late_osr_cell_refuses_a_callee_of_another_abi() {
        let cache = JitCache::new();
        let Some(cell) = publish_osr_caller(&cache) else {
            return;
        };
        publish_callee(&cache, false);
        assert_eq!(cell.current_entry(), 0);
        cache.clear_all();
    }

    /// The switch is default ON since j27 2026-10-28 (`=0` kills) and implies
    /// the late cell's own switches.
/// t1 wave 3, lane X: the IR-OSR registration is default ON, killed by
    /// `CRATONVM_JIT_IR_OSR_CELL_REGISTER=0`, and implied by the opt-in switch.
/// t1 wave 3, lane X: an IR-OSR body's cell (unpublished body) never reads
    /// a stale target. Unregistered, nothing fills it, so a callee published
    /// and then evicted is never seen; registered, it binds the live callee,
    /// returns to dispatch on the callee's eviction, rebinds on a republication,
    /// and is cleared by `clear_all`.
    #[test]
    fn t1x_an_ir_osr_cell_follows_its_callee_only_while_registered() {
        let cache = JitCache::new();
        let mut osr = body(true);
        osr.compiled_via_osr = true;
        let cell = JitCycleEdgeCell::new(
            "t1j2/B",
            "b",
            "()V",
            cratonvm_types::ClassId::new(CALLEE),
            true,
        );
        osr._jit_cycle_cells.push(Arc::clone(&cell));
        let osr = Arc::new(osr);
        publish_callee(&cache, true);
        cache.remove("t1j2/B", "b", "()V", cratonvm_types::ClassId::new(CALLEE));
        assert_eq!(cell.current_entry(), 0, "unregistered: never filled");
        publish_callee(&cache, true);
        cache.register_unpublished_body_late_cells(&osr);
        let Some(b1) = callee_entry(&cache) else {
            return;
        };
        assert_eq!(cell.current_entry(), b1, "registration binds the live callee");
        cache.remove("t1j2/B", "b", "()V", cratonvm_types::ClassId::new(CALLEE));
        assert_eq!(cell.current_entry(), 0, "the eviction sends the site back to dispatch");
        publish_callee(&cache, true);
        let Some(b2) = callee_entry(&cache) else {
            return;
        };
        assert_eq!(cell.current_entry(), b2, "the republication rebinds it");
        cache.clear_all();
        assert_eq!(cell.current_entry(), 0);
        drop(osr);
    }
