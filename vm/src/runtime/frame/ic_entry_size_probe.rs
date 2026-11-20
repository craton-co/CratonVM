/// `InvokeCache::get` hands back `&CachedInvokeTarget` and every caller
/// immediately `.clone()`s it, so this is bytes copied per invoke on top of
/// the Arc refcount traffic.
#[test]
fn report_ic_entry_size() {
    eprintln!(
        "size_of::<CachedInvokeTarget<RetainedCode>>() = {}",
        std::mem::size_of::<
            cratonvm_classloading::resolution::CachedInvokeTarget<cratonvm_jit::RetainedCode>,
        >()
    );
    eprintln!(
        "size_of::<RetainedCode>() = {}",
        std::mem::size_of::<cratonvm_jit::RetainedCode>()
    );
}
