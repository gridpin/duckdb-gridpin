//! Narrow batch ABI. Rust owns database state and response storage.
use super::{geocode, load, no_panic, reset, reverse, Indexes, Shared};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, RwLock};

#[repr(C)]
pub struct Text {
    data: *const u8,
    len: usize,
    is_null: u8,
}

pub struct Response {
    rows: Vec<Option<String>>,
    error: Option<String>,
}

#[no_mangle]
pub(crate) extern "C" fn gridpin_state_new() -> *mut Shared {
    catch_unwind(|| Box::into_raw(Box::new(Shared(Arc::new(RwLock::new(Indexes::default()))))))
        .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// State is a live, uniquely owned pointer returned by gridpin_state_new, or null.
#[no_mangle]
pub(crate) unsafe extern "C" fn gridpin_state_drop(state: *mut Shared) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !state.is_null() {
            drop(Box::from_raw(state));
        }
    }));
}

/// # Safety
/// State is live; each non-null input array holds len initialized entries for the call.
/// Text buffers hold their declared byte length and are borrowed only until return.
#[no_mangle]
pub(crate) unsafe extern "C" fn gridpin_invoke(
    state: *const Shared,
    operation: u32,
    texts: *const Text,
    lats: *const f64,
    lons: *const f64,
    nulls: *const u8,
    len: usize,
) -> *mut Response {
    // Keep all input conversion, engine work and response allocation inside the ABI boundary.
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut rows = Vec::new();
        let name = match operation {
            0 => "gridpin_load",
            1 => "gridpin_load_poi",
            2 => "gridpin_reset",
            3 => "gridpin_geocode",
            4 => "gridpin_reverse",
            _ => "gridpin",
        };
        let result = no_panic(name, || {
            let shared = state.as_ref().ok_or("missing GridPin state")?;
            rows = if operation == 2 {
                reset(shared, len)?
            } else if operation == 4 {
                if len > 0 && (lats.is_null() || lons.is_null() || nulls.is_null()) {
                    return Err("missing coordinate buffers".into());
                }
                let points = (0..len)
                    .map(|i| {
                        if *nulls.add(i) != 0 {
                            None
                        } else {
                            Some((*lats.add(i), *lons.add(i)))
                        }
                    })
                    .collect::<Vec<_>>();
                reverse(shared, &points)?
            } else {
                if len > 0 && texts.is_null() {
                    return Err("missing text buffer".into());
                }
                let strings = (0..len)
                    .map(|i| {
                        let text = &*texts.add(i);
                        if text.is_null != 0 {
                            return Ok(None);
                        }
                        if text.data.is_null() {
                            return Err("missing text bytes".into());
                        }
                        let bytes = std::slice::from_raw_parts(text.data, text.len);
                        Ok(Some(std::str::from_utf8(bytes)?.to_string()))
                    })
                    .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
                match operation {
                    0 | 1 => load(shared, &strings, operation == 1)?,
                    3 => geocode(shared, &strings)?,
                    _ => return Err("unknown GridPin operation".into()),
                }
            };
            Ok(())
        });
        Box::into_raw(Box::new(Response {
            rows,
            error: result.err().map(|e| e.to_string()),
        }))
    }));
    // A null response is a C++ SQL error, never an unwind into DuckDB.
    result.unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// Response is live and output is writable. Returned bytes are borrowed until response_drop.
#[no_mangle]
pub(crate) unsafe extern "C" fn gridpin_response_get(
    response: *const Response,
    row: usize,
    output: *mut Text,
) -> u8 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(response) = response.as_ref() else {
            return 2;
        };
        let Some(output) = output.as_mut() else {
            return 2;
        };
        let value = if let Some(error) = response.error.as_ref() {
            Some(error)
        } else {
            response.rows.get(row).and_then(Option::as_ref)
        };
        *output = match value {
            Some(value) => Text {
                data: value.as_ptr(),
                len: value.len(),
                is_null: 0,
            },
            None => Text {
                data: std::ptr::null(),
                len: 0,
                is_null: 1,
            },
        };
        u8::from(response.error.is_some())
    }))
    .unwrap_or(2)
}

/// # Safety
/// Response is uniquely owned and returned by gridpin_invoke, or null.
#[no_mangle]
pub(crate) unsafe extern "C" fn gridpin_response_drop(response: *mut Response) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !response.is_null() {
            drop(Box::from_raw(response));
        }
    }));
}
