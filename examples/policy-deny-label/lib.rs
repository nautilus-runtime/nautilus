//! Example Wasm policy that denies requests carrying the nautilus.policy/deny label.

use std::sync::Mutex;

// The runtime instantiates a fresh module for each policy call.
static PAYLOAD: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static REASON: &[u8] = b"expected valid policy JSON without the nautilus.policy/deny label";

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut payload = PAYLOAD.lock().unwrap();
    payload.resize(usize::try_from(len).unwrap(), 0);
    payload.as_mut_ptr() as i32
}

#[no_mangle]
pub extern "C" fn evaluate(ptr: i32, len: i32) -> i32 {
    let payload = PAYLOAD.lock().unwrap();
    if ptr != payload.as_ptr() as i32 || usize::try_from(len) != Ok(payload.len()) {
        return 1;
    }
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(&payload) else {
        return 1;
    };
    match request["labels"].as_object() {
        Some(labels) if !labels.contains_key("nautilus.policy/deny") => 0,
        _ => 1,
    }
}

#[no_mangle]
pub extern "C" fn reason_ptr() -> i32 {
    REASON.as_ptr() as i32
}

#[no_mangle]
pub extern "C" fn reason_len() -> i32 {
    REASON.len() as i32
}
