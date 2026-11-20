use super::*;

#[derive(Default)]
struct CountingSink {
    charged: parking_lot::Mutex<usize>,
}

impl JitBodyWithdrawalSink for CountingSink {
    fn bodies_withdrawn(&self, _keys: &[WithdrawnMethodKey]) {}
    fn optimizing_bodies_invalidated(&self, keys: &[WithdrawnMethodKey]) {
        *self.charged.lock() += keys.len();
    }
}

fn key(name: &str) -> WithdrawnMethodKey {
    (
        Arc::from("r21w5/Charged"),
        Arc::from(name),
        Arc::from("()V"),
        cratonvm_types::ClassId::new(0),
    )
}

