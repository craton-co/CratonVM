use super::*;

fn info(kind: u8, desc: &'static str) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/util/ArrayList",
        method_name: "get",
        descriptor: desc,
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

/// The classification is by the RESOLVED callback's identity plus the
/// call-site descriptor, and never for a static site or a
/// signature-polymorphic entry.
#[test]
fn arraylist_sites_classify_by_callback_identity() {
    let get: cratonvm_native_api::NativeCallback = cratonvm_native_collections::native_al_get;
    let size: cratonvm_native_api::NativeCallback = cratonvm_native_collections::native_al_size;
    for k in [0u8, 2] {
        assert!(
            collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), get, false)
                == LeafNativeKind::ArrayListGet
        );
        assert!(
            collection_leaf_kind(&info(k, "()I"), size, false) == LeafNativeKind::ArrayListSize
        );
        // Callback/descriptor mismatch: not served by the probe.
        assert!(collection_leaf_kind(&info(k, "()I"), get, false) == LeafNativeKind::Callback);
        assert!(
            collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), size, false)
                == LeafNativeKind::Callback
        );
        // Poly entries keep their own return-unboxing path.
        assert!(
            collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), get, true)
                == LeafNativeKind::Callback
        );
    }
    // A static site has no receiver, and an `invokespecial` keeps the
    // plain callback path.
    for k in [1u8, 3] {
        assert!(
            collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), get, false)
                == LeafNativeKind::Callback
        );
    }
}
