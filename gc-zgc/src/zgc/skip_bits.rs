pub use cratonvm_gc_core::skip_bits::*;

impl super::ZgcRealHeap {
    /// zt t31/zc11: the `[GC] zgc-skip-bits:` summary figures.
    pub fn skip_bits_report(&self) -> ZSkipBitsReport {
        self.counters.skipbits.report()
    }
}
