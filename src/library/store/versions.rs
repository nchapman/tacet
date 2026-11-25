use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, Transaction, params};

use super::LibraryStore;
use crate::library::types::{LibraryError, Result, quality_rank, short_hash};

impl LibraryStore {
    pub fn set_primary_version(&self, album_id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let group_id: Option<String> = tx
            .query_row(
                "SELECT version_group_id FROM albums WHERE id = ?1",
                params![album_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(group_id) = group_id else {
            return Err(LibraryError::NotFound);
        };
        if !group_id.is_empty() {
            tx.execute(
                "UPDATE albums SET is_primary_override = 0 WHERE version_group_id = ?1",
                params![group_id],
            )?;
        }
        tx.execute(
            "UPDATE albums SET is_primary_override = 1 WHERE id = ?1",
            params![album_id],
        )?;
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn group_versions(&self, album_ids: &[String]) -> Result<String> {
        let mut unique: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for id in album_ids {
            let trimmed = id.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                unique.push(trimmed.to_string());
            }
        }
        if unique.len() < 2 {
            return Err(LibraryError::InvalidArgument(
                "at least 2 distinct albumIds are required".into(),
            ));
        }
        unique.sort();

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let group_id = format!(
            "manual:{}",
            short_hash(&format!("manual:{}", unique.join(",")))
        );
        for id in &unique {
            let n = tx.execute(
                "UPDATE albums SET version_group_id = ?1, version_excluded = 0 WHERE id = ?2",
                params![group_id, id],
            )?;
            if n == 0 {
                return Err(LibraryError::NotFound);
            }
        }
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(group_id)
    }

    pub fn ungroup_version(&self, album_id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let n = tx.execute(
            "UPDATE albums SET version_group_id = '', version_excluded = 1, is_primary_override = 0 WHERE id = ?1",
            params![album_id],
        )?;
        if n == 0 {
            return Err(LibraryError::NotFound);
        }
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }
}

pub(super) fn recompute_versions_tx(tx: &Transaction<'_>) -> Result<()> {
    let mut all: Vec<VersionAlbRow> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, source_type, primary_format, max_bit_depth, max_sample_rate,
                    track_count, version_key, version_group_id, is_primary, version_excluded,
                    is_primary_override, available
             FROM albums",
        )?;
        let rows = stmt.query_map([], |r| {
            let group_id: String = r.get(7)?;
            Ok(VersionAlbRow {
                id: r.get(0)?,
                source_type: r.get(1)?,
                format: r.get(2)?,
                bit_depth: r.get(3)?,
                sample_rate: r.get(4)?,
                track_count: r.get(5)?,
                version_key: r.get(6)?,
                orig_group_id: group_id.clone(),
                group_id,
                orig_primary: r.get::<_, i32>(8)? != 0,
                excluded: r.get::<_, i32>(9)? != 0,
                override_primary: r.get::<_, i32>(10)? != 0,
                available: r.get::<_, i32>(11)? != 0,
            })
        })?;
        for r in rows {
            all.push(r?);
        }
    }

    let mut key_counts: HashMap<String, usize> = HashMap::new();
    for r in &all {
        if r.excluded || r.group_id.starts_with("manual:") || r.version_key.is_empty() {
            continue;
        }
        *key_counts.entry(r.version_key.clone()).or_insert(0) += 1;
    }

    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in all.iter_mut().enumerate() {
        if r.excluded {
            r.group_id.clear();
        } else if !r.group_id.starts_with("manual:") {
            if !r.version_key.is_empty()
                && key_counts.get(&r.version_key).copied().unwrap_or(0) >= 2
            {
                r.group_id = format!("auto:{}", short_hash(&format!("vgroup:{}", r.version_key)));
            } else {
                r.group_id.clear();
            }
        }
        if !r.group_id.is_empty() {
            groups.entry(r.group_id.clone()).or_default().push(i);
        }
    }

    let mut is_primary = vec![true; all.len()];
    for indices in groups.values() {
        if indices.len() < 2 {
            for &idx in indices {
                all[idx].group_id.clear();
                is_primary[idx] = true;
            }
            continue;
        }
        let mut best_idx = indices[0];
        for &idx in &indices[1..] {
            if is_better_primary(&all[idx], &all[best_idx]) {
                best_idx = idx;
            }
        }
        for &idx in indices {
            is_primary[idx] = idx == best_idx;
        }
    }

    let mut stmt = None;
    for (i, r) in all.iter().enumerate() {
        if r.group_id == r.orig_group_id && is_primary[i] == r.orig_primary {
            continue;
        }
        if stmt.is_none() {
            stmt = Some(tx.prepare(
                "UPDATE albums SET version_group_id = ?1, is_primary = ?2 WHERE id = ?3",
            )?);
        }
        if let Some(s) = stmt.as_mut() {
            s.execute(params![r.group_id, if is_primary[i] { 1 } else { 0 }, r.id])?;
        }
    }
    Ok(())
}

fn is_better_primary(a: &VersionAlbRow, b: &VersionAlbRow) -> bool {
    if a.available != b.available {
        return a.available;
    }
    if a.override_primary != b.override_primary {
        return a.override_primary;
    }
    let ra = quality_rank(&a.format, a.bit_depth, a.sample_rate, &a.source_type);
    let rb = quality_rank(&b.format, b.bit_depth, b.sample_rate, &b.source_type);
    if ra != rb {
        return ra > rb;
    }
    if a.track_count != b.track_count {
        return a.track_count > b.track_count;
    }
    a.id < b.id
}

struct VersionAlbRow {
    id: String,
    source_type: String,
    format: String,
    bit_depth: i32,
    sample_rate: i32,
    track_count: i32,
    version_key: String,
    group_id: String,
    orig_group_id: String,
    orig_primary: bool,
    excluded: bool,
    override_primary: bool,
    available: bool,
}
