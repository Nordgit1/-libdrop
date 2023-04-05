use std::{
    collections::{hash_map::Entry, HashMap},
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::{mpsc::UnboundedSender, Mutex};
use uuid::Uuid;

use crate::{
    file::FileId,
    service::State,
    ws::{client::ClientReq, server::ServerReq},
    Error, Transfer,
};

#[derive(Clone)]
pub enum TransferConnection {
    Client(UnboundedSender<ClientReq>),
    Server(UnboundedSender<ServerReq>),
}

pub struct TransferState {
    pub(crate) xfer: Transfer,
    pub(crate) connection: TransferConnection,
    // Used for mapping directories inside the destination
    dir_mappings: HashMap<PathBuf, String>,
}

/// Transfer manager is responsible for keeping track of all ongoing or pending
/// transfers and their status
pub(crate) struct TransferManager {
    transfers: HashMap<Uuid, TransferState>,
    storage: Arc<Mutex<drop_storage::Storage>>,
}

impl TransferState {
    fn new(xfer: Transfer, connection: TransferConnection) -> Self {
        Self {
            xfer,
            connection,
            dir_mappings: HashMap::new(),
        }
    }
}

impl TransferManager {
    pub(crate) fn new(storage: Arc<Mutex<drop_storage::Storage>>) -> TransferManager {
        TransferManager {
            transfers: HashMap::new(),
            storage,
        }
    }

    // TODO: add `since`
    pub async fn get_state(&self) -> Result<Vec<drop_storage::SerializedTransferStorage>, Error> {
        Ok(
            match self
                .storage
                .lock()
                .await
                .get_serialized_transfer_data()
                .await
            {
                Ok(it) => it,
                Err(_err) => {
                    return Err(Error::BadTransfer); // TODO
                }
            },
        )
    }
    /// Get ALL of the ongoing file transfers for a given transfer ID
    /// returns None if a transfer does not exist
    pub(crate) fn get_transfer_files(&self, transfer_id: Uuid) -> Option<Vec<FileId>> {
        let state = self.transfers.get(&transfer_id)?;

        let ids = state
            .xfer
            .flat_file_list()
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        Some(ids)
    }

    /// Cancel ALL of the ongoing file transfers for a given transfer ID    
    pub(crate) fn cancel_transfer(&mut self, transfer_id: Uuid) -> Result<(), Error> {
        self.transfers
            .remove(&transfer_id)
            .ok_or(Error::BadTransfer)?;

        Ok(())
    }

    pub(crate) async fn insert_transfer(
        &mut self,
        xfer: Transfer,
        connection: TransferConnection,
    ) -> crate::Result<()> {
        let peer = xfer.peer();
        let id = xfer.id();

        match connection {
            TransferConnection::Client(_) => {
                let mut lock = self.storage.lock().await;

                match lock.get_outgoing_transfer(&id.to_string()).await {
                    Ok(_) => return Err(Error::BadTransfer),
                    Err(err) => match err {
                        drop_storage::error::Error::InternalError(_) => {
                            return Err(Error::StorageError)
                        }
                        drop_storage::error::Error::DBError(_) => return Err(Error::StorageError),
                        drop_storage::error::Error::RowNotFound => {
                            lock.insert_outgoing_transfer(&id.to_string(), &peer.to_string())
                                .await
                                .map_err(|_| Error::StorageError)?;
                        }
                    },
                }
            }
            TransferConnection::Server(_) => {
                let mut lock = self.storage.lock().await;

                match lock.get_incoming_transfer(&xfer.id().to_string()).await {
                    Ok(_) => return Err(Error::BadTransfer),
                    Err(err) => match err {
                        drop_storage::error::Error::InternalError(_) => {
                            return Err(Error::StorageError)
                        }
                        drop_storage::error::Error::DBError(_) => return Err(Error::StorageError),
                        drop_storage::error::Error::RowNotFound => {
                            lock.insert_incoming_transfer(&id.to_string(), &peer.to_string())
                                .await
                                .map_err(|_| Error::StorageError)?;
                        }
                    },
                }
            }
        }

        match self.transfers.entry(xfer.id()) {
            Entry::Occupied(_) => Err(Error::BadTransferState),
            Entry::Vacant(entry) => {
                entry.insert(TransferState::new(xfer, connection));
                Ok(())
            }
        }
    }

    pub(crate) fn transfer(&self, id: &Uuid) -> Option<&Transfer> {
        self.transfers.get(id).map(|state| &state.xfer)
    }

    pub(crate) fn connection(&self, id: Uuid) -> Option<&TransferConnection> {
        self.transfers.get(&id).map(|state| &state.connection)
    }

    pub(crate) fn apply_dir_mapping(
        &mut self,
        id: Uuid,
        dest_dir: &Path,
        file_id: &FileId,
    ) -> crate::Result<PathBuf> {
        let state = self
            .transfers
            .get_mut(&id)
            .ok_or(crate::Error::BadTransfer)?;

        let mut iter = file_id.iter().map(crate::utils::normalize_filename);

        let probe = iter.next().ok_or(crate::Error::BadPath)?;
        let next = iter.next();

        let mapped = match next {
            Some(next) => {
                // Check if dir exists and is known to us
                let name = match state.dir_mappings.entry(dest_dir.join(probe)) {
                    // Dir is known, reuse
                    Entry::Occupied(occ) => occ.get().clone(),
                    // Dir in new, check if there is name conflict and add to known
                    Entry::Vacant(vacc) => {
                        let mapped = crate::utils::map_path_if_exists(vacc.key())?;
                        vacc.insert(
                            mapped
                                .file_name()
                                .ok_or(crate::Error::BadPath)?
                                .to_string_lossy()
                                .to_string(),
                        )
                        .clone()
                    }
                };

                [name, next].into_iter().chain(iter).collect()
            }
            None => {
                // Ordinary file
                probe.into()
            }
        };

        Ok(mapped)
    }
}

pub(crate) struct TransferGuard {
    state: ManuallyDrop<Arc<State>>,
    id: Uuid,
}

impl TransferGuard {
    pub(crate) fn new(state: Arc<State>, id: Uuid) -> Self {
        Self {
            state: ManuallyDrop::new(state),
            id,
        }
    }
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        let state = unsafe { ManuallyDrop::take(&mut self.state) };
        let id = self.id;

        tokio::spawn(async move {
            let mut lock = state.transfer_manager.lock().await;
            let _ = lock.cancel_transfer(id);
        });
    }
}
