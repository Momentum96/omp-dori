//! Coordination records (DESIGN 7.3 "Jobs" and "Evidence", milestone 3b): one JSON document per
//! (kind, id) with a write counter, so each record has one writer and a compare-and-swap. Kinds are
//! named by their owners (`lanes::coord`); the store knows none of their fields.

use rusqlite::{OptionalExtension, Transaction, params};

use super::Reads;

/// One stored document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Doc {
    pub(crate) id: String,
    pub(crate) version: i64,
    pub(crate) at_ms: i64,
    pub(crate) body: String,
}

/// Every coordination mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoordWrite {
    /// Writes a document. `expect: Some(0)` creates it (refused when it exists), `Some(v)` needs the
    /// stored version `v`, `None` writes whatever is there.
    Put {
        kind: String,
        id: String,
        doc: String,
        expect: Option<i64>,
    },
    Delete {
        kind: String,
        id: String,
    },
    /// Keeps the newest `keep` documents of a kind by id order (ids sort by time where kept).
    Trim {
        kind: String,
        keep: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoordDone {
    /// The new version.
    Saved(i64),
    /// The stored version differs from the one expected.
    Moved,
    Done,
}

pub(crate) fn write(tx: &Transaction<'_>, w: &CoordWrite, now: i64) -> rusqlite::Result<CoordDone> {
    match w {
        CoordWrite::Put {
            kind,
            id,
            doc,
            expect,
        } => {
            let stored: Option<i64> = tx
                .query_row(
                    "SELECT version FROM coord WHERE kind = ?1 AND id = ?2",
                    params![kind, id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(want) = expect
                && stored.unwrap_or(0) != *want
            {
                return Ok(CoordDone::Moved);
            }
            let version = stored.unwrap_or(0) + 1;
            tx.execute(
                "INSERT INTO coord (kind, id, version, at_ms, doc) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (kind, id) DO UPDATE SET version = ?3, at_ms = ?4, doc = ?5",
                params![kind, id, version, now, doc],
            )?;
            Ok(CoordDone::Saved(version))
        }
        CoordWrite::Delete { kind, id } => {
            tx.execute(
                "DELETE FROM coord WHERE kind = ?1 AND id = ?2",
                params![kind, id],
            )?;
            Ok(CoordDone::Done)
        }
        CoordWrite::Trim { kind, keep } => {
            tx.execute(
                "DELETE FROM coord WHERE kind = ?1 AND id NOT IN
                 (SELECT id FROM coord WHERE kind = ?1 ORDER BY id DESC LIMIT ?2)",
                params![kind, keep],
            )?;
            Ok(CoordDone::Done)
        }
    }
}

impl Reads {
    pub(crate) fn coord(&self, kind: &str, id: &str) -> rusqlite::Result<Option<Doc>> {
        self.0
            .query_row(
                "SELECT id, version, at_ms, doc FROM coord WHERE kind = ?1 AND id = ?2",
                params![kind, id],
                |r| {
                    Ok(Doc {
                        id: r.get(0)?,
                        version: r.get(1)?,
                        at_ms: r.get(2)?,
                        body: r.get(3)?,
                    })
                },
            )
            .optional()
    }

    /// Every document of a kind, by id.
    pub(crate) fn coord_all(&self, kind: &str) -> rusqlite::Result<Vec<Doc>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT id, version, at_ms, doc FROM coord WHERE kind = ?1 ORDER BY id",
        )?;
        stmt.query_map(params![kind], |r| {
            Ok(Doc {
                id: r.get(0)?,
                version: r.get(1)?,
                at_ms: r.get(2)?,
                body: r.get(3)?,
            })
        })?
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::open_temp;

    fn put(id: &str, expect: Option<i64>) -> CoordWrite {
        CoordWrite::Put {
            kind: "item".into(),
            id: id.into(),
            doc: format!("{{\"id\":\"{id}\"}}"),
            expect,
        }
    }

    #[tokio::test]
    async fn compare_and_swap_and_trim() {
        let (store, _h, _dir) = open_temp("store-coord");
        assert_eq!(
            store.coord(put("a", Some(0))).await.unwrap(),
            CoordDone::Saved(1)
        );
        assert_eq!(
            store.coord(put("a", Some(0))).await.unwrap(),
            CoordDone::Moved
        );
        assert_eq!(
            store.coord(put("a", Some(1))).await.unwrap(),
            CoordDone::Saved(2)
        );
        assert_eq!(
            store.coord(put("a", None)).await.unwrap(),
            CoordDone::Saved(3)
        );
        for id in ["b", "c", "d"] {
            store.coord(put(id, None)).await.unwrap();
        }
        store
            .coord(CoordWrite::Trim {
                kind: "item".into(),
                keep: 2,
            })
            .await
            .unwrap();
        let all = store.read(|r| r.coord_all("item")).await.unwrap();
        let ids: Vec<&str> = all.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["c", "d"]);
        let one = store.read(|r| r.coord("item", "d")).await.unwrap().unwrap();
        assert_eq!(one.version, 1);
    }
}
