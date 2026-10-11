//! Rust engine and state behind the DuckDB C++ registration layer.
use std::error::Error;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, RwLock};
mod ffi;

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
pub(crate) struct Shared(Arc<RwLock<Indexes>>);

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

fn load(
    shared: &Shared,
    paths: &[Option<String>],
    poi: bool,
) -> Result<Vec<Option<String>>, Box<dyn Error>> {
    let name = if poi {
        "gridpin_load_poi"
    } else {
        "gridpin_load"
    };
    let Some(path) = single_path(name, paths)? else {
        return Ok(vec![None; paths.len()]);
    };
    let mut indexes = shared.write();
    let current = if poi {
        &indexes.poi_path
    } else {
        &indexes.addr_path
    };
    if current.as_deref() != Some(path.as_str()) {
        if let Some(cur) = current.as_deref() {
            if poi {
                return Err(format!(
                    "gridpin_load_poi: '{path}' while '{cur}' is loaded — the path must stay \
                         constant for the whole session; to switch run \
                         SELECT gridpin_reset(); then reload"
                )
                .into());
            }
            return Err(format!(
                "gridpin_load: '{path}' while '{cur}' is loaded — the path must stay \
                         constant for the whole session; to switch sheets run \
                         SELECT gridpin_reset(); then gridpin_load('{path}')"
            )
            .into());
        }
        if poi {
            let idx = gridpin::query::Index::open_poi(Path::new(&path))
                .map_err(|e| format!("gridpin_load_poi: {e}"))?;
            if let Some(addr) = indexes.addr.as_ref() {
                gridpin::query::check_pair(addr, &idx)
                    .map_err(|e| format!("gridpin_load_poi: {e}"))?;
            }
            indexes.poi = Some(idx);
            indexes.poi_path = Some(path.clone());
        } else {
            let idx = gridpin::query::Index::open_address(Path::new(&path))
                .map_err(|e| format!("gridpin_load: {e}"))?;
            if let Some(poi) = indexes.poi.as_ref() {
                if gridpin::query::check_pair(&idx, poi).is_err() {
                    indexes.poi = None;
                    indexes.poi_path = None;
                }
            }
            indexes.addr = Some(idx);
            indexes.addr_path = Some(path.clone());
        }
    }
    drop(indexes);
    Ok(paths
        .iter()
        .map(|p| p.as_ref().map(|p| format!("index loaded: {p}")))
        .collect())
}

fn reset(shared: &Shared, len: usize) -> Result<Vec<Option<String>>, Box<dyn Error>> {
    if len > 1 {
        return Err("gridpin_reset: must be a standalone statement (SELECT gridpin_reset()),                             not part of a multi-row query".into());
    }
    *shared.write() = Indexes::default();
    Ok(vec![Some("index unloaded".into()); len.max(1)])
}

fn geocode(
    shared: &Shared,
    queries: &[Option<String>],
) -> Result<Vec<Option<String>>, Box<dyn Error>> {
    let guard = shared.read();
    let idx = guard
        .addr
        .as_ref()
        .ok_or("call gridpin_load('<path>.bin') first")?;
    queries
        .iter()
        .map(|q| {
            q.as_ref()
                .map(|q| {
                    let hits = gridpin::query::query_cascade(idx, guard.poi.as_ref(), q, 1);
                    match hits.first() {
                        Some(h) => serde_json::to_string(h).map_err(Into::into),
                        None => Ok("{}".into()),
                    }
                })
                .transpose()
        })
        .collect()
}

fn reverse(
    shared: &Shared,
    coordinates: &[Option<(f64, f64)>],
) -> Result<Vec<Option<String>>, Box<dyn Error>> {
    let guard = shared.read();
    let idx = guard
        .addr
        .as_ref()
        .ok_or("call gridpin_load('<path>.bin') first")?;
    coordinates
        .iter()
        .map(|point| {
            point
                .map(|(lat, lon)| {
                    let hits = idx.try_reverse(lat, lon, 1)?;
                    match hits.first() {
                        Some(h) => serde_json::to_string(h).map_err(Into::into),
                        None => Ok("{}".into()),
                    }
                })
                .transpose()
        })
        .collect()
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
