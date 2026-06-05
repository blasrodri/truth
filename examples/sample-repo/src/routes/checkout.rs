//! Legacy checkout route handler.

/// Payments are retried this many times before giving up.
pub const PAYMENT_RETRY_COUNT: u32 = 5;

/// Alias used by callers / docs.
pub const MAX_RETRIES: u32 = 5;

/// Per-request timeout in seconds.
pub const REQUEST_TIMEOUT: u32 = 30;

pub fn register(router: &mut Router) {
    // The legacy checkout endpoint is still wired up.
    router.post("/v1/checkout", handle_checkout);
    router.get("/v1/checkout", handle_checkout_status);
}

fn handle_checkout() {}
fn handle_checkout_status() {}

pub struct Router;
impl Router {
    pub fn post(&mut self, _path: &str, _h: fn()) {}
    pub fn get(&mut self, _path: &str, _h: fn()) {}
}
