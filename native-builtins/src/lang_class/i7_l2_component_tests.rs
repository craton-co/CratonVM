    use super::{loading_component_id, retyped_ref_array};
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
    use cratonvm_types::{ClassId, Value};

    /// A loaded component is found; an absent one the context cannot load
    /// (the mock's `load_class` answers nothing) keeps the pre-i7 `Object`.
    #[test]
    fn loading_component_id_finds_loaded_and_falls_back_to_object() {
        let mut ctx = mock_ctx();
        assert_eq!(loading_component_id(&mut ctx, "p/Absent"), ClassId::new(0));
        let url = ctx
            .ensure_class_initialized("java/net/URL")
            .expect("mock registers the class");
        assert_eq!(loading_component_id(&mut ctx, "java/net/URL"), url);
    }

    /// zt t2/rv: the listing component answers a loaded class by name on both
    /// arms of `CRATONVM_REFLECTION_LISTING_LOADS_COMPONENT`, and an absent one
    /// still falls back to `ClassId(0)` on both.
/// The copy carries the new component and every element, in order.
    #[test]
    fn retyped_ref_array_copies_every_element_under_the_new_component() {
        let mut ctx = mock_ctx();
        let component = ctx
            .ensure_class_initialized("java/util/logging/Handler")
            .expect("mock registers the class");
        let src = ctx.new_ref_array(ClassId::new(0), 3);
        let a = ctx.create_string("a");
        let c = ctx.create_string("c");
        ctx.set_array_element(src, 0, Value::Object(Some(a)));
        ctx.set_array_element(src, 2, Value::Object(Some(c)));
        let dst = retyped_ref_array(&mut ctx, src, component);
        assert_eq!(ctx.ref_array_component(dst), Some(component));
        assert_eq!(ctx.array_length(dst), 3);
        assert_eq!(ctx.get_array_element(dst, 0), Value::Object(Some(a)));
        assert_eq!(ctx.get_array_element(dst, 1), Value::Object(None));
        assert_eq!(ctx.get_array_element(dst, 2), Value::Object(Some(c)));
    }
