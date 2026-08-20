//! DuckDB extension: geocoding directly in SQL.
//!
//!   LOAD 'gridpin_ext.duckdb_extension';
//!   SELECT gridpin_load('data/france.bin');           -- open the index (mmap)
//!   SELECT gridpin_load_poi('data/fr_poi.bin');       -- OPTIONAL: POI layer (cascade)
//!   SELECT gridpin_geocode(address) FROM clients;     -- best hit as a JSON string
//!   SELECT gridpin_reset();                           -- unload (required before switching sheets)
//!
//! All functions carry the gridpin_ prefix: DuckDB's function catalog is shared
//! across extensions, so generic names would risk collisions (community practice:
//! h3_*, ST_*).

use std::error::Error;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, RwLock};

use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeId};
use duckdb::ffi::duckdb_string_t;
use duckdb::types::DuckString;
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;
use duckdb::{duckdb_entrypoint_c_api, Connection, Result};

/// shared index holder (address index + optional POI layer; cloned as Arc).
/// SCOPE (documented limitation, audit R8-H14): this state is per DATABASE
/// process, not per connection — DuckDB registers extension functions in the shared catalog, so
/// a reset+load on another connection swaps the sheet under statements running elsewhere. The
/// scalar-function API offers no statement/connection boundary to enforce it; the README tells
/// users to coordinate switches externally. In-statement swaps ARE enforced (loads refuse a
/// different path; reset refuses multi-row invocations).
#[derive(Default)]
struct Indexes {
    addr: Option<gridpin::query::Index>,
    poi: Option<gridpin::query::Index>,
    // remember the loaded paths so a per-chunk re-invoke with the SAME constant path does not
    // reopen (and re-leak the mmap) on every 2048-row DataChunk (audit R6-H14)
    addr_path: Option<String>,
    poi_path: Option<String>,
}

/// The load argument must be a SINGLE CONSTANT path, not a column of differing values. Within one
/// DataChunk this returns the sole non-NULL path (or None if all NULL); differing paths are a
/// hard error rather than the misleading "last path in the chunk wins" (audit R6-H14).
fn single_path(
    fn_name: &str,
    paths: &[Option<String>],
) -> std::result::Result<Option<String>, Box<dyn Error>> {
    let mut it = paths.iter().flatten();
    let Some(first) = it.next() else {
        return Ok(None);
    };
    if it.any(|p| p != first) {
        return Err(format!(
            "{fn_name} expects a single constant path, not a column of differing paths"
        )
        .into());
    }
    Ok(Some(first.clone()))
}

#[derive(Clone)]
struct Shared(Arc<RwLock<Indexes>>);

impl Shared {
    // A poisoned lock only means another thread panicked mid-update; the state is
    // two Options, always structurally valid — recover instead of propagating the
    // panic into the host database process (audit-16 C-02).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Indexes> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Indexes> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// duckdb-rs invokes extensions without a panic boundary: an unwinding panic
/// crosses the C ABI and aborts the WHOLE host process (the database, and
/// whatever application embeds it). Convert panics into SQL errors instead.
fn no_panic<F>(name: &str, f: F) -> std::result::Result<(), Box<dyn Error>>
where
    F: FnOnce() -> std::result::Result<(), Box<dyn Error>>,
{
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(format!("{name}: internal error: {msg}").into())
        }
    }
}

unsafe fn read_strings(input: &mut DataChunkHandle, col: usize) -> Vec<Option<String>> {
    // The C API flattens the chunk before calling us (duckdb capi Flatten), so a
    // flat read of `n` rows is safe; the temporary-copy DuckString is upstream
    // duckdb-rs idiom — `.to_string()` must stay inside the same expression.
    let n = input.len();
    let v = input.flat_vector(col);
    let raw = v.as_slice_with_len::<duckdb_string_t>(n);
    (0..n)
        .map(|i| {
            if v.row_is_null(i as u64) {
                None
            } else {
                Some(DuckString::new(&mut { raw[i] }).as_str().to_string())
            }
        })
        .collect()
}

/// Fill the status column for a load call: exactly ONE path is opened per call (all non-NULL
/// rows are equal by single_path, and a path differing from the loaded one is a hard error —
/// audit R8-H14) — opening per row would leak one mmap per row on a column argument
/// (audit-16 H-01). NULL rows get NULL (SQL semantics, audit-16 M-02).
unsafe fn load_statuses(paths: &[Option<String>], loaded: &str, output: &mut dyn WritableVector) {
    let mut out = output.flat_vector();
    for (i, p) in paths.iter().enumerate() {
        match p.as_deref() {
            None => out.set_null(i),
            Some(p) if p == loaded => out.insert(i, format!("index loaded: {p}").as_str()),
            Some(p) => out.insert(i, format!("ignored (last path wins): {p}").as_str()),
        }
    }
}

/// gridpin_load('path/country.bin') -> status message
struct LoadFn;

impl VScalar for LoadFn {
    type State = Shared;

    unsafe fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn Error>> {
        let paths = read_strings(input, 0);
        no_panic("gridpin_load", || {
            let Some(last) = single_path("gridpin_load", &paths)? else {
                // all NULL: nothing to load, NULL out for every row
                let mut out = output.flat_vector();
                (0..paths.len()).for_each(|i| out.set_null(i));
                return Ok(());
            };
            let mut indexes = state.write();
            // idempotent: the same constant path across chunks must not reopen/re-leak (R6-H14)
            if indexes.addr_path.as_deref() != Some(last.as_str()) {
                // The loaded path may NEVER change implicitly (audit R8-H14): scalar functions see
                // one DataChunk at a time with no statement boundary, so a column whose path flips
                // between chunks of ONE query would silently swap the index mid-query and mix two
                // countries' answers in the same result set. A different path is therefore a hard,
                // deterministic error; switching sheets is EXPLICIT — gridpin_reset() then load.
                if let Some(cur) = indexes.addr_path.as_deref() {
                    return Err(format!(
                        "gridpin_load: '{last}' while '{cur}' is loaded — the path must stay \
                         constant for the whole session; to switch sheets run \
                         SELECT gridpin_reset(); then gridpin_load('{last}')"
                    )
                    .into());
                }
                let idx = gridpin::query::Index::open_address(Path::new(&last))
                    .map_err(|e| format!("gridpin_load: {e}"))?;
                // A POI layer belongs to the country it was built for. Keep an already-loaded
                // POI ONLY if it pairs with the address index (same country/layer): this preserves
                // the layer when POI was loaded FIRST (self-review round 2), and refuses French
                // cafes answering Italian queries.
                if let Some(poi) = indexes.poi.as_ref() {
                    if gridpin::query::check_pair(&idx, poi).is_err() {
                        indexes.poi = None;
                        indexes.poi_path = None;
                    }
                }
                indexes.addr = Some(idx);
                indexes.addr_path = Some(last.clone());
            }
            drop(indexes);
            load_statuses(&paths, &last, output);
            Ok(())
        })
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeId::Varchar.into()],
            LogicalTypeId::Varchar.into(),
        )]
    }
}

/// gridpin_load_poi('path/country_poi.bin') -> status message. Cascade: the POI layer is
/// queried only when the address answer is weak and never overrides an exact house hit
/// (query::query_cascade).
struct LoadPoiFn;

impl VScalar for LoadPoiFn {
    type State = Shared;

    unsafe fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn Error>> {
        let paths = read_strings(input, 0);
        no_panic("gridpin_load_poi", || {
            let Some(last) = single_path("gridpin_load_poi", &paths)? else {
                let mut out = output.flat_vector();
                (0..paths.len()).for_each(|i| out.set_null(i));
                return Ok(());
            };
            let mut indexes = state.write();
            if indexes.poi_path.as_deref() != Some(last.as_str()) {
                // the same no-implicit-switch rule as gridpin_load (audit R8-H14)
                if let Some(cur) = indexes.poi_path.as_deref() {
                    return Err(format!(
                        "gridpin_load_poi: '{last}' while '{cur}' is loaded — the path must stay \
                         constant for the whole session; to switch run \
                         SELECT gridpin_reset(); then reload"
                    )
                    .into());
                }
                // R7-H11: open_poi refuses an ADDRESS sheet loaded as a POI layer (the old
                // permissive `open` accepted it). Symmetric to gridpin_load's open_address.
                let idx = gridpin::query::Index::open_poi(Path::new(&last))
                    .map_err(|e| format!("gridpin_load_poi: {e}"))?;
                // v6 identity: refuse a POI layer from another country (audit H-08)
                if let Some(addr) = indexes.addr.as_ref() {
                    gridpin::query::check_pair(addr, &idx)
                        .map_err(|e| format!("gridpin_load_poi: {e}"))?;
                }
                indexes.poi = Some(idx);
                indexes.poi_path = Some(last.clone());
            }
            drop(indexes);
            load_statuses(&paths, &last, output);
            Ok(())
        })
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeId::Varchar.into()],
            LogicalTypeId::Varchar.into(),
        )]
    }
}

/// gridpin_reset() -> status. Unloads the address index AND the POI layer. The ONLY way to switch
/// sheets (audit R8-H14): loads refuse a different path, so a mid-query path flip can never swap
/// the index under a running statement — switching is an explicit two-statement act.
struct ResetFn;

impl VScalar for ResetFn {
    type State = Shared;

    unsafe fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn Error>> {
        let n = input.len().max(1);
        no_panic("gridpin_reset", || {
            // A reset embedded in a MULTI-ROW projection re-arms gridpin_load on every chunk, so
            // `SELECT gridpin_reset(), gridpin_load(<column>), …` swapped the index mid-query
            // despite the no-implicit-switch guard (audit R8-H14). A reset is a
            // standalone statement: refuse any invocation carrying more than one row.
            if input.len() > 1 {
                return Err("gridpin_reset: must be a standalone statement (SELECT gridpin_reset()),                             not part of a multi-row query"
                    .into());
            }
            let mut indexes = state.write();
            indexes.addr = None;
            indexes.addr_path = None;
            indexes.poi = None;
            indexes.poi_path = None;
            drop(indexes);
            let out = output.flat_vector();
            (0..n).for_each(|i| out.insert(i, "index unloaded"));
            Ok(())
        })
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![],
            LogicalTypeId::Varchar.into(),
        )]
    }

    fn volatile() -> bool {
        true // a side-effect function must never be constant-folded away
    }
}

/// gridpin_geocode('address') -> JSON of the best hit ('{}' if none)
struct GeocodeFn;

impl VScalar for GeocodeFn {
    type State = Shared;

    unsafe fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn Error>> {
        let queries = read_strings(input, 0);
        no_panic("gridpin_geocode", || {
            let guard = state.read();
            let idx = guard
                .addr
                .as_ref()
                .ok_or("call gridpin_load('<path>.bin') first")?;
            let mut out = output.flat_vector();
            for (i, q) in queries.iter().enumerate() {
                match q {
                    None => out.set_null(i), // NULL in -> NULL out
                    Some(q) => {
                        let hits = gridpin::query::query_cascade(idx, guard.poi.as_ref(), q, 1);
                        let json = match hits.first() {
                            Some(h) => serde_json::to_string(h)?,
                            None => "{}".to_string(),
                        };
                        out.insert(i, json.as_str());
                    }
                }
            }
            Ok(())
        })
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeId::Varchar.into()],
            LogicalTypeId::Varchar.into(),
        )]
    }
}

/// gridpin_reverse(lat, lon) -> JSON of the nearest indexed house within ~10 km ('{}' if none);
/// approximate — check precision/distance_m (audit R6-H09)
struct ReverseFn;

impl VScalar for ReverseFn {
    type State = Shared;

    unsafe fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn Error>> {
        let n = input.len();
        let vlat = input.flat_vector(0);
        let vlon = input.flat_vector(1);
        let lats = vlat.as_slice_with_len::<f64>(n).to_vec();
        let lons = vlon.as_slice_with_len::<f64>(n).to_vec();
        let null_lat: Vec<bool> = (0..n).map(|i| vlat.row_is_null(i as u64)).collect();
        let null_lon: Vec<bool> = (0..n).map(|i| vlon.row_is_null(i as u64)).collect();
        no_panic("gridpin_reverse", || {
            let guard = state.read();
            let idx = guard
                .addr
                .as_ref()
                .ok_or("call gridpin_load('<path>.bin') first")?;
            let mut out = output.flat_vector();
            for i in 0..n {
                if null_lat[i] || null_lon[i] {
                    out.set_null(i);
                    continue;
                }
                // try_reverse is the single strict entry point (R6-M02/R7-M02): NaN/Inf/out-of-range
                // is a clear error, not a silent empty "success", identically to the CLI/py.
                let hits = idx.try_reverse(lats[i], lons[i], 1)?;
                let json = match hits.first() {
                    Some(h) => serde_json::to_string(h)?,
                    None => "{}".to_string(),
                };
                out.insert(i, json.as_str());
            }
            Ok(())
        })
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeId::Double.into(), LogicalTypeId::Double.into()],
            LogicalTypeId::Varchar.into(),
        )]
    }
}

#[duckdb_entrypoint_c_api()]
pub unsafe fn extension_entrypoint(con: Connection) -> Result<(), Box<dyn Error>> {
    let shared = Shared(Arc::new(RwLock::new(Indexes::default())));
    con.register_scalar_function_with_state::<LoadFn>("gridpin_load", &shared)?;
    con.register_scalar_function_with_state::<LoadPoiFn>("gridpin_load_poi", &shared)?;
    con.register_scalar_function_with_state::<ResetFn>("gridpin_reset", &shared)?;
    con.register_scalar_function_with_state::<GeocodeFn>("gridpin_geocode", &shared)?;
    con.register_scalar_function_with_state::<ReverseFn>("gridpin_reverse", &shared)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_path_enforces_a_constant_path_contract() {
        // audit R6-H14: the load arg must be a single constant path, not a per-chunk "last wins".
        assert_eq!(
            single_path("f", &[None, None]).unwrap(),
            None,
            "all NULL -> nothing to load"
        );
        assert_eq!(
            single_path("f", &[None, Some("a".into()), Some("a".into())]).unwrap(),
            Some("a".to_string()),
            "one distinct non-NULL path is the constant"
        );
        let err = single_path("f", &[Some("a".into()), Some("b".into())]).unwrap_err();
        assert!(err.to_string().contains("single constant path"), "{err}");
    }

    #[test]
    fn no_panic_converts_a_panic_into_a_sql_error() {
        // duckdb-rs has no panic boundary: an unwinding panic would abort the host DB.
        // no_panic must turn it into an Err carrying the message (audit-16 C-02).
        let ok = no_panic("f", || Ok(()));
        assert!(ok.is_ok());
        let boom = no_panic("gridpin_geocode", || {
            panic!("kaboom");
        });
        let msg = boom.unwrap_err().to_string();
        assert!(
            msg.contains("gridpin_geocode") && msg.contains("kaboom"),
            "got: {msg}"
        );
    }

    #[test]
    fn shared_recovers_from_a_poisoned_lock() {
        // A thread panicking while holding the lock poisons it; the state is two Options
        // (always structurally valid), so read()/write() must recover, not re-panic and
        // take down the host (audit-16 C-02).
        let shared = Shared(Arc::new(RwLock::new(Indexes::default())));
        let s2 = shared.clone();
        // poison the lock: panic while holding the write guard
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = s2.0.write().unwrap();
            panic!("poison it");
        }));
        assert!(shared.0.is_poisoned(), "precondition: lock is poisoned");
        // both accessors must still hand back a usable guard
        assert!(shared.read().addr.is_none());
        shared.write().poi = None;
    }
}
