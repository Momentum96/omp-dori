//! Typed access to the coordination records of `store::coord`: each kind is a JSON document type
//! with one writer (the hub), read and written with compare-and-swap on its version.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::store::Store;
use crate::store::coord::{CoordDone, CoordWrite};

/// Record kinds.
pub(crate) const QUEUED: &str = "queued";
pub(crate) const ITEM: &str = "item";
pub(crate) const PLANNER: &str = "planner";
pub(crate) const PLAN_RUN: &str = "plan_run";
pub(crate) const VERDICT: &str = "verdict";
pub(crate) const FILE: &str = "file";
pub(crate) const RULE: &str = "rule";
pub(crate) const BOARD: &str = "board";

pub(crate) const MOVED: &str = "busy: the record changed meanwhile; read it again";

pub(crate) async fn get<T: DeserializeOwned + Send + 'static>(
    store: &Store,
    kind: &'static str,
    id: &str,
) -> Result<Option<(T, i64)>, String> {
    let id = id.to_string();
    let doc = store
        .read(move |r| r.coord(kind, &id))
        .await
        .map_err(|e| e.to_string())?;
    doc.map(|d| {
        serde_json::from_str(&d.body)
            .map(|t| (t, d.version))
            .map_err(|e| format!("{kind} record {} damaged: {e}", d.id))
    })
    .transpose()
}

/// Every readable document of a kind, by id; a damaged one is skipped with its error.
pub(crate) async fn all<T: DeserializeOwned + Send + 'static>(
    store: &Store,
    kind: &'static str,
) -> Result<(Vec<(T, i64)>, Vec<String>), String> {
    let docs = store
        .read(move |r| r.coord_all(kind))
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(docs.len());
    let mut damaged = Vec::new();
    for d in docs {
        match serde_json::from_str(&d.body) {
            Ok(t) => out.push((t, d.version)),
            Err(e) => damaged.push(format!("{kind} {}: {e}", d.id)),
        }
    }
    Ok((out, damaged))
}

/// Writes a document; `expect: Some(0)` creates, `Some(v)` swaps version `v`, `None` overwrites.
pub(crate) async fn put<T: Serialize>(
    store: &Store,
    kind: &str,
    id: &str,
    doc: &T,
    expect: Option<i64>,
) -> Result<i64, String> {
    let doc = serde_json::to_string(doc).map_err(|e| e.to_string())?;
    match store
        .coord(CoordWrite::Put {
            kind: kind.into(),
            id: id.into(),
            doc,
            expect,
        })
        .await
        .map_err(|e| e.to_string())?
    {
        CoordDone::Saved(v) => Ok(v),
        CoordDone::Moved => Err(MOVED.into()),
        CoordDone::Done => Err("unexpected store answer".into()),
    }
}

pub(crate) async fn delete(store: &Store, kind: &str, id: &str) -> Result<(), String> {
    store
        .coord(CoordWrite::Delete {
            kind: kind.into(),
            id: id.into(),
        })
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

pub(crate) async fn trim(store: &Store, kind: &str, keep: i64) -> Result<(), String> {
    store
        .coord(CoordWrite::Trim {
            kind: kind.into(),
            keep,
        })
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}
