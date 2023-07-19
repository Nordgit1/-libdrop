mod handler;
mod v2;
mod v4;
mod v5;

use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::Context;
use drop_auth::Nonce;
use drop_storage::sync;
use futures::{SinkExt, StreamExt};
use handler::{Downloader, HandlerInit, HandlerLoop, Request};
use hyper::StatusCode;
use slog::{debug, error, info, warn, Logger};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver},
        Mutex,
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use warp::{
    ws::{Message, WebSocket},
    Filter,
};

use super::events::FileEventTx;
use crate::{
    auth,
    error::ResultExt,
    event::DownloadSuccess,
    file::{self, FileSubPath, FileToRecv},
    manager::IncomingState,
    protocol,
    quarantine::PathExt,
    service::State,
    transfer::{IncomingTransfer, Transfer},
    utils::Hidden,
    ws::Pinger,
    Error, Event, File, FileId,
};

const MAX_FILENAME_LENGTH: usize = 255;
const MAX_FILE_SUFFIX_LEN: usize = 5; // Assume that the suffix will fit into 5 characters e.g.
                                      // `<filename>(999).<ext>`
const REPORT_PROGRESS_THRESHOLD: u64 = 1024 * 64;

pub enum ServerReq {
    Download { task: Box<FileXferTask> },
    Cancel { file: FileId },
    Reject { file: FileId },
}

pub struct FileXferTask {
    pub file: FileToRecv,
    pub xfer: Arc<IncomingTransfer>,
    pub base_dir: Hidden<PathBuf>,
}

struct TmpFileState {
    meta: fs::Metadata,
    csum: [u8; 32],
}

struct StreamCtx<'a> {
    logger: &'a Logger,
    state: &'a State,
    tmp_loc: &'a Hidden<PathBuf>,
    stream: &'a mut UnboundedReceiver<Vec<u8>>,
    events: &'a FileEventTx,
}

pub(crate) fn start(
    addr: IpAddr,
    stop: CancellationToken,
    state: Arc<State>,
    auth: Arc<auth::Context>,
    logger: Logger,
) -> crate::Result<JoinHandle<()>> {
    let nonce_store = Arc::new(Mutex::new(HashMap::new()));

    #[derive(Debug)]
    struct MissingAuth(SocketAddr);
    impl warp::reject::Reject for MissingAuth {}

    #[derive(Debug)]
    struct Unauthrorized;
    impl warp::reject::Reject for Unauthrorized {}

    #[derive(Debug)]
    struct ToManyReqs;
    impl warp::reject::Reject for ToManyReqs {}

    async fn handle_rejection(
        nonces: &Mutex<HashMap<SocketAddr, Nonce>>,
        err: warp::Rejection,
    ) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
        if let Some(MissingAuth(peer)) = err.find() {
            let nonce = Nonce::generate();
            let value = drop_auth::http::WWWAuthenticate::new(nonce);

            nonces.lock().await.insert(*peer, nonce);

            Ok(Box::new(warp::reply::with_header(
                StatusCode::UNAUTHORIZED,
                drop_auth::http::WWWAuthenticate::KEY,
                value.to_string(),
            )))
        } else if let Some(Unauthrorized) = err.find() {
            Ok(Box::new(StatusCode::UNAUTHORIZED))
        } else if let Some(ToManyReqs) = err.find() {
            Ok(Box::new(StatusCode::TOO_MANY_REQUESTS))
        } else {
            Err(err)
        }
    }

    let service = {
        let stop = stop.clone();
        let logger = logger.clone();
        let nonces = nonce_store.clone();
        let rate_limiter = Arc::new(governor::RateLimiter::dashmap(governor::Quota::per_second(
            state
                .config
                .max_reqs_per_sec
                .try_into()
                .map_err(|_| crate::Error::InvalidArgument)?,
        )));

        warp::path("drop")
            .and(warp::path::param().and_then(|version: String| async move {
                version
                    .parse::<protocol::Version>()
                    .map_err(|_| warp::reject::not_found())
            }))
            .and(
                warp::filters::addr::remote().and_then(move |peer: Option<SocketAddr>| {
                    let peer = peer.expect("Transport should use IP addresses");
                    let check = rate_limiter.check_key(&peer.ip());

                    async move {
                        match check {
                            Ok(_) => Ok(peer),
                            Err(_) => Err(warp::reject::custom(ToManyReqs)),
                        }
                    }
                }),
            )
            .and(warp::filters::header::optional("authorization"))
            .and_then(
                move |version: protocol::Version, peer: SocketAddr, auth_header: Option<String>| {
                    let nonces = nonces.clone();
                    let auth = auth.clone();

                    async move {
                        // Uncache the peer nonce first
                        let nonce = nonces.lock().await.remove(&peer);

                        match version {
                            protocol::Version::V1 | protocol::Version::V2 => (),
                            _ => {
                                let auth_header = auth_header
                                    .ok_or_else(|| warp::reject::custom(MissingAuth(peer)))?;

                                let nonce =
                                    nonce.ok_or_else(|| warp::reject::custom(Unauthrorized))?;

                                if !auth.authorize(peer.ip(), &auth_header, &nonce) {
                                    return Err(warp::reject::custom(Unauthrorized));
                                }
                            }
                        };

                        Ok((version, peer))
                    }
                },
            )
            .untuple_one()
            .and(warp::ws())
            .map(
                move |version: protocol::Version, peer: SocketAddr, ws: warp::ws::Ws| {
                    let state = Arc::clone(&state);
                    let stop = stop.clone();
                    let logger = logger.clone();

                    ws.on_upgrade(move |socket| async move {
                        info!(logger, "Client requested protocol version: {}", version);

                        let ctx = RunContext {
                            logger: &logger,
                            state: state.clone(),
                            socket,
                            stop: &stop,
                        };

                        match version {
                            protocol::Version::V1 => {
                                ctx.run(v2::HandlerInit::<false>::new(peer.ip(), &state, &logger))
                                    .await
                            }
                            protocol::Version::V2 => {
                                ctx.run(v2::HandlerInit::<true>::new(peer.ip(), &state, &logger))
                                    .await
                            }
                            protocol::Version::V4 => {
                                ctx.run(v4::HandlerInit::new(peer.ip(), state, &logger))
                                    .await
                            }
                            protocol::Version::V5 => {
                                ctx.run(v5::HandlerInit::new(peer.ip(), state, &logger))
                                    .await
                            }
                        }
                    })
                },
            )
            .recover(move |err| {
                let nonces = Arc::clone(&nonce_store);
                async move { handle_rejection(&nonces, err).await }
            })
    };

    let future = match warp::serve(service)
        .try_bind_with_graceful_shutdown((addr, drop_config::PORT), async move {
            stop.cancelled().await
        }) {
        Ok((socket, future)) => {
            debug!(logger, "WS server is bound to: {socket}");
            future
        }
        Err(err) => {
            // Check if this is IO error about address already in use
            if let Some(ioerr) = std::error::Error::source(&err)
                .and_then(|src| src.downcast_ref::<hyper::Error>())
                .and_then(std::error::Error::source)
                .and_then(|src| src.downcast_ref::<io::Error>())
            {
                if ioerr.kind() == io::ErrorKind::AddrInUse {
                    error!(
                        logger,
                        "Found that the address {}:{} is already used, while trying to bind the \
                         WS server: {}",
                        addr,
                        drop_config::PORT,
                        ioerr
                    );
                    return Err(Error::AddrInUse);
                }
            }

            return Err(err.into());
        }
    };

    let task = tokio::spawn(async move {
        future.await;
        debug!(logger, "WS server stopped");
    });

    Ok(task)
}

pub(crate) async fn resume(state: &State, logger: &Logger) {
    let transfers = match state.storage.incoming_transfers_to_resume() {
        Ok(transfers) => transfers,
        Err(err) => {
            error!(
                logger,
                "Failed to restore incoming pedning transfers form DB: {err}"
            );
            return;
        }
    };

    let transfers: Vec<_> = transfers
        .into_iter()
        .filter_map(|transfer| {
            let restore_transfer = || {
                let files = transfer
                    .files
                    .into_iter()
                    .map(|dbfile| {
                        FileToRecv::new(dbfile.file_id.into(), dbfile.subpath.into(), dbfile.size)
                    })
                    .collect();

                let xfer = IncomingTransfer::new_with_uuid(
                    transfer.peer.parse().context("Failed to parse peer IP")?,
                    files,
                    transfer.uuid,
                    &state.config,
                )
                .context("Failed to create transfer")?;

                let mut xstate = IncomingState {
                    xfer: Arc::new(xfer),
                    conn: None,
                    dir_mappings: Default::default(),
                };

                register_finished_paths(state, &mut xstate, logger);
                anyhow::Ok(xstate)
            };

            match restore_transfer() {
                Ok(xstate) => Some((xstate.xfer.id(), xstate)),
                Err(err) => {
                    error!(
                        logger,
                        "Failed to restore transfer {}: {err}", transfer.uuid
                    );

                    None
                }
            }
        })
        .collect();

    let mut lock = state.transfer_manager.incoming.lock().await;
    lock.extend(transfers);
}

struct RunContext<'a> {
    logger: &'a slog::Logger,
    state: Arc<State>,
    stop: &'a CancellationToken,
    socket: WebSocket,
}

impl RunContext<'_> {
    async fn run(mut self, mut handler: impl HandlerInit) {
        let recv_task = handler.recv_req(&mut self.socket);

        let xfer = tokio::select! {
            biased;

            _ = self.stop.cancelled() => {
                debug!(self.logger, "Stopping client request on shutdown");
                return;
            },
            r = recv_task => {
                match r {
                    Ok(xfer) => xfer,
                    Err(err) => {
                        error!(self.logger, "Failed to initiate transfer: {:?}", err);
                        return;
                    }
                }
            },
        };

        let xfer = match xfer.parse() {
            Ok(xfer) => {
                debug!(self.logger, "RunContext::run() called with {:?}", xfer);
                xfer
            }
            Err(err) => {
                if let Err(err) = handler.on_error(&mut self.socket, err).await {
                    error!(
                        self.logger,
                        "Failed to close connection on invalid request: {:?}", err
                    );
                }

                return;
            }
        };

        let xfer = Arc::new(xfer);

        let job = handle_client(&self.state, self.logger, self.socket, handler, xfer);

        tokio::select! {
            biased;

            _ = self.stop.cancelled() => {
                info!(self.logger, "Aborting transfer download");
            },
            _ = job => (),
        };
    }
}

async fn handle_client(
    state: &Arc<State>,
    logger: &slog::Logger,
    mut socket: WebSocket,
    mut handler: impl handler::HandlerInit,
    xfer: Arc<IncomingTransfer>,
) {
    let (req_send, mut req_rx) = mpsc::unbounded_channel();

    if let Err(err) = init_client_handler(state, &xfer, req_send, logger).await {
        error!(logger, "Failed to init trasfer: {err:?}");
        let _ = handler.on_error(&mut socket, err).await;
        return;
    }

    let mut ping = handler.pinger();

    let (send_tx, mut send_rx) = mpsc::channel(2);
    let mut handler =
        if let Some(handler) = handler.upgrade(&mut socket, send_tx, xfer.clone()).await {
            handler
        } else {
            return;
        };

    let task = async {
        loop {
            tokio::select! {
                biased;

                // API request
                req = req_rx.recv() => {
                    if let Some(req) = req {
                        handler.on_req(&mut socket, req).await?;
                    } else {
                        debug!(logger, "Stoppping server connection gracefuly");
                        socket.send(Message::close()).await?;
                        handler.on_close(false).await;
                        break;
                    }
                },
                // Message received
                recv = super::utils::recv(&mut socket, handler.recv_timeout()) => {
                    match recv? {
                        Some(msg) => {
                            if handler.on_recv(&mut socket, msg).await?.is_break() {
                                break;
                            }
                        },
                        None => break,
                    };
                },
                // Message to send down the wire
                msg = send_rx.recv() => {
                    let msg = msg.expect("Handler channel should always be open");
                    socket.send(msg).await?;
                },
                _ = ping.tick() => {
                    socket.send(Message::ping(Vec::new())).await.context("Failed to send PING message")?;
                }
            };
        }
        anyhow::Ok(())
    };

    let result = task.await;
    handler.on_stop().await;

    if let Err(err) = result {
        handler.finalize_failure(err).await;
    } else {
        let task = async {
            // Drain messages
            while socket.next().await.transpose()?.is_some() {}
            socket.close().await
        };

        if let Err(err) = task.await {
            warn!(
                logger,
                "Failed to gracefully close the client connection: {}", err
            );
        } else {
            debug!(logger, "WS client disconnected");
        }

        let _ = state
            .transfer_manager
            .outgoing
            .lock()
            .await
            .remove(&xfer.id());
        if let Err(err) = state.storage.trasnfer_sync_clear(xfer.id()) {
            warn!(
                logger,
                "Failed to clear sync state for {}: {err}",
                xfer.id()
            );
        }

        handler.finalize_success().await;
    }
}

impl FileXferTask {
    pub fn new(file: FileToRecv, xfer: Arc<IncomingTransfer>, base_dir: PathBuf) -> Self {
        Self {
            file,
            xfer,
            base_dir: Hidden(base_dir),
        }
    }

    async fn stream_file(
        &mut self,
        StreamCtx {
            logger,
            state,
            tmp_loc,
            stream,
            events,
        }: StreamCtx<'_>,
        downloader: &mut impl Downloader,
        offset: u64,
    ) -> crate::Result<PathBuf> {
        let mut out_file = match downloader.open(tmp_loc).await {
            Ok(out_file) => out_file,
            Err(err) => {
                error!(
                    logger,
                    "Could not create {tmp_loc:?} for downloading: {err}"
                );

                return Err(err);
            }
        };

        let consume_file_chunks = async {
            let mut bytes_received = offset;
            let mut last_progress = bytes_received;

            // Announce initial state of the transfer
            downloader.progress(bytes_received).await?;
            events
                .emit(crate::Event::FileDownloadProgress(
                    self.xfer.clone(),
                    self.file.id().clone(),
                    bytes_received,
                ))
                .await;

            while bytes_received < self.file.size() {
                let chunk = stream.recv().await.ok_or(crate::Error::Canceled)?;

                let chunk_size = chunk.len();
                if chunk_size as u64 + bytes_received > self.file.size() {
                    return Err(crate::Error::MismatchedSize);
                }

                out_file.write_all(&chunk)?;

                bytes_received += chunk_size as u64;

                if last_progress + REPORT_PROGRESS_THRESHOLD <= bytes_received {
                    // send progress to the caller
                    downloader.progress(bytes_received).await?;
                    events
                        .emit(crate::Event::FileDownloadProgress(
                            self.xfer.clone(),
                            self.file.id().clone(),
                            bytes_received,
                        ))
                        .await;

                    last_progress = bytes_received;
                }
            }

            if bytes_received > self.file.size() {
                return Err(crate::Error::UnexpectedData);
            }

            downloader.validate(tmp_loc).await?;
            Ok(bytes_received)
        };

        let bytes_received = match consume_file_chunks.await {
            Ok(br) => br,
            Err(err) => {
                if let Err(ioerr) = fs::remove_file(&tmp_loc.0) {
                    error!(
                        logger,
                        "Could not remove temporary file {tmp_loc:?} after failed download: {}",
                        ioerr
                    );
                }

                return Err(err);
            }
        };

        let dst = match self.place_file_into_dest(state, logger, tmp_loc).await {
            Ok(dst) => dst,
            Err(err) => {
                error!(
                    logger,
                    "Could not rename temporary file of {} after downloading: {err}",
                    self.file.id(),
                );
                return Err(err);
            }
        };

        downloader.done(bytes_received).await?;

        Ok(dst)
    }

    async fn prepare_abs_path(&self, state: &State) -> crate::Result<PathBuf> {
        let mut lock = state.transfer_manager.incoming.lock().await;

        let state = lock
            .get_mut(&self.xfer.id())
            .ok_or(crate::Error::Canceled)?;

        let mapping = state
            .dir_mappings
            .compose_final_path(&self.base_dir, self.file.subpath())?;

        drop(lock);

        Ok(self.base_dir.join(mapping))
    }

    async fn place_file_into_dest(
        &self,
        state: &State,
        logger: &Logger,
        tmp_location: &Hidden<PathBuf>,
    ) -> crate::Result<PathBuf> {
        let abs_path = self.prepare_abs_path(state).await?;
        if let Some(parent) = abs_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let dst = move_tmp_to_dst(tmp_location, Hidden(&abs_path), logger)?;

        Ok(dst)
    }

    async fn run(
        mut self,
        state: Arc<State>,
        events: Arc<FileEventTx>,
        mut downloader: impl Downloader,
        mut stream: UnboundedReceiver<Vec<u8>>,
        logger: Logger,
    ) {
        let init_res = match downloader.init(&self).await {
            Ok(init) => init,
            Err(crate::Error::Canceled) => {
                // TODO(msz): This is not 100% correct. So there are two cases when we can get
                // this error here: transfer cancel or file cancel. In case of transfer
                // cancellation, we do not want to emit any event, but in case of file
                // cancelation, we actually want.
                warn!(logger, "File cancelled on download init stage");
                return;
            }
            Err(err) => {
                events
                    .emit_force(crate::Event::FileDownloadFailed(
                        self.xfer.clone(),
                        self.file.id().clone(),
                        err,
                    ))
                    .await;
                return;
            }
        };

        match init_res {
            handler::DownloadInit::Stream {
                offset,
                tmp_location,
            } => {
                let transfer_time = Instant::now();

                state.moose.service_quality_transfer_file(
                    Ok(()),
                    drop_analytics::Phase::Start,
                    self.xfer.id().to_string(),
                    0,
                    Some(self.file.info()),
                );

                events
                    .start(crate::Event::FileDownloadStarted(
                        self.xfer.clone(),
                        self.file.id().clone(),
                        self.base_dir.to_string_lossy().to_string(),
                    ))
                    .await;

                let result = self
                    .stream_file(
                        StreamCtx {
                            logger: &logger,
                            state: &state,
                            tmp_loc: &tmp_location,
                            stream: &mut stream,
                            events: &events,
                        },
                        &mut downloader,
                        offset,
                    )
                    .await;

                state.moose.service_quality_transfer_file(
                    result.to_status(),
                    drop_analytics::Phase::End,
                    self.xfer.id().to_string(),
                    transfer_time.elapsed().as_millis() as i32,
                    Some(self.file.info()),
                );

                let event = match result {
                    Ok(dst_location) => Some(Event::FileDownloadSuccess(
                        self.xfer.clone(),
                        DownloadSuccess {
                            id: self.file.id().clone(),
                            final_path: Hidden(dst_location.into_boxed_path()),
                        },
                    )),
                    Err(crate::Error::Canceled) => None,
                    Err(err) => {
                        let _ = downloader.error(err.to_string()).await;
                        Some(Event::FileDownloadFailed(
                            self.xfer.clone(),
                            self.file.id().clone(),
                            err,
                        ))
                    }
                };

                if let Some(event) = event {
                    events.stop(event).await;
                }
            }
        }
    }
}

impl TmpFileState {
    // Blocking operation
    fn load(path: &Path) -> io::Result<Self> {
        let file = fs::File::open(path)?;

        let meta = file.metadata()?;
        let csum = file::checksum(&mut io::BufReader::new(file))?;
        Ok(TmpFileState { meta, csum })
    }
}

fn validate_tmp_location_path(tmp_location: &Hidden<PathBuf>) -> crate::Result<()> {
    let char_count = tmp_location
        .file_name()
        .expect("Cannot extract filename")
        .len();

    if char_count > MAX_FILENAME_LENGTH {
        return Err(crate::Error::FilenameTooLong);
    }

    Ok(())
}

fn move_tmp_to_dst(
    tmp_location: &Hidden<PathBuf>,
    absolute_path: Hidden<&Path>,
    logger: &Logger,
) -> crate::Result<PathBuf> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);

    let mut iter = crate::utils::filepath_variants(absolute_path.0)?;
    let dst_location = loop {
        let path = iter.next().expect("File paths iterator should never end");

        match opts.open(&path) {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(err) => {
                error!(logger, "Failed to crate destination file: {err}");
                return Err(err.into());
            }
            Ok(file) => {
                drop(file); // Close the file
                break path;
            }
        }
    };

    if let Err(err) = fs::rename(&tmp_location.0, &dst_location) {
        if let Err(err) = fs::remove_file(&dst_location) {
            warn!(
                logger,
                "Failed to remove touched destination file on move error: {err}"
            );
        }
        return Err(err.into());
    }

    if let Err(err) = dst_location.quarantine() {
        error!(logger, "Failed to quarantine downloaded file: {err}");
    }

    Ok(dst_location)
}

async fn init_client_handler(
    state: &State,
    xfer: &Arc<IncomingTransfer>,
    req_send: mpsc::UnboundedSender<ServerReq>,
    logger: &Logger,
) -> anyhow::Result<()> {
    match state.storage.transfer_sync_state(xfer.id())? {
        Some(tr_state) => {
            if let (sync::TransferState::New, _) = (tr_state.local_state, tr_state.remote_state) {
                state.storage.update_transfer_sync_states(
                    xfer.id(),
                    Some(sync::TransferState::Active),
                    Some(sync::TransferState::Active),
                )?
            }

            let existing_info = state
                .storage
                .incoming_transfer_info(xfer.id())?
                .context("Transfer missing in DB")?;
            let current_info = xfer.storage_info();

            ensure_resume_matches_existing_transfer(current_info, existing_info)
                .with_context(|| format!("Failed to validate transfer {} resume", xfer.id()))?;

            info!(
                logger,
                "Transfer {} resume. Resuming started files",
                xfer.id()
            );

            match tr_state.local_state {
                sync::TransferState::Canceled => drop(req_send),
                _ => {
                    reject_transfer_files(state, xfer, &req_send, logger);
                    resume_transfer_files(state, xfer, &req_send, logger);

                    state
                        .transfer_manager
                        .incoming
                        .lock()
                        .await
                        .get_mut(&xfer.id())
                        .expect("Missing incoming transfer data")
                        .conn = Some(req_send);
                }
            }
        }
        None => {
            if let Err(err) = state.storage.insert_transfer(&xfer.storage_info()) {
                error!(logger, "Failed to insert transfer into the DB: {err:?}");
            }

            let _ = state.transfer_manager.incoming.lock().await.insert(
                xfer.id(),
                IncomingState {
                    xfer: xfer.clone(),
                    conn: Some(req_send),
                    dir_mappings: Default::default(),
                },
            );

            state
                .event_tx
                .send(Event::RequestReceived(xfer.clone()))
                .await
                .expect("Failed to notify receiving peer!");
        }
    }

    Ok(())
}

fn ensure_resume_matches_existing_transfer(
    current_info: drop_storage::TransferInfo,
    mut existing_info: drop_storage::IncomingTransferInfo,
) -> anyhow::Result<()> {
    // Check if the transfer matches
    anyhow::ensure!(
        current_info.peer == existing_info.peer,
        "Peers do not match",
    );

    match current_info.files {
        drop_storage::types::TransferFiles::Incoming(mut files) => {
            files.sort();
            existing_info.files.sort();

            anyhow::ensure!(files == existing_info.files, "Files do not match",);
        }
        drop_storage::types::TransferFiles::Outgoing(_) => {
            panic!("Transfer handled by the server cannot be outgoing")
        }
    }
    Ok(())
}

fn resume_transfer_files(
    state: &State,
    xfer: &Arc<IncomingTransfer>,
    req_send: &mpsc::UnboundedSender<ServerReq>,
    logger: &Logger,
) {
    let files = match state.storage.incoming_files_to_resume(xfer.id()) {
        Ok(files) => files,
        Err(err) => {
            warn!(logger, "Failed to fetch files to resume: {err}");
            return;
        }
    };

    for file in files {
        info!(logger, "Resuming file: {}", file.file_id);

        if let Some(xfile) = xfer.files().get(&file.file_id) {
            let task = FileXferTask::new(xfile.clone(), xfer.clone(), file.base_dir.into());

            let _ = req_send.send(ServerReq::Download {
                task: Box::new(task),
            });
        } else {
            warn!(logger, "Missing file: {}", file.file_id);
        }
    }
}

fn reject_transfer_files(
    state: &State,
    xfer: &Arc<IncomingTransfer>,
    req_send: &mpsc::UnboundedSender<ServerReq>,
    logger: &Logger,
) {
    let files = match state.storage.incoming_files_to_reject(xfer.id()) {
        Ok(files) => files,
        Err(err) => {
            warn!(logger, "Failed to fetch files to reject: {err}");
            return;
        }
    };

    for file_id in files {
        info!(logger, "Rejecting file: {file_id}");

        if xfer.files().get(&file_id).is_some() {
            let _ = req_send.send(ServerReq::Reject {
                file: file_id.into(),
            });
        } else {
            warn!(logger, "Missing file: {file_id}");
        }
    }
}

fn register_finished_paths(state: &State, xstate: &mut IncomingState, logger: &Logger) {
    let paths = match state.storage.finished_incoming_files(xstate.xfer.id()) {
        Ok(paths) => paths,
        Err(err) => {
            warn!(logger, "Failed to fetch finished paths: {err:?}");
            return;
        }
    };

    for path in paths {
        let subpath = FileSubPath::from(path.subpath);
        xstate
            .dir_mappings
            .register_preexisting_final_path(&subpath, &path.final_path);
    }
}
