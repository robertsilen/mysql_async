// Copyright (c) 2017 Anatoly Ikorsky
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

use mysql_common::row::convert::FromRowError;
use mysql_common::{io::ReadMysqlExt, packets::parse_local_infile_packet};
use tokio::prelude::*;

use std::{borrow::Cow, marker::PhantomData, result::Result as StdResult, sync::Arc};

use crate::{
    connection_like::{Connection, ConnectionLike},
    consts::StatusFlags,
    error::*,
    prelude::{FromRow, Protocol},
    Column, Row,
};

/// Result set metadata.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ResultSetMeta {
    /// Text result set, that may contain rows.
    Text(Arc<Vec<Column>>),
    /// Binary result set, that may contain rows.
    Binary(Arc<Vec<Column>>),
}

impl ResultSetMeta {
    fn columns(&self) -> &Arc<Vec<Column>> {
        match self {
            ResultSetMeta::Text(columns) | ResultSetMeta::Binary(columns) => columns,
        }
    }
}

/// Result of a query or statement execution.
pub struct QueryResult<'a, 't: 'a, P> {
    conn: Connection<'a, 't>,
    __phantom: PhantomData<P>,
}

impl<'a, 't: 'a, P> QueryResult<'a, 't, P>
where
    P: Protocol,
{
    pub(crate) fn new<T: Into<Connection<'a, 't>>>(conn: T) -> Self {
        QueryResult {
            conn: conn.into(),
            __phantom: PhantomData,
        }
    }

    fn meta(&self) -> Option<&ResultSetMeta> {
        self.conn.conn_ref().get_pending_result()
    }

    /// Returns `true` if this query result may contain rows.
    ///
    /// If `false` then there is no rows possible (e.g. result of an UPDATE query).
    fn has_rows(&self) -> bool {
        !matches!(self.meta(), None)
    }

    /// `true` if there is no more rows nor result sets in this query.
    ///
    /// One could use it to check if there is more than one result set in this query result.
    pub fn is_empty(&self) -> bool {
        !self.has_rows()
    }

    fn make_empty(&mut self) {
        self.conn.conn_mut().set_pending_result(None);
    }

    async fn get_row_raw(&mut self) -> Result<Option<Vec<u8>>> {
        if self.is_empty() {
            return Ok(None);
        }

        let packet: Vec<u8> = self.conn.conn_mut().read_packet().await?;

        if P::is_last_result_set_packet(self.conn.conn_ref().capabilities(), &packet) {
            if self.more_results_exists() {
                self.conn.conn_mut().sync_seq_id();
                self.conn.conn_mut().read_result_set::<P>().await?;
                Ok(None)
            } else {
                self.make_empty();
                Ok(None)
            }
        } else {
            Ok(Some(packet))
        }
    }

    /// Returns next row, if any.
    ///
    /// Requires that `self.meta()` is not `None`.
    pub(crate) async fn get_row(&mut self) -> Result<Option<Row>> {
        let packet = self.get_row_raw().await?;
        if let Some(packet) = packet {
            let columns = self.meta().expect("must be here").columns();
            let row = P::read_result_set_row(&packet, columns.clone())?;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    /// Last insert id, if any.
    pub fn last_insert_id(&self) -> Option<u64> {
        self.conn.conn_ref().last_insert_id()
    }

    /// Number of affected rows, as reported by the server, or `0`.
    pub fn affected_rows(&self) -> u64 {
        self.conn.conn_ref().affected_rows()
    }

    /// Text information, as reported by the server, or an empty string.
    pub fn info(&self) -> Cow<'_, str> {
        self.conn.conn_ref().info()
    }

    /// Number of warnings, as reported by the server, or `0`.
    pub fn warnings(&self) -> u16 {
        self.conn.conn_ref().get_warnings()
    }

    /// Returns `true` if the `SERVER_MORE_RESULTS_EXISTS` flag is contained in status flags
    /// of the connection.
    fn more_results_exists(&self) -> bool {
        self.conn
            .conn_ref()
            .status()
            .contains(StatusFlags::SERVER_MORE_RESULTS_EXISTS)
    }

    /// Returns a future that collects result set of this query result.
    ///
    /// It is parametrized by `R` and internally calls `R::from_row(Row)` on each row.
    ///
    /// It will collect rows up to a neares result set boundary. This means that you should call
    /// `collect` as many times as result sets in your query result. For example query
    /// `SELECT 'foo'; SELECT 'foo', 'bar';` will produce `QueryResult` with two result sets in it.
    /// One can use `QueryResult::is_empty` to make sure that there is no more result sets.
    ///
    /// # Panic
    ///
    /// It'll panic if any row isn't convertible to `R` (i.e. programmer error or unknown schema).
    /// * In case of programmer error see [`FromRow`] docs;
    /// * In case of unknown schema use [`QueryResult::try_collect`].
    pub async fn collect<R>(&mut self) -> Result<Vec<R>>
    where
        R: FromRow + Send + 'static,
    {
        self.reduce(Vec::new(), |mut acc, row| {
            acc.push(FromRow::from_row(row));
            acc
        })
        .await
    }

    /// Returns a future that collects result set of this query result.
    ///
    /// It works the same way as [`QueryResult::collect`] but won't panic if row isn't convertible
    /// to `R`.
    pub async fn try_collect<R>(&mut self) -> Result<Vec<StdResult<R, FromRowError>>>
    where
        R: FromRow + Send + 'static,
    {
        self.reduce(Vec::new(), |mut acc, row| {
            acc.push(FromRow::from_row_opt(row));
            acc
        })
        .await
    }

    /// Returns a future that collects the current result set of this query result and drops
    /// everything else.
    ///
    /// # Panic
    ///
    /// It'll panic if any row isn't convertible to `R` (i.e. programmer error or unknown schema).
    /// * In case of programmer error see `FromRow` docs;
    /// * In case of unknown schema use [`QueryResult::try_collect`].
    pub async fn collect_and_drop<R>(mut self) -> Result<Vec<R>>
    where
        R: FromRow + Send + 'static,
    {
        let output = self.collect::<R>().await?;
        self.drop_result().await?;
        Ok(output)
    }

    /// Returns a future that collects the current result set of this query result and drops
    /// everything else.
    ///
    /// It works the same way as [`QueryResult::collect_and_drop`] but won't panic if row isn't
    /// convertible to `R`.
    pub async fn try_collect_and_drop<R>(mut self) -> Result<Vec<StdResult<R, FromRowError>>>
    where
        R: FromRow + Send + 'static,
    {
        let output = self.try_collect().await?;
        self.drop_result().await?;
        Ok(output)
    }

    /// Returns a future that will execute `fun` on every row of the current result set.
    ///
    /// It will stop on the nearest result set boundary (see `QueryResult::collect` docs).
    pub async fn for_each<F>(&mut self, mut fun: F) -> Result<()>
    where
        F: FnMut(Row),
    {
        if self.is_empty() {
            Ok(())
        } else {
            loop {
                let row = self.get_row().await?;
                if let Some(row) = row {
                    fun(row);
                } else {
                    break Ok(());
                }
            }
        }
    }

    /// Returns a future that will execute `fun` on every row of the current result set and drop
    /// everything else.
    pub async fn for_each_and_drop<F>(mut self, fun: F) -> Result<()>
    where
        F: FnMut(Row),
    {
        self.for_each(fun).await?;
        self.drop_result().await?;
        Ok(())
    }

    /// Returns a future that will map every row of the current result set to `U` using `fun`.
    ///
    /// It will stop on the nearest result set boundary (see `QueryResult::collect` docs).
    pub async fn map<F, U>(&mut self, mut fun: F) -> Result<Vec<U>>
    where
        F: FnMut(Row) -> U,
    {
        if self.is_empty() {
            Ok(Vec::new())
        } else {
            let mut rows = Vec::new();
            loop {
                let row = self.get_row().await?;
                if let Some(row) = row {
                    rows.push(fun(row));
                } else {
                    break Ok(rows);
                }
            }
        }
    }

    /// Returns a future that will map every row of the current result set to `U` using `fun`
    /// and drop everything else.
    pub async fn map_and_drop<F, U>(mut self, fun: F) -> Result<Vec<U>>
    where
        F: FnMut(Row) -> U,
    {
        let rows = self.map(fun).await?;
        self.drop_result().await?;
        Ok(rows)
    }

    /// Returns a future that will reduce rows of the current result set to `U` using `fun`.
    ///
    /// It will stop on the nearest result set boundary (see `QueryResult::collect` docs).
    pub async fn reduce<T, F, U>(&mut self, init: U, mut fun: F) -> Result<U>
    where
        F: FnMut(U, T) -> U,
        T: FromRow + Send + 'static,
    {
        if self.is_empty() {
            Ok(init)
        } else {
            let mut acc = init;
            loop {
                let row = self.get_row().await?;
                if let Some(row) = row {
                    acc = fun(acc, crate::from_row(row));
                } else {
                    break Ok(acc);
                }
            }
        }
    }

    /// Returns a future that will reduce rows of the current result set to `U` using `fun` and drop
    /// everything else.
    pub async fn reduce_and_drop<T, F, U>(mut self, init: U, fun: F) -> Result<U>
    where
        F: FnMut(U, T) -> U,
        T: FromRow + Send + 'static,
    {
        let acc = self.reduce(init, fun).await?;
        self.drop_result().await?;
        Ok(acc)
    }

    /// Returns a future that will drop this query result.
    pub async fn drop_result(mut self) -> Result<()> {
        loop {
            if !self.has_rows() {
                self.make_empty();
                if self.more_results_exists() {
                    self.conn.conn_mut().read_result_set::<P>().await?;
                } else {
                    break;
                }
            } else {
                self.get_row_raw().await?;
            }
        }

        Ok(())
    }

    /// Returns a reference to a columns list of this query result.
    ///
    /// Empty list means, that this result set was never meant to contain rows.
    pub fn columns_ref(&self) -> &[Column] {
        self.meta()
            .map(|meta| &***meta.columns())
            .unwrap_or_default()
    }

    /// Returns a copy of a columns list of this query result.
    pub fn columns(&self) -> Option<Arc<Vec<Column>>> {
        self.meta().map(|meta| meta.columns().clone())
    }
}

impl crate::Conn {
    /// Will read result set and write pending result into `self` (if any).
    pub(crate) async fn read_result_set<P>(&mut self) -> Result<()>
    where
        P: Protocol,
    {
        let packet = self.read_packet().await?;

        match packet.get(0) {
            Some(0x00) => self.set_pending_result(None),
            Some(0xFB) => self.handle_local_infile(&*packet).await?,
            _ => self.handle_result_set::<P>(&*packet).await?,
        }

        Ok(())
    }

    /// Will handle local infile packet.
    pub(crate) async fn handle_local_infile(&mut self, packet: &[u8]) -> Result<()> {
        let local_infile = parse_local_infile_packet(&*packet)?;
        let (local_infile, handler) = match self.opts().local_infile_handler() {
            Some(handler) => ((local_infile.into_owned(), handler)),
            None => return Err(DriverError::NoLocalInfileHandler.into()),
        };
        let mut reader = handler.handle(local_infile.file_name_ref()).await?;

        let mut buf = [0; 4096];
        loop {
            let read = reader.read(&mut buf[..]).await?;
            self.write_packet(&buf[..read]).await?;

            if read == 0 {
                break;
            }
        }

        self.read_packet().await?;
        self.set_pending_result(None);
        Ok(())
    }

    /// Helper that handles result set packet.
    pub(crate) async fn handle_result_set<P>(&mut self, mut packet: &[u8]) -> Result<()>
    where
        P: Protocol,
    {
        let column_count = packet.read_lenenc_int()?;
        let columns = self.read_column_defs(column_count as usize).await?;

        if column_count > 0 {
            let columns = Arc::new(columns);
            let meta = P::result_set_meta(columns.clone());
            self.set_pending_result(Some(meta.clone()));
        } else {
            self.set_pending_result(None);
        }

        Ok(())
    }
}
