//! cv-web — WASM bindings for clustervision.
//!
//! The browser app uploads a harness directory as a `.zip`; we unzip it entirely in memory (no
//! filesystem), sniff + parse each entry via [`cv_core::ingest`], and hand back the sessions as a
//! JSON string for the frontend.

use std::io::Cursor;
use wasm_bindgen::prelude::*;

/// Ingest a complete bounded harness archive, rejecting partial/corrupt uploads explicitly.
#[wasm_bindgen]
pub fn ingest_zip(bytes: &[u8]) -> Result<String, JsValue> {
    let files = cv_core::ingest::archive::extract(Cursor::new(bytes), Default::default())
        .map_err(|error| JsValue::from_str(&format!("zip: {error:#}")))?;
    let sessions = cv_core::ingest::ingest_files(files);
    serde_json::to_string(&sessions).map_err(|error| JsValue::from_str(&format!("json: {error}")))
}
