//! Support for `LOAD DATA LOCAL INFILE` statements
//!
//! This MySQL feature allows the client to send a local file to the server, which is then
//! loaded into a table. This should be faster than sending the data row-by-row.
//!
//! # Example
//! ```rust,no_run
//! use sqlx::mysql::infile::MySqlPoolInfileExt;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), sqlx::Error> {
//!     let pool = sqlx::mysql::MySqlPool::connect("mysql://root:password@localhost:3306/sqlx").await?;
//!
//!     let res = {
//!         let mut stream = pool
//!             .load_local_infile("LOAD DATA LOCAL INFILE 'dummy' INTO TABLE testje")
//!             .await?;
//!         stream.send(b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10").await?;
//!         stream.finish().await?
//!     };
//!     println!("{}", res); // 10
//!
//!     Ok(())
//! }
//! ```

use std::ops::DerefMut;

use crate::error::Error;
use crate::protocol::response::LocalInfilePacket;
use crate::protocol::text::Query;
use crate::sqlx_core::net::Socket;
use futures_core::future::BoxFuture;
use sqlx_core::pool::{Pool, PoolConnection};

use crate::{MySql, MySqlConnection};

/// Extension of the [`Executor`][`crate::executor::Executor`] trait with support for `LOAD DATA LOCAL INFILE` statements.
pub trait MySqlPoolInfileExt {
    /// Execute the query using the given handler.
    ///
    /// See the module documentation for an example.
    fn load_data_infile<'a>(
        &'a self,
        statement: &'a str,
    ) -> BoxFuture<'a, Result<MySqlLocalInfile<PoolConnection<MySql>>, Error>>;
}

impl MySqlPoolInfileExt for Pool<MySql> {
    fn load_local_infile<'a>(
        &'a self,
        statement: &'a str,
    ) -> BoxFuture<'a, Result<MySqlLocalInfile<PoolConnection<MySql>>, Error>> {
        Box::pin(async { MySqlLocalInfile::begin(self.acquire().await?, statement).await })
    }
}

const MAX_MYSQL_PACKET_SIZE: usize = (1 << 24) - 2;

impl MySqlConnection {
    pub async fn load_local_infile(
        &mut self,
        statement: &str,
    ) -> Result<MySqlLocalInfile<&mut Self>, Error> {
        MySqlLocalInfile::begin(self, statement).await
    }
}

pub struct MySqlLocalInfile<C: DerefMut<Target = MySqlConnection>> {
    conn: C,
    filename: Vec<u8>,
    buf: Vec<u8>,
}

impl<C: DerefMut<Target = MySqlConnection>> MySqlLocalInfile<C> {
    async fn begin(mut conn: C, statement: &str) -> Result<Self, Error> {
        conn.stream.wait_until_ready().await?;
        conn.stream.send_packet(Query(statement)).await?;

        let packet = conn.stream.recv_packet().await?;
        let packet: LocalInfilePacket = packet.decode()?;
        let filename = packet.filename;

        let mut buf = Vec::with_capacity(MAX_MYSQL_PACKET_SIZE);
        buf.extend_from_slice(&[0; 4]);

        Ok(Self {
            conn,
            filename,
            buf,
        })
    }

    pub fn get_filename(&self) -> &[u8] {
        &self.filename
    }

    /// Write data to the stream.
    ///
    /// The data is buffered and send to the server in packets of at most 16MB. The data is automatically flushed when the buffer is full.
    pub async fn write(&mut self, buf: &[u8]) -> Result<(), Error> {
        let mut right = buf;
        while !right.is_empty() {
            let (left, right_) = right.split_at(std::cmp::min(MAX_MYSQL_PACKET_SIZE, right.len()));
            self.buf.extend_from_slice(left);
            if self.buf.len() >= MAX_MYSQL_PACKET_SIZE + 4 {
                assert_eq!(self.buf.len(), MAX_MYSQL_PACKET_SIZE + 4);
                self.drain_packet(MAX_MYSQL_PACKET_SIZE).await?;
                assert!(self.buf.is_empty());
                self.buf.extend_from_slice(&[0; 4]);
            }
            right = right_;
        }
        Ok(())
    }

    /// Flush the stream.
    pub async fn flush(&mut self) -> Result<(), Error> {
        if self.buf.len() > 4 {
            // Cannot have multiple packets in buffer, as they would have been drained by write() already
            assert!(self.buf.len() <= MAX_MYSQL_PACKET_SIZE + 4);
            self.drain_packet(self.buf.len() - 4).await?;
        }
        Ok(())
    }

    async fn drain_packet(&mut self, len: usize) -> Result<(), Error> {
        self.buf[0..3].copy_from_slice(&(len as u32).to_le_bytes()[..3]);
        self.buf[3] = self.conn.stream.sequence_id;
        self.conn
            .stream
            .socket
            .socket_mut()
            .write(&self.buf[..len + 4])
            .await?;
        self.buf.drain(..len + 4);
        self.conn.stream.sequence_id = self.conn.stream.sequence_id.wrapping_add(1);
        Ok(())
    }

    /// Finish sending the LOCAL INFILE data to the server.
    ///
    /// This must always be called after you're done writing the data.
    pub async fn finish(mut self) -> Result<u64, Error> {
        self.flush().await?;
        self.conn.stream.send_empty_response().await?;
        let packet = self.conn.stream.recv_packet().await?;
        let packet = packet.ok()?;
        Ok(packet.affected_rows)
    }
}
