//! A query result read page by page: the rows Snowflake sends inline, then
//! each further chunk downloaded only when the user asks for more.

use std::collections::VecDeque;
use std::io::Cursor;

use arrow::ipc::reader::StreamReader;
use base64::Engine as _;
use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::CellLimits;
use dq_core::{DbError, DbResult};
use serde_json::Value;

use crate::client::{Chunk, Client, QueryData, RowType};
use crate::values::{arrow_rows, column_meta, json_rows};

pub struct ResultSet {
    pub rowtype: Vec<RowType>,
    rows: VecDeque<CellRow>,
    chunks: VecDeque<Chunk>,
    headers: serde_json::Map<String, Value>,
    qrmk: Option<String>,
    arrow: bool,
    limits: CellLimits,
}

fn decode_arrow(bytes: &[u8], rowtype: &[RowType], limits: CellLimits) -> DbResult<Vec<CellRow>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(|e| DbError::internal(format!("unreadable result: {e}")))?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| DbError::internal(format!("unreadable result: {e}")))?;
        out.extend(arrow_rows(&batch, rowtype, limits));
    }
    Ok(out)
}

impl ResultSet {
    pub fn new(data: QueryData, limits: CellLimits) -> DbResult<Self> {
        let arrow = data.format.as_deref().is_none_or(|f| f.eq_ignore_ascii_case("arrow")) && data.rowset.is_none();
        let rows = if let Some(rowset) = &data.rowset {
            json_rows(rowset, &data.rowtype, limits)
        } else {
            let encoded = data.rowset_base64.as_deref().unwrap_or_default();
            let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|e| DbError::internal(format!("unreadable result: {e}")))?;
            decode_arrow(&bytes, &data.rowtype, limits)?
        };
        Ok(Self {
            rowtype: data.rowtype,
            rows: rows.into(),
            chunks: data.chunks.into(),
            headers: data.chunk_headers,
            qrmk: data.qrmk,
            arrow,
            limits,
        })
    }

    pub fn columns(&self) -> Vec<ColumnMeta> {
        column_meta(&self.rowtype)
    }

    /// Up to `limit` rows, and whether more remain.
    pub async fn take(&mut self, client: &Client, limit: usize) -> DbResult<(Vec<CellRow>, bool)> {
        let mut out = Vec::with_capacity(limit.min(4096));
        loop {
            while out.len() < limit {
                match self.rows.pop_front() {
                    Some(row) => out.push(row),
                    None => break,
                }
            }
            if out.len() == limit || self.chunks.is_empty() {
                // A chunk may hold no rows; make sure a row really follows.
                while self.rows.is_empty() && !self.chunks.is_empty() && out.len() == limit {
                    self.load_chunk(client).await?;
                }
                return Ok((out, !self.rows.is_empty()));
            }
            self.load_chunk(client).await?;
        }
    }

    async fn load_chunk(&mut self, client: &Client) -> DbResult<()> {
        let Some(chunk) = self.chunks.pop_front() else { return Ok(()) };
        let bytes = client.chunk(&chunk, &self.headers, self.qrmk.as_deref()).await?;
        let rows = if self.arrow {
            decode_arrow(&bytes, &self.rowtype, self.limits)?
        } else {
            // JSON chunks are rows without the enclosing brackets: `[…],[…]`.
            let text = format!("[{}]", String::from_utf8_lossy(&bytes));
            let rows: Vec<Vec<Value>> = serde_json::from_str(&text).map_err(|e| DbError::internal(format!("unreadable result chunk: {e}")))?;
            json_rows(&rows, &self.rowtype, self.limits)
        };
        self.rows.extend(rows);
        Ok(())
    }

    /// Every row, for catalog queries.
    pub async fn all(mut self, client: &Client) -> DbResult<Vec<CellRow>> {
        let (rows, _) = self.take(client, usize::MAX).await?;
        Ok(rows)
    }
}
