//! D93 pins: the concurrency shapes the daemon relies on, asserted at
//! the type level so a refactor that breaks them fails here, not in a
//! deadlock or a data-race hunt.

fn assert_sync<T: Sync>() {}
fn assert_send<T: Send>() {}

/// The drone fleet shares ONE executor (compiled components cache
/// once); this is the load-bearing `Sync`.
#[test]
fn executor_is_sync() {
    assert_sync::<datboi_exec::Executor<'static>>();
    assert_send::<datboi_exec::Executor<'static>>();
}

/// The store is shared by every thread in the process (request path,
/// prime, drones) by plain reference.
#[test]
fn store_is_sync() {
    assert_sync::<datboi_store_fs::Store>();
}
