use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn build_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register(&mut r);
    r
}

#[test]
fn standard_config_data_resolver_uses_real_bytecode() {
    let r = build_registry();
    const CLASS: &str =
        "org/springframework/boot/context/config/StandardConfigDataLocationResolver";
    assert!(r
        .find(
            CLASS,
            "resolve",
            "(Lorg/springframework/boot/context/config/ConfigDataLocationResolverContext;Lorg/springframework/boot/context/config/ConfigDataLocation;)Ljava/util/List;",
        )
        .is_none());
    assert!(r
        .find(
            CLASS,
            "resolveProfileSpecific",
            "(Lorg/springframework/boot/context/config/ConfigDataLocationResolverContext;Lorg/springframework/boot/context/config/ConfigDataLocation;Lorg/springframework/boot/context/config/Profiles;)Ljava/util/List;",
        )
        .is_none());
}

#[test]
fn register_is_callable_on_fresh_registry() {
    // Smoke test: registration helper must run to completion against an
    // empty registry without panic. Equivalent to lib.rs's wiring path.
    let mut r = NativeMethodRegistry::new();
    register(&mut r);
}

#[test]
fn bean_factory_intercepts_registered() {
    let r = build_registry();
    assert!(r
        .find(
            GENERIC_CTX,
            "getBeanFactory",
            "()Lorg/springframework/beans/factory/support/DefaultListableBeanFactory;",
        )
        .is_some());
}

#[test]
fn orphan_bean_filter_intercepts_registered() {
    // The sportme cascade — every filter point should be wired.
    let r = build_registry();
    assert!(r
        .find(
            "org/springframework/beans/factory/support/AbstractBeanDefinition",
            "getBeanClassName",
            "()Ljava/lang/String;",
        )
        .is_some());
    assert!(r
        .find(
            "org/springframework/beans/factory/support/AbstractBeanDefinition",
            "hasBeanClass",
            "()Z",
        )
        .is_some());
    assert!(r
        .find(
            "org/springframework/beans/factory/support/BeanDefinitionReaderUtils",
            "registerBeanDefinition",
            "(Lorg/springframework/beans/factory/config/BeanDefinitionHolder;\
             Lorg/springframework/beans/factory/support/BeanDefinitionRegistry;)V",
        )
        .is_some());
}

#[test]
fn environment_intercepts_registered() {
    let r = build_registry();
    // getEnvironment/createEnvironment are always registered — they are
    // non-null guards that prefer the real StandardEnvironment.<init>(),
    // not fabricated-config shims.
    assert!(r
        .find(
            ABSTRACT_CTX,
            "getEnvironment",
            "()Lorg/springframework/core/env/ConfigurableEnvironment;",
        )
        .is_some());
}

// B5: the faked property-access getters are only installed under the
// default-OFF `app-stubs` feature. Under the default build the real
// Environment bytecode answers property queries, so the override is
// intentionally absent.
#[cfg(feature = "app-stubs")]
#[test]
fn environment_property_access_intercepts_registered_under_app_stubs() {
    let r = build_registry();
    assert!(r
        .find(
            "org/springframework/core/env/StandardEnvironment",
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
        )
        .is_some());
}
