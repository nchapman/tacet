use std::collections::HashMap;

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::LibraryStore;
use crate::library::types::{LibraryError, Result};

impl LibraryStore {
    pub fn get_pref(&self, key: &str) -> Result<Option<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let val: Option<String> = conn
            .query_row(
                "SELECT value FROM app_prefs WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(val)
    }

    pub fn set_pref(&self, key: &str, value: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO app_prefs(key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn delete_pref(&self, key: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        conn.execute("DELETE FROM app_prefs WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn list_prefs(&self) -> Result<HashMap<String, String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare("SELECT key, value FROM app_prefs")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for r in rows {
            let (k, v) = r?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub fn get_provider_credentials<T: DeserializeOwned>(
        &self,
        provider: &str,
    ) -> Result<Option<T>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT data_json FROM provider_credentials WHERE provider = ?1",
                params![provider],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let v = serde_json::from_str(&raw)?;
        Ok(Some(v))
    }

    pub fn set_provider_credentials<T: Serialize>(&self, provider: &str, src: &T) -> Result<()> {
        let raw = serde_json::to_string(src)?;
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO provider_credentials(provider, data_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET data_json = excluded.data_json, updated_at = excluded.updated_at",
            params![provider, raw, now],
        )?;
        Ok(())
    }

    pub fn delete_provider_credentials(&self, provider: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        conn.execute(
            "DELETE FROM provider_credentials WHERE provider = ?1",
            params![provider],
        )?;
        Ok(())
    }
}
