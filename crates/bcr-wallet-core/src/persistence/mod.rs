// ----- standard library imports
// ----- extra library imports
// ----- local modules
#[cfg(not(target_arch = "wasm32"))]
pub mod inmemory;
#[cfg(target_arch = "wasm32")]
pub mod rexie;

// ----- end imports
