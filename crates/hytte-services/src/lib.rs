//! Async clients to system daemons exposed as hytte services. Each module
//! provides a `service()` constructor (registered via `App::with`) and free
//! functions returning `Signal`s of the daemon's state.
