use rusqlite::{params, types::FromSql, Connection, OptionalExtension, ToSql};
use uuid::Uuid;

use crate::QueryResult;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum TransferState {
    New = 0,
    Active = 1,
    Canceled = 2,
}

impl ToSql for TransferState {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok((*self as u8).into())
    }
}

impl FromSql for TransferState {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_i64()? {
            0 => Ok(Self::New),
            1 => Ok(Self::Active),
            2 => Ok(Self::Canceled),
            x => Err(rusqlite::types::FromSqlError::OutOfRange(x)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum FileState {
    Alive = 0,
    Rejected = 1,
}

impl ToSql for FileState {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok((*self as u8).into())
    }
}

impl FromSql for FileState {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_i64()? {
            0 => Ok(Self::Alive),
            1 => Ok(Self::Rejected),
            x => Err(rusqlite::types::FromSqlError::OutOfRange(x)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Transfer {
    pub remote_state: TransferState,
    pub local_state: TransferState,
    pub is_outgoing: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct File {
    pub remote_state: FileState,
    pub local_state: FileState,
}

#[derive(Debug, Clone)]
pub struct FileInFilght {
    pub base_dir: String,
    pub file_id: String,
}

pub(super) fn insert_transfer(
    conn: &Connection,
    transfer_id: Uuid,
    is_incoming: bool,
) -> super::Result<()> {
    let tid = transfer_id.to_string();

    let sync_id: i64 = conn.query_row(
        "INSERT INTO sync_transfer (transfer_id, local_state, remote_state) VALUES (?1, ?2, ?2) \
         RETURNING sync_id",
        params![tid, TransferState::New],
        |r| r.get(0),
    )?;

    if is_incoming {
        conn.execute(
            r#"
                INSERT INTO sync_incoming_files (sync_id, path_id, local_state, remote_state)
                SELECT st.sync_id, ip.id, ?2, ?2
                FROM transfers t
                INNER JOIN sync_transfer st ON t.id = st.transfer_id
                INNER JOIN incoming_paths ip ON t.id = ip.transfer_id
                WHERE st.sync_id = ?1
                "#,
            params![sync_id, FileState::Alive],
        )?;
    } else {
        conn.execute(
            r#"
                INSERT INTO sync_outgoing_files (sync_id, path_id, local_state, remote_state)
                SELECT st.sync_id, ip.id, ?2, ?2
                FROM transfers t
                INNER JOIN sync_transfer st ON t.id = st.transfer_id
                INNER JOIN outgoing_paths ip ON t.id = ip.transfer_id
                WHERE st.sync_id = ?1
                "#,
            params![sync_id, FileState::Alive],
        )?;
    }

    Ok(())
}

pub(super) fn transfer_state(
    conn: &Connection,
    transfer_id: Uuid,
) -> super::Result<Option<Transfer>> {
    let tid = transfer_id.to_string();

    let res = conn
        .query_row(
            r#"
            SELECT st.local_state, st.remote_state, t.is_outgoing
            FROM sync_transfer st
            INNER JOIN transfers t ON t.id = st.transfer_id
            WHERE st.transfer_id = ?1
            "#,
            params![tid],
            |r| {
                Ok(Transfer {
                    remote_state: r.get(1)?,
                    local_state: r.get(0)?,
                    is_outgoing: r.get(2)?,
                })
            },
        )
        .optional()?;

    Ok(res)
}

pub(super) fn transfer_set_remote_state(
    conn: &Connection,
    transfer_id: Uuid,
    state: TransferState,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        "UPDATE sync_transfer SET remote_state = ?2 WHERE transfer_id = ?1",
        params![tid, state],
    )?;

    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn transfer_set_local_state(
    conn: &Connection,
    transfer_id: Uuid,
    state: TransferState,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        "UPDATE sync_transfer SET local_state = ?2 WHERE transfer_id = ?1",
        params![tid, state],
    )?;

    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn transfer_clear(conn: &Connection, transfer_id: Uuid) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();
    let count = conn.execute(
        "DELETE FROM sync_transfer WHERE transfer_id = ?1",
        params![tid],
    )?;
    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn outgoing_file_state(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
) -> super::Result<Option<File>> {
    let tid = transfer_id.to_string();

    let res = conn
        .query_row(
            r#"
            SELECT sof.local_state, sof.remote_state
            FROM sync_outgoing_files sof
            INNER JOIN sync_transfer st USING(sync_id)
            INNER JOIN transfers t ON t.id = st.transfer_id
            INNER JOIN outgoing_paths op ON op.id = sof.path_id
            WHERE st.transfer_id = ?1 AND op.path_hash = ?2
            "#,
            params![tid, file_id],
            |r| {
                Ok(File {
                    remote_state: r.get(1)?,
                    local_state: r.get(0)?,
                })
            },
        )
        .optional()?;

    Ok(res)
}

pub(super) fn outgoing_file_set_local_state(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
    state: FileState,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        r#"
        UPDATE sync_outgoing_files sof
        SET sof.local_state = ?3
        WHERE sof.sync_id, sof.path_id IN (
            SELECT st.sync_id, op.id
            FROM sync_transfer st
            INNER JOIN transfers t ON t.id = st.transfer_id
            INNER JOIN outgoing_paths op ON t.id = op.transfer_id
            WHERE st.transfer_id = ?1 AND op.path_hash = ?2
        )
        "#,
        params![tid, file_id, state],
    )?;
    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn outgoing_file_set_remote_state(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
    state: FileState,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        r#"
        UPDATE sync_outgoing_files sof
        SET sof.remote_state = ?3
        WHERE sof.sync_id, sof.path_id IN (
            SELECT st.sync_id, op.id
            FROM sync_transfer st
            INNER JOIN transfers t ON t.id = st.transfer_id
            INNER JOIN outgoing_paths op ON t.id = op.transfer_id
            WHERE st.transfer_id = ?1 AND op.path_hash = ?2
        )
        "#,
        params![tid, file_id, state],
    )?;
    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn outgoing_files_to_reject(
    conn: &Connection,
    transfer_id: Uuid,
) -> super::Result<Vec<String>> {
    let tid = transfer_id.to_string();

    let res = conn
        .prepare(
            r#"
        SELECT sof.path_id
        FROM sync_outgoing_files sof
        INNER JOIN sync_transfer st USING(sync_id)
        WHERE st.transfer_id = ?1
            AND sof.local_state = ?2
            AND NOT sof.remote_state = sof.local_state
        "#,
        )?
        .query_map(params![tid, FileState::Rejected], |r| r.get(0))?
        .collect::<QueryResult<_>>()?;

    Ok(res)
}

pub(super) fn incoming_files_in_flight(
    conn: &Connection,
    transfer_id: Uuid,
) -> super::Result<Vec<FileInFilght>> {
    let tid = transfer_id.to_string();

    let res = conn
        .prepare(
            r#"
        SELECT sifi.base_dir, sif.path_id
        FROM sync_incoming_files sif
        INNER JOIN sync_incoming_files_inflight sifi USING(sync_id, path_id)
        INNER JOIN sync_transfer st USING(sync_id)
        INNER JOIN transfers t ON t.id = st.transfer_id
        WHERE st.transfer_id = ?1 AND sif.local_state = ?2
        "#,
        )?
        .query_map(params![tid, FileState::Alive], |r| {
            Ok(FileInFilght {
                base_dir: r.get(0)?,
                file_id: r.get(1)?,
            })
        })?
        .collect::<QueryResult<_>>()?;

    Ok(res)
}

pub(super) fn incoming_files_to_reject(
    conn: &Connection,
    transfer_id: Uuid,
) -> super::Result<Vec<String>> {
    let tid = transfer_id.to_string();

    let res = conn
        .prepare(
            r#"
        SELECT sif.path_id
        FROM sync_incoming_files sif
        INNER JOIN sync_transfer st USING(sync_id)
        WHERE st.transfer_id = ?1
            AND sif.local_state = ?2
            AND NOT sif.remote_state = sif.local_state
        "#,
        )?
        .query_map(params![tid, FileState::Rejected], |r| r.get(0))?
        .collect::<QueryResult<_>>()?;

    Ok(res)
}

pub(super) fn stop_incoming_file(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        r#"
        DELETE FROM sync_incoming_files_inflight sifi
        WHERE sifi.sync_id, sifi.path_id IN (
            SELECT st.sync_id, ip.id
            FROM sync_transfer st
            INNER JOIN transfers t ON t.id = st.transfer_id
            INNER JOIN incoming_paths ip ON t.id = ip.transfer_id
            WHERE st.transfer_id = ?1 AND ip.path_hash = ?2
        )
        "#,
        params![tid, file_id],
    )?;

    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn start_incoming_file(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
    base_dir: &str,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        r#"
        INSERT INTO sync_incoming_files_inflight (sync_id, path_id, base_dir)
        SELECT sif.sync_id, sif.path_id, ?3
        FROM sync_incoming_files sif
        INNER JOIN sync_transfer st USING(sync_id)
        INNER JOIN incoming_paths ip ON ip.id = sif.path_id
        WHERE st.transfer_id = ?1 AND ip.path_hash = ?2
        "#,
        params![tid, file_id, base_dir],
    )?;

    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn incoming_file_state(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
) -> super::Result<Option<File>> {
    let tid = transfer_id.to_string();

    let res = conn
        .query_row(
            r#"
            SELECT sif.local_state, sif.remote_state
            FROM sync_incoming_files sif
            INNER JOIN sync_transfer st USING(sync_id)
            INNER JOIN transfers t ON t.id = st.transfer_id
            INNER JOIN incoming_paths ip ON ip.id = sif.path_id
            WHERE st.transfer_id = ?1 AND ip.path_hash = ?2
            "#,
            params![tid, file_id],
            |r| {
                Ok(File {
                    remote_state: r.get(1)?,
                    local_state: r.get(0)?,
                })
            },
        )
        .optional()?;

    Ok(res)
}

pub(super) fn incoming_file_set_local_state(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
    state: FileState,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        r#"
        UPDATE sync_incoming_files sif
        SET sif.local_state = ?3
        WHERE sif.sync_id, sif.path_id IN (
            SELECT st.sync_id, ip.id
            FROM sync_transfer st
            INNER JOIN transfers t ON t.id = st.transfer_id
            INNER JOIN incoming_paths ip ON t.id = ip.transfer_id
            WHERE st.transfer_id = ?1 AND ip.path_hash = ?2
        )
        "#,
        params![tid, file_id, state],
    )?;
    Ok(if count > 0 { Some(()) } else { None })
}

pub(super) fn incoming_file_set_remote_state(
    conn: &Connection,
    transfer_id: Uuid,
    file_id: &str,
    state: FileState,
) -> super::Result<Option<()>> {
    let tid = transfer_id.to_string();

    let count = conn.execute(
        r#"
        UPDATE sync_incoming_files sif
        SET sif.remote_state = ?3
        WHERE sif.sync_id, sif.path_id IN (
            SELECT st.sync_id, ip.id
            FROM sync_transfer st
            INNER JOIN transfers t ON t.id = st.transfer_id
            INNER JOIN incoming_paths ip ON t.id = ip.transfer_id
            WHERE st.transfer_id = ?1 AND ip.path_hash = ?2
        )
        "#,
        params![tid, file_id, state],
    )?;
    Ok(if count > 0 { Some(()) } else { None })
}
