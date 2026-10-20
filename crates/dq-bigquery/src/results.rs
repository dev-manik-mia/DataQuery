//! A query result read page by page: the rows of the first answer, then each
//! further page (by its page token) only when the caller asks for more.

use std::collections::VecDeque;

use dq_core::DbResult;
use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::CellLimits;

use crate::client::{Client, Field, JobRef, QueryPage};
use crate::values::{column_meta, rows};

/// Rows asked for per page when more are needed.
const PAGE_ROWS: usize = 10_000;

pub struct ResultSet {
    pub fields: Vec<Field>,
    buffered: VecDeque<CellRow>,
    job: JobRef,
    page_token: Option<String>,
    limits: CellLimits,
}

impl ResultSet {
    pub fn new(job: JobRef, page: QueryPage, limits: CellLimits) -> Self {
        let fields = page.schema.map(|s| s.fields).unwrap_or_default();
        Self { buffered: rows(&fields, &page.rows, limits).into(), fields, job, page_token: page.page_token, limits }
    }

    pub fn columns(&self) -> Vec<ColumnMeta> {
        column_meta(&self.fields)
    }

    /// Up to `limit` rows, and whether more remain.
    pub async fn take(&mut self, client: &Client, limit: usize) -> DbResult<(Vec<CellRow>, bool)> {
        let mut out = Vec::with_capacity(limit.min(4096));
        loop {
            let n = self.buffered.len().min(limit - out.len());
            out.extend(self.buffered.drain(..n));
            let Some(token) = self.page_token.as_deref().filter(|_| out.len() < limit) else { break };
            let wanted = (limit - out.len()).clamp(1, PAGE_ROWS) as u32;
            let page = client.page(&self.job, token, wanted).await?;
            self.buffered.extend(rows(&self.fields, &page.rows, self.limits));
            self.page_token = page.page_token;
        }
        Ok((out, !self.buffered.is_empty() || self.page_token.is_some()))
    }

    /// Every row, for catalog queries (which bound themselves with `LIMIT`).
    pub async fn all(mut self, client: &Client) -> DbResult<Vec<CellRow>> {
        Ok(self.take(client, usize::MAX).await?.0)
    }
}
