// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    chunk_request::{GetChunkRequest, TargetType},
    chunk_response::{GetChunkResponse, ResponseLedgerInfo},
    counters,
    executor_proxy::ExecutorProxyTrait,
    logging::{LogEntry, LogEvent, LogSchema},
    network::{StateSynchronizerEvents, StateSynchronizerMsg, StateSynchronizerSender},
    request_manager::{PeerScoreUpdateType, RequestManager},
    SynchronizerState,
};
use anyhow::{bail, ensure, format_err, Result};
use futures::{
    channel::{mpsc, oneshot},
    stream::select_all,
    StreamExt,
};
use libra_config::{
    config::{PeerNetworkId, RoleType, StateSyncConfig, UpstreamConfig},
    network_id::NodeNetworkId,
};
use libra_logger::prelude::*;
use libra_mempool::{CommitNotification, CommitResponse, CommittedTransaction};
use libra_types::{
    contract_event::ContractEvent,
    epoch_change::Verifier,
    ledger_info::LedgerInfoWithSignatures,
    transaction::{Transaction, TransactionListWithProof, Version},
    waypoint::Waypoint,
};
use network::protocols::network::Event;
use std::{
    collections::{BTreeMap, HashMap},
    ops::Bound::Included,
    time::{Duration, SystemTime},
};
use tokio::time::{interval, timeout};

pub struct SyncRequest {
    // The Result value returned to the caller is Error in case the StateSynchronizer failed to
    // reach the target (the LI in the storage remains unchanged as if nothing happened).
    pub callback: oneshot::Sender<Result<()>>,
    pub target: LedgerInfoWithSignatures,
    pub last_progress_tst: SystemTime,
}

/// message used by StateSyncClient for communication with Coordinator
pub enum CoordinatorMessage {
    // used to initiate new sync
    Request(Box<SyncRequest>),
    // used to notify about new txn commit
    Commit(
        // committed transactions
        Vec<Transaction>,
        // reconfiguration events
        Vec<ContractEvent>,
        // callback for recipient to send response back to this sender
        oneshot::Sender<Result<CommitResponse>>,
    ),
    GetState(oneshot::Sender<SynchronizerState>),
    // Receive a notification via a given channel when coordinator is initialized.
    WaitInitialize(oneshot::Sender<Result<()>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingRequestInfo {
    expiration_time: SystemTime,
    known_version: u64,
    request_epoch: u64,
    limit: u64,
}

// DS to help sync requester to keep track of ledger infos in the future
// if it is lagging far behind the upstream node
// Should only be modified upon local storage sync
struct PendingLedgerInfos {
    // In-memory store of ledger infos that are pending commits
    // (k, v) - (LI version, LI)
    pending_li_queue: BTreeMap<Version, LedgerInfoWithSignatures>,
    // max size limit on `pending_li_queue`, to prevent OOM
    max_pending_li_limit: usize,
    // target li
    target_li: Option<LedgerInfoWithSignatures>,
}

impl PendingLedgerInfos {
    fn new(max_pending_li_limit: usize) -> Self {
        Self {
            pending_li_queue: BTreeMap::new(),
            max_pending_li_limit,
            target_li: None,
        }
    }

    /// Adds `new_li` to the queue of pending LI's
    fn add_li(&mut self, new_li: LedgerInfoWithSignatures) {
        if self.pending_li_queue.len() >= self.max_pending_li_limit {
            warn!(
                LogSchema::new(LogEntry::ProcessChunkResponse),
                "pending LI store reached max capacity {}, failed to add LI {}",
                self.max_pending_li_limit,
                new_li
            );
            return;
        }

        // update pending_ledgers if new LI is ahead of target LI (in terms of version)
        let target_version = self
            .target_li
            .as_ref()
            .map_or(0, |li| li.ledger_info().version());
        if new_li.ledger_info().version() > target_version {
            self.pending_li_queue
                .insert(new_li.ledger_info().version(), new_li);
        }
    }

    fn update(&mut self, sync_state: &SynchronizerState, chunk_limit: u64) {
        let highest_committed_li = sync_state.highest_local_li.ledger_info().version();
        let highest_synced = sync_state.highest_version_in_local_storage();

        // prune any pending LIs that were successfully committed
        self.pending_li_queue = self.pending_li_queue.split_off(&(highest_committed_li + 1));

        // pick target LI to use for sending ProgressiveTargetType requests.
        self.target_li = if highest_committed_li == highest_synced {
            // try to find LI with max version that will fit in a single chunk
            self.pending_li_queue
                .range((Included(0), Included(highest_synced + chunk_limit)))
                .rev()
                .next()
                .map(|(_version, ledger_info)| ledger_info.clone())
        } else {
            self.pending_li_queue
                .iter()
                .next()
                .map(|(_version, ledger_info)| ledger_info.clone())
        };
    }

    fn target_li(&self) -> Option<LedgerInfoWithSignatures> {
        self.target_li.clone()
    }
}

/// Coordination of synchronization process is driven by SyncCoordinator, which `start()` function
/// runs an infinite event loop and triggers actions based on external / internal requests.
/// The coordinator can work in two modes:
/// * FullNode: infinite stream of ChunkRequests is sent to the predefined static peers
/// (the parent is going to reply with a ChunkResponse if its committed version becomes
/// higher within the timeout interval).
/// * Validator: the ChunkRequests are generated on demand for a specific target LedgerInfo to
/// synchronize to.
pub(crate) struct SyncCoordinator<T> {
    // used to process client requests
    client_events: mpsc::UnboundedReceiver<CoordinatorMessage>,
    // used to send messages (e.g. notifications about newly committed txns) to mempool
    state_sync_to_mempool_sender: mpsc::Sender<CommitNotification>,
    // Current state of the storage, which includes both the latest committed transaction and the
    // latest transaction covered by the LedgerInfo (see `SynchronizerState` documentation).
    // The state is updated via syncing with the local storage.
    local_state: SynchronizerState,
    // config
    config: StateSyncConfig,
    // role of node
    role: RoleType,
    // An initial waypoint: for as long as the local version is less than a version determined by
    // waypoint a node is not going to be abl
    waypoint: Waypoint,
    // network senders - (k, v) = (network ID, network sender)
    network_senders: HashMap<NodeNetworkId, StateSynchronizerSender>,
    // Actor for sending chunk requests
    // Manages to whom and how to send chunk requests
    request_manager: RequestManager,
    // Optional sync request to be called when the target sync is reached
    sync_request: Option<SyncRequest>,
    // Ledger infos in the future that have not been committed yet
    pending_ledger_infos: PendingLedgerInfos,
    // Option initialization listener to be called when the coordinator is caught up with
    // its waypoint.
    initialization_listener: Option<oneshot::Sender<Result<()>>>,
    // queue of incoming long polling requests
    // peer will be notified about new chunk of transactions if it's available before expiry time
    subscriptions: HashMap<PeerNetworkId, PendingRequestInfo>,
    executor_proxy: T,
}

impl<T: ExecutorProxyTrait> SyncCoordinator<T> {
    pub fn new(
        client_events: mpsc::UnboundedReceiver<CoordinatorMessage>,
        state_sync_to_mempool_sender: mpsc::Sender<CommitNotification>,
        network_senders: HashMap<NodeNetworkId, StateSynchronizerSender>,
        role: RoleType,
        waypoint: Waypoint,
        config: StateSyncConfig,
        upstream_config: UpstreamConfig,
        executor_proxy: T,
        initial_state: SynchronizerState,
    ) -> Self {
        info!(LogSchema::event_log(LogEntry::Waypoint, LogEvent::Initialize).waypoint(waypoint));
        let retry_timeout_val = match role {
            RoleType::FullNode => config.tick_interval_ms + config.long_poll_timeout_ms,
            RoleType::Validator => 2 * config.tick_interval_ms,
        };
        let multicast_timeout = Duration::from_millis(config.multicast_timeout_ms);

        Self {
            client_events,
            state_sync_to_mempool_sender,
            local_state: initial_state,
            pending_ledger_infos: PendingLedgerInfos::new(config.max_pending_li_limit),
            config,
            role,
            waypoint,
            request_manager: RequestManager::new(
                upstream_config,
                Duration::from_millis(retry_timeout_val),
                multicast_timeout,
                network_senders.clone(),
            ),
            network_senders,
            subscriptions: HashMap::new(),
            sync_request: None,
            initialization_listener: None,
            executor_proxy,
        }
    }

    /// main routine. starts sync coordinator that listens for CoordinatorMsg
    pub async fn start(
        mut self,
        network_handles: Vec<(
            NodeNetworkId,
            StateSynchronizerSender,
            StateSynchronizerEvents,
        )>,
    ) {
        info!(LogSchema::new(LogEntry::RuntimeStart));
        let mut interval = interval(Duration::from_millis(self.config.tick_interval_ms)).fuse();

        let events: Vec<_> = network_handles
            .into_iter()
            .map(|(network_id, _sender, events)| events.map(move |e| (network_id.clone(), e)))
            .collect();
        let mut network_events = select_all(events).fuse();

        loop {
            ::futures::select! {
                msg = self.client_events.select_next_some() => {
                    match msg {
                        CoordinatorMessage::Request(request) => {
                            let _timer = counters::PROCESS_COORDINATOR_MSG_LATENCY
                                .with_label_values(&[counters::SYNC_MSG_LABEL])
                                .start_timer();
                            if let Err(e) = self.request_sync(*request) {
                                error!(LogSchema::new(LogEntry::SyncRequest).error(&e));
                                counters::SYNC_REQUEST_RESULT.with_label_values(&[counters::FAIL_LABEL]).inc();
                            }
                        }
                        CoordinatorMessage::Commit(txns, events, callback) => {
                            {
                                let _timer = counters::PROCESS_COORDINATOR_MSG_LATENCY
                                    .with_label_values(&[counters::COMMIT_MSG_LABEL])
                                    .start_timer();
                                if let Err(e) = self.process_commit(txns, Some(callback), None).await {
                                    counters::CONSENSUS_COMMIT_FAIL_COUNT.inc();
                                    error!(LogSchema::event_log(LogEntry::ConsensusCommit, LogEvent::PostCommitFail).error(&e));
                                }
                            }
                            if let Err(e) = self.executor_proxy.publish_on_chain_config_updates(events) {
                                counters::RECONFIG_PUBLISH_COUNT
                                    .with_label_values(&[counters::FAIL_LABEL])
                                    .inc();
                                error!(LogSchema::event_log(LogEntry::Reconfig, LogEvent::Fail).error(&e));
                            }
                        }
                        CoordinatorMessage::GetState(callback) => {
                            self.get_state(callback);
                        }
                        CoordinatorMessage::WaitInitialize(cb_sender) => {
                            self.set_initialization_listener(cb_sender);
                        }
                    };
                },
                (network_id, event) = network_events.select_next_some() => {
                    match event {
                        Event::NewPeer(peer_id, origin) => {
                            let peer = PeerNetworkId(network_id, peer_id);
                            self.request_manager.enable_peer(peer, origin);
                            self.check_progress();
                        }
                        Event::LostPeer(peer_id, origin) => {
                            let peer = PeerNetworkId(network_id, peer_id);
                            self.request_manager.disable_peer(&peer, origin);
                        }
                        Event::Message(peer_id, message) => self.process_one_message(PeerNetworkId(network_id.clone(), peer_id), message).await,
                        unexpected_event => {
                            counters::NETWORK_ERROR_COUNT.inc();
                            warn!(LogSchema::new(LogEntry::NetworkError),
                            "received unexpected network event: {:?}", unexpected_event);
                        },
                    }
                },
                _ = interval.select_next_some() => {
                    self.check_progress();
                }
            }
        }
    }

    pub(crate) async fn process_one_message(
        &mut self,
        peer: PeerNetworkId,
        msg: StateSynchronizerMsg,
    ) {
        match msg {
            StateSynchronizerMsg::GetChunkRequest(request) => {
                let _timer = counters::PROCESS_MSG_LATENCY
                    .with_label_values(&[
                        &peer.raw_network_id().to_string(),
                        &peer.peer_id().to_string(),
                        counters::CHUNK_REQUEST_MSG_LABEL,
                    ])
                    .start_timer();
                let result_label =
                    if let Err(err) = self.process_chunk_request(peer.clone(), *request.clone()) {
                        error!(
                            LogSchema::event_log(LogEntry::ProcessChunkRequest, LogEvent::Fail)
                                .peer(&peer)
                                .error(&err)
                                .local_li_version(
                                    self.local_state.highest_local_li.ledger_info().version()
                                )
                                .chunk_req(&request)
                        );
                        counters::FAIL_LABEL
                    } else {
                        counters::SUCCESS_LABEL
                    };
                counters::PROCESS_CHUNK_REQUEST_COUNT
                    .with_label_values(&[
                        &peer.raw_network_id().to_string(),
                        &peer.peer_id().to_string(),
                        result_label,
                    ])
                    .inc();
            }
            StateSynchronizerMsg::GetChunkResponse(response) => {
                let _timer = counters::PROCESS_MSG_LATENCY
                    .with_label_values(&[
                        &peer.raw_network_id().to_string(),
                        &peer.peer_id().to_string(),
                        counters::CHUNK_RESPONSE_MSG_LABEL,
                    ])
                    .start_timer();
                self.process_chunk_response(&peer, *response).await;
            }
        }
    }

    /// Sync up coordinator state with the local storage
    /// and updates the pending ledger info accordingly
    fn sync_state_with_local_storage(&mut self) -> Result<()> {
        let new_state = self.executor_proxy.get_local_storage_state().map_err(|e| {
            counters::STORAGE_READ_FAIL_COUNT.inc();
            e
        })?;
        if new_state.epoch() > self.local_state.epoch() {
            info!(LogSchema::new(LogEntry::EpochChange)
                .old_epoch(self.local_state.epoch())
                .new_epoch(new_state.epoch()));
        }
        self.local_state = new_state;

        self.pending_ledger_infos
            .update(&self.local_state, self.config.chunk_limit);
        Ok(())
    }

    /// Verify that the local state's latest LI version (i.e. committed version) has reached the waypoint version.
    fn is_initialized(&self) -> bool {
        self.waypoint.version() <= self.local_state.highest_local_li.ledger_info().version()
    }

    fn set_initialization_listener(&mut self, cb_sender: oneshot::Sender<Result<()>>) {
        if self.is_initialized() {
            if let Err(e) = Self::send_initialization_callback(cb_sender, Ok(())) {
                error!(LogSchema::event_log(LogEntry::Waypoint, LogEvent::CallbackFail).error(&e));
            }
        } else {
            self.initialization_listener = Some(cb_sender);
        }
    }

    /// In case there has been another pending request it's going to be overridden.
    /// The caller will be notified about request completion via request.callback oneshot:
    /// at that moment it's guaranteed that the highest LI exposed by the storage is equal to the
    /// target LI.
    /// StateSynchronizer assumes that it's the only one modifying the storage (consensus is not
    /// trying to commit transactions concurrently).
    fn request_sync(&mut self, request: SyncRequest) -> Result<()> {
        let local_li_version = self.local_state.highest_local_li.ledger_info().version();
        let target_version = request.target.ledger_info().version();
        debug!(
            LogSchema::event_log(LogEntry::SyncRequest, LogEvent::Received)
                .target_version(target_version)
                .local_li_version(local_li_version)
        );

        self.sync_state_with_local_storage()?;
        ensure!(
            self.is_initialized(),
            "[state sync] Sync request but initialization is not complete!"
        );
        if target_version == local_li_version {
            return Self::send_sync_req_callback(request, Ok(()));
        }

        if target_version < local_li_version {
            Self::send_sync_req_callback(request, Err(format_err!("Sync request to old version")))?;
            bail!(
                "[state sync] Sync request for version {} < known version {}",
                target_version,
                local_li_version,
            );
        }

        self.sync_request = Some(request);
        self.send_chunk_request(
            self.local_state.highest_version_in_local_storage(),
            self.local_state.epoch(),
        )
    }

    /// The function is called after new txns have been applied to the local storage.
    /// As a result it might:
    /// 1) help remote subscribers with long poll requests, 2) finish local sync request
    async fn process_commit(
        &mut self,
        transactions: Vec<Transaction>,
        commit_callback: Option<oneshot::Sender<Result<CommitResponse>>>,
        chunk_sender: Option<&PeerNetworkId>,
    ) -> Result<()> {
        // We choose to re-sync the state with the storage as it's the simplest approach:
        // in case the performance implications of re-syncing upon every commit are high,
        // it's possible to manage some of the highest known versions in memory.
        self.sync_state_with_local_storage()?;
        let synced_version = self.local_state.highest_version_in_local_storage();
        let committed_version = self.local_state.highest_local_li.ledger_info().version();
        let local_epoch = self.local_state.epoch();
        counters::VERSION
            .with_label_values(&[counters::SYNCED_VERSION_LABEL])
            .set(synced_version as i64);
        counters::VERSION
            .with_label_values(&[counters::COMMITTED_VERSION_LABEL])
            .set(committed_version as i64);
        counters::EPOCH.set(local_epoch as i64);
        debug!(LogSchema::new(LogEntry::LocalState)
            .local_li_version(committed_version)
            .local_synced_version(synced_version)
            .local_epoch(local_epoch));
        let block_timestamp_usecs = self
            .local_state
            .highest_local_li
            .ledger_info()
            .timestamp_usecs();

        // send notif to shared mempool
        // filter for user transactions here
        let mut committed_user_txns = vec![];
        for txn in transactions {
            if let Transaction::UserTransaction(signed_txn) = txn {
                committed_user_txns.push(CommittedTransaction {
                    sender: signed_txn.sender(),
                    sequence_number: signed_txn.sequence_number(),
                });
            }
        }
        let (callback, callback_rcv) = oneshot::channel();
        let req = CommitNotification {
            transactions: committed_user_txns,
            block_timestamp_usecs,
            callback,
        };
        let mut mempool_channel = self.state_sync_to_mempool_sender.clone();
        let mut msg = "";
        if let Err(e) = mempool_channel.try_send(req) {
            error!(
                LogSchema::new(LogEntry::CommitFlow).error(&e.into()),
                "failed to notify mempool of commit"
            );
            counters::COMMIT_FLOW_FAIL
                .with_label_values(&[counters::TO_MEMPOOL_LABEL])
                .inc();
            msg = "state sync failed to send commit notif to shared mempool";
        } else if let Err(e) = timeout(Duration::from_secs(5), callback_rcv).await {
            error!(
                LogSchema::new(LogEntry::CommitFlow).error(&e.into()),
                "did not receive ACK for commit notification sent to mempool"
            );
            counters::COMMIT_FLOW_FAIL
                .with_label_values(&[counters::FROM_MEMPOOL_LABEL])
                .inc();
            msg = "state sync did not receive ACK for commit notification sent to mempool";
        }

        if let Some(cb) = commit_callback {
            // send back ACK to consensus
            if cb
                .send(Ok(CommitResponse {
                    msg: msg.to_string(),
                }))
                .is_err()
            {
                counters::COMMIT_FLOW_FAIL
                    .with_label_values(&[counters::CONSENSUS_LABEL])
                    .inc();
                error!(
                    LogSchema::new(LogEntry::CommitFlow),
                    "failed to send commit ACK to consensus"
                );
            }
        }

        self.check_subscriptions();
        self.request_manager.remove_requests(synced_version);
        if let Some(peer) = chunk_sender {
            self.request_manager.process_success_response(peer);
        }

        if let Some(mut req) = self.sync_request.as_mut() {
            req.last_progress_tst = SystemTime::now();
        }
        let sync_request_complete = match self.sync_request.as_ref() {
            Some(sync_req) => {
                // Each `ChunkResponse` is verified to make sure it never goes beyond the requested
                // target version, hence, the local version should never go beyond sync req target.
                let sync_target_version = sync_req.target.ledger_info().version();
                ensure!(
                    synced_version <= sync_target_version,
                    "local version {} is beyond sync req target {}",
                    synced_version,
                    sync_target_version
                );
                sync_target_version == synced_version
            }
            None => false,
        };

        if sync_request_complete {
            debug!(
                LogSchema::event_log(LogEntry::SyncRequest, LogEvent::Complete)
                    .local_li_version(committed_version)
                    .local_synced_version(synced_version)
                    .local_epoch(local_epoch)
            );
            counters::SYNC_REQUEST_RESULT
                .with_label_values(&[counters::COMPLETE_LABEL])
                .inc();
            if let Some(sync_request) = self.sync_request.take() {
                Self::send_sync_req_callback(sync_request, Ok(()))?;
            }
        }

        let initialization_complete = self
            .initialization_listener
            .as_ref()
            .map_or(false, |_| self.is_initialized());
        if initialization_complete {
            info!(LogSchema::event_log(LogEntry::Waypoint, LogEvent::Complete)
                .local_li_version(committed_version)
                .local_synced_version(synced_version)
                .local_epoch(local_epoch));
            if let Some(listener) = self.initialization_listener.take() {
                Self::send_initialization_callback(listener, Ok(()))?;
            }
        }
        Ok(())
    }

    fn get_state(&mut self, callback: oneshot::Sender<SynchronizerState>) {
        if let Err(e) = self.sync_state_with_local_storage() {
            error!(
                "[state sync] failed to sync with local storage for get_state request: {:?}",
                e
            );
        }
        if callback.send(self.local_state.clone()).is_err() {
            error!("[state sync] failed to send internal state");
        }
    }

    /// There are two types of ChunkRequests:
    /// 1) Validator chunk requests are for a specific target LI and don't ask for long polling.
    /// 2) FullNode chunk requests don't specify a target LI and can allow long polling.
    fn process_chunk_request(
        &mut self,
        peer: PeerNetworkId,
        request: GetChunkRequest,
    ) -> Result<()> {
        debug!(
            LogSchema::event_log(LogEntry::ProcessChunkRequest, LogEvent::Received)
                .peer(&peer)
                .chunk_req(&request)
                .local_li_version(self.local_state.highest_local_li.ledger_info().version())
        );
        self.sync_state_with_local_storage()?;

        match request.target().clone() {
            TargetType::TargetLedgerInfo(li) => self.process_request_target_li(peer, request, li),
            TargetType::HighestAvailable {
                target_li,
                timeout_ms,
            } => self.process_request_highest_available(peer, request, target_li, timeout_ms),
            TargetType::Waypoint(waypoint_version) => {
                self.process_request_waypoint(peer, request, waypoint_version)
            }
        }
    }

    /// Processing requests with a specified target LedgerInfo.
    /// Assumes that the local state is uptodate with storage.
    fn process_request_target_li(
        &mut self,
        peer: PeerNetworkId,
        request: GetChunkRequest,
        target_li: LedgerInfoWithSignatures,
    ) -> Result<()> {
        let limit = std::cmp::min(request.limit, self.config.max_chunk_limit);
        let response_li = self.choose_response_li(request.current_epoch, Some(target_li))?;
        // In case known_version is lower than the requested ledger info an empty response might be
        // sent.
        self.deliver_chunk(
            peer,
            request.known_version,
            ResponseLedgerInfo::VerifiableLedgerInfo(response_li),
            limit,
        )
    }

    /// Processing requests with no target LedgerInfo (highest available) and potentially long
    /// polling.
    /// Assumes that the local state is uptodate with storage.
    fn process_request_highest_available(
        &mut self,
        peer: PeerNetworkId,
        request: GetChunkRequest,
        target_li: Option<LedgerInfoWithSignatures>,
        timeout_ms: u64,
    ) -> Result<()> {
        let limit = std::cmp::min(request.limit, self.config.max_chunk_limit);
        let timeout = std::cmp::min(timeout_ms, self.config.max_timeout_ms);

        // If there is nothing a node can help with, and the request supports long polling,
        // add it to the subscriptions.
        let local_version = self.local_state.highest_local_li.ledger_info().version();
        if local_version <= request.known_version && timeout > 0 {
            let expiration_time = SystemTime::now().checked_add(Duration::from_millis(timeout));
            if let Some(time) = expiration_time {
                let request_info = PendingRequestInfo {
                    expiration_time: time,
                    known_version: request.known_version,
                    request_epoch: request.current_epoch,
                    limit,
                };
                self.subscriptions.insert(peer, request_info);
            }
            return Ok(());
        }

        // If the request's epoch is in the past, `target_li` will be set to the end-of-epoch LI for that epoch
        let target_li = self.choose_response_li(request.current_epoch, target_li)?;
        // Only populate highest_li field if it is different from target_li
        let highest_li = if target_li.ledger_info().version() < local_version
            && target_li.ledger_info().epoch() == self.local_state.epoch()
        {
            Some(self.local_state.highest_local_li.clone())
        } else {
            None
        };

        self.deliver_chunk(
            peer,
            request.known_version,
            ResponseLedgerInfo::ProgressiveLedgerInfo {
                target_li,
                highest_li,
            },
            limit,
        )
    }

    fn process_request_waypoint(
        &mut self,
        peer: PeerNetworkId,
        request: GetChunkRequest,
        waypoint_version: Version,
    ) -> Result<()> {
        let mut limit = std::cmp::min(request.limit, self.config.max_chunk_limit);
        ensure!(
            self.local_state.highest_local_li.ledger_info().version() >= waypoint_version,
            "Local version {} < requested waypoint version {}.",
            self.local_state.highest_local_li.ledger_info().version(),
            waypoint_version
        );
        ensure!(
            request.known_version < waypoint_version,
            "Waypoint request version {} is not smaller than waypoint {}",
            request.known_version,
            waypoint_version
        );

        // Retrieve the waypoint LI.
        let waypoint_li = self
            .executor_proxy
            .get_epoch_ending_ledger_info(waypoint_version)?;

        // Txns are up to the end of request epoch with the proofs relative to the waypoint LI.
        let end_of_epoch_li = if waypoint_li.ledger_info().epoch() > request.current_epoch {
            Some(self.executor_proxy.get_epoch_proof(request.current_epoch)?)
        } else {
            None
        };
        if let Some(li) = end_of_epoch_li.as_ref() {
            let num_txns_until_end_of_epoch = li.ledger_info().version() - request.known_version;
            limit = std::cmp::min(limit, num_txns_until_end_of_epoch);
        }
        self.deliver_chunk(
            peer,
            request.known_version,
            ResponseLedgerInfo::LedgerInfoForWaypoint {
                waypoint_li,
                end_of_epoch_li,
            },
            limit,
        )
    }

    /// Generate and send the ChunkResponse to the given peer.
    /// The chunk response contains transactions from the local storage with the proofs relative to
    /// the given target ledger info.
    /// In case target is None, the ledger info is set to the local highest ledger info.
    fn deliver_chunk(
        &mut self,
        peer: PeerNetworkId,
        known_version: u64,
        response_li: ResponseLedgerInfo,
        limit: u64,
    ) -> Result<()> {
        let txns = self
            .executor_proxy
            .get_chunk(known_version, limit, response_li.version())?;
        let chunk_response = GetChunkResponse::new(response_li, txns);
        let log = LogSchema::event_log(LogEntry::ProcessChunkRequest, LogEvent::DeliverChunk)
            .chunk_resp(&chunk_response)
            .peer(&peer);
        let msg = StateSynchronizerMsg::GetChunkResponse(Box::new(chunk_response));

        let network_sender = self
            .network_senders
            .get_mut(&peer.network_id())
            .expect("missing network sender");
        let send_result = network_sender.send_to(peer.peer_id(), msg);
        let send_result_label = if send_result.is_err() {
            counters::SEND_FAIL_LABEL
        } else {
            debug!(log);
            counters::SEND_SUCCESS_LABEL
        };
        counters::RESPONSES_SENT
            .with_label_values(&[
                &peer.raw_network_id().to_string(),
                &peer.peer_id().to_string(),
                send_result_label,
            ])
            .inc();

        send_result.map_err(|e| {
            error!(log.error(&e.into()));
            format_err!("Network error in sending chunk response to {}", peer)
        })
    }

    /// The choice of the LedgerInfo in the response follows the following logic:
    /// * response LI is either the requested target or the highest local LI if target is None.
    /// * if the response LI would not belong to `request_epoch`, change
    /// the response LI to the LI that is terminating `request_epoch`.
    fn choose_response_li(
        &self,
        request_epoch: u64,
        target: Option<LedgerInfoWithSignatures>,
    ) -> Result<LedgerInfoWithSignatures> {
        let mut target_li = target.unwrap_or_else(|| self.local_state.highest_local_li.clone());
        let target_epoch = target_li.ledger_info().epoch();
        if target_epoch > request_epoch {
            let end_of_epoch_li = self.executor_proxy.get_epoch_proof(request_epoch)?;
            debug!(LogSchema::event_log(
                LogEntry::ProcessChunkRequest,
                LogEvent::PastEpochRequested
            )
            .old_epoch(request_epoch)
            .new_epoch(target_epoch));
            target_li = end_of_epoch_li;
        }
        Ok(target_li)
    }

    /// Applies (= executes and stores) chunk to storage if `response` is valid
    /// Chunk response checks performed:
    /// - does chunk contain no transactions?
    /// - does chunk of transactions matches the local state's version?
    /// - verify LIs in chunk response against local state
    /// - execute and commit chunk
    /// Returns error if above chunk response checks fail or chunk was not able to be stored to storage, else
    /// return Ok(()) if above checks all pass and chunk was stored to storage
    fn apply_chunk(&mut self, peer: &PeerNetworkId, response: GetChunkResponse) -> Result<()> {
        debug!(
            LogSchema::event_log(LogEntry::ProcessChunkResponse, LogEvent::Received)
                .chunk_resp(&response)
        );

        if !self.request_manager.is_known_upstream_peer(peer) {
            counters::RESPONSE_FROM_DOWNSTREAM_COUNT
                .with_label_values(&[
                    &peer.raw_network_id().to_string(),
                    &peer.peer_id().to_string(),
                ])
                .inc();
            bail!("received chunk response from downstream");
        }

        let txn_list_with_proof = response.txn_list_with_proof.clone();
        let known_version = self.local_state.highest_version_in_local_storage();
        let chunk_start_version =
            txn_list_with_proof
                .first_transaction_version
                .ok_or_else(|| {
                    self.request_manager
                        .update_score(&peer, PeerScoreUpdateType::EmptyChunk);
                    format_err!("[state sync] Empty chunk from {:?}", peer)
                })?;

        if chunk_start_version != known_version + 1 {
            // Old / wrong chunk.
            self.request_manager.process_chunk_version_mismatch(
                peer,
                chunk_start_version,
                known_version,
            )?;
        }

        let chunk_size = txn_list_with_proof.len() as u64;
        match response.response_li {
            ResponseLedgerInfo::VerifiableLedgerInfo(li) => {
                self.process_response_with_verifiable_li(txn_list_with_proof, li, None)
            }
            ResponseLedgerInfo::ProgressiveLedgerInfo {
                target_li,
                highest_li,
            } => {
                let highest_li = highest_li.unwrap_or_else(|| target_li.clone());
                ensure!(
                    target_li.ledger_info().version() <= highest_li.ledger_info().version(),
                    "Progressive ledger info received target LI {} higher than highest LI {}",
                    target_li,
                    highest_li
                );
                self.process_response_with_verifiable_li(
                    txn_list_with_proof,
                    target_li,
                    Some(highest_li),
                )
            }
            ResponseLedgerInfo::LedgerInfoForWaypoint {
                waypoint_li,
                end_of_epoch_li,
            } => self.process_response_with_waypoint_li(
                txn_list_with_proof,
                waypoint_li,
                end_of_epoch_li,
            ),
        }
        .map_err(|e| {
            self.request_manager
                .update_score(peer, PeerScoreUpdateType::InvalidChunk);
            format_err!("[state sync] failed to apply chunk: {}", e)
        })?;

        counters::STATE_SYNC_CHUNK_SIZE
            .with_label_values(&[
                &peer.raw_network_id().to_string(),
                &peer.peer_id().to_string(),
            ])
            .observe(chunk_size as f64);
        debug!(
            LogSchema::event_log(LogEntry::ProcessChunkResponse, LogEvent::ApplyChunkSuccess),
            "Applied chunk of size {}. Previous version: {}, new version {}",
            chunk_size,
            known_version,
            known_version + chunk_size
        );

        // The overall chunk processing duration is calculated starting from the very first attempt
        // until the commit
        if let Some(first_attempt_tst) = self.request_manager.get_first_request_time(known_version)
        {
            if let Ok(duration) = SystemTime::now().duration_since(first_attempt_tst) {
                counters::SYNC_PROGRESS_DURATION.observe_duration(duration);
            }
        }
        Ok(())
    }

    /// * Verifies and stores chunk in response
    /// * Triggers post-commit actions based on new local state after successful chunk processing in above step
    async fn process_chunk_response(&mut self, peer: &PeerNetworkId, response: GetChunkResponse) {
        let new_txns = response.txn_list_with_proof.transactions.clone();
        // Part 1: check response, validate and store chunk
        // any errors thrown here should be for detecting actual bad chunks
        if let Err(e) = self.apply_chunk(peer, response) {
            // count, log, and exit
            error!(
                LogSchema::event_log(LogEntry::ProcessChunkResponse, LogEvent::ApplyChunkFail)
                    .peer(peer)
                    .error(&e)
            );

            counters::APPLY_CHUNK_COUNT
                .with_label_values(&[
                    &peer.raw_network_id().to_string(),
                    &peer.peer_id().to_string(),
                    counters::FAIL_LABEL,
                ])
                .inc();
            return;
        }

        counters::APPLY_CHUNK_COUNT
            .with_label_values(&[
                &peer.raw_network_id().to_string(),
                &peer.peer_id().to_string(),
                counters::SUCCESS_LABEL,
            ])
            .inc();

        // Part 2: post-chunk-process stage: process commit
        if let Err(e) = self.process_commit(new_txns, None, Some(peer)).await {
            error!(
                LogSchema::event_log(LogEntry::ProcessChunkResponse, LogEvent::PostCommitFail)
                    .error(&e)
            );
        }
    }

    /// Processing chunk responses that carry a LedgerInfo that should be verified using the
    /// current local trusted validator set.
    fn process_response_with_verifiable_li(
        &mut self,
        txn_list_with_proof: TransactionListWithProof,
        response_li: LedgerInfoWithSignatures,
        // LI to verify and add to pending_ledger_infos
        // may be the same as response_li
        pending_li: Option<LedgerInfoWithSignatures>,
    ) -> Result<()> {
        ensure!(
            self.is_initialized(),
            "Response with a non-waypoint LI while still not initialized"
        );
        if let Some(sync_req) = self.sync_request.as_ref() {
            // Valid responses should not exceed the LI version of the request.
            if sync_req.target.ledger_info().version() < response_li.ledger_info().version() {
                bail!(
                    "[state sync] Response has an LI version {} higher than requested version {}.",
                    response_li.ledger_info().version(),
                    sync_req.target.ledger_info().version(),
                );
            }
        }
        // Optimistically fetch the next chunk assuming the current chunk is going to be applied
        // successfully.
        let new_version =
            self.local_state.highest_version_in_local_storage() + txn_list_with_proof.len() as u64;
        let new_epoch = if response_li.ledger_info().version() == new_version
            && response_li.ledger_info().ends_epoch()
        {
            // This chunk is going to finish the current epoch, optimistically request a chunk
            // from the next epoch.
            self.local_state.epoch() + 1
        } else {
            // Remain in the current epoch
            self.local_state.epoch()
        };
        self.local_state.trusted_epoch.verify(&response_li)?;
        if let Some(li) = pending_li {
            if li != response_li {
                self.local_state.trusted_epoch.verify(&li)?;
            }
            self.pending_ledger_infos.add_li(li);
        }
        self.validate_and_store_chunk(txn_list_with_proof, response_li, None)?;

        // need to sync with local storage to see whether response LI was actually committed
        // and update pending_ledger_infos accordingly
        self.sync_state_with_local_storage()?;
        let new_version = self.local_state.highest_version_in_local_storage();

        // don't throw error for failed chunk request send, as this failure is not related to
        // validity of the chunk response itself
        if let Err(e) = self.send_chunk_request(new_version, new_epoch) {
            error!(LogSchema::event_log(
                LogEntry::ProcessChunkResponse,
                LogEvent::SendChunkRequestFail
            )
            .error(&e));
        }

        Ok(())
    }

    /// Processing chunk responses that carry a LedgerInfo corresponding to the waypoint.
    fn process_response_with_waypoint_li(
        &mut self,
        txn_list_with_proof: TransactionListWithProof,
        waypoint_li: LedgerInfoWithSignatures,
        end_of_epoch_li: Option<LedgerInfoWithSignatures>,
    ) -> Result<()> {
        ensure!(
            !self.is_initialized(),
            "Response with a waypoint LI but we're already initialized"
        );
        // Optimistically fetch the next chunk.
        let new_version =
            self.local_state.highest_version_in_local_storage() + txn_list_with_proof.len() as u64;
        // The epoch in the optimistic request should be the next epoch if the current chunk
        // is the last one in its epoch.
        let new_epoch = end_of_epoch_li
            .as_ref()
            .map_or(self.local_state.epoch(), |li| {
                if li.ledger_info().version() == new_version {
                    self.local_state.epoch() + 1
                } else {
                    self.local_state.epoch()
                }
            });
        if new_version < self.waypoint.version() {
            if let Err(e) = self.send_chunk_request(new_version, new_epoch) {
                error!(LogSchema::event_log(
                    LogEntry::ProcessChunkResponse,
                    LogEvent::SendChunkRequestFail
                )
                .error(&e));
            }
        }

        self.waypoint.verify(waypoint_li.ledger_info())?;
        self.validate_and_store_chunk(txn_list_with_proof, waypoint_li, end_of_epoch_li)
    }

    // Assumes that the target LI has been already verified by the caller.
    fn validate_and_store_chunk(
        &mut self,
        txn_list_with_proof: TransactionListWithProof,
        target: LedgerInfoWithSignatures,
        intermediate_end_of_epoch_li: Option<LedgerInfoWithSignatures>,
    ) -> Result<()> {
        let target_epoch = target.ledger_info().epoch();
        let target_version = target.ledger_info().version();
        let local_epoch = self.local_state.highest_local_li.ledger_info().epoch();
        let local_version = self.local_state.highest_local_li.ledger_info().version();
        if (target_epoch, target_version) <= (local_epoch, local_version) {
            warn!(
                LogSchema::event_log(LogEntry::ProcessChunkResponse, LogEvent::OldResponseLI)
                    .local_li_version(local_version)
                    .local_epoch(local_epoch),
                response_li_version = target_version,
                response_li_epoch = target_epoch
            );
            return Ok(());
        }

        self.executor_proxy
            .execute_chunk(txn_list_with_proof, target, intermediate_end_of_epoch_li)
    }

    /// Ensures that StateSynchronizer is making progress:
    /// * kick-starts initial sync process (= initialization syncing to waypoint)
    /// * issue a new request if too much time passed since requesting highest_synced_version + 1.
    fn check_progress(&mut self) {
        if self.request_manager.no_available_peers() {
            return;
        }
        if self.role == RoleType::Validator && self.sync_request.is_none() && self.is_initialized()
        {
            return;
        }

        // check that we made progress in fulfilling consensus sync request
        let sync_request_expired = self.sync_request.as_ref().map_or(false, |req| {
            let default_timeout = Duration::from_millis(self.config.sync_request_timeout_ms);
            if let Some(tst) = req.last_progress_tst.checked_add(default_timeout) {
                return SystemTime::now().duration_since(tst).is_ok();
            }
            false
        });
        // notify consensus if sync request timed out
        if sync_request_expired {
            counters::SYNC_REQUEST_RESULT
                .with_label_values(&[counters::TIMEOUT_LABEL])
                .inc();
            warn!(LogSchema::event_log(
                LogEntry::SyncRequest,
                LogEvent::Timeout
            ));

            if let Some(sync_request) = self.sync_request.take() {
                if let Err(e) = Self::send_sync_req_callback(
                    sync_request,
                    Err(format_err!("request timed out")),
                ) {
                    error!(
                        LogSchema::event_log(LogEntry::SyncRequest, LogEvent::CallbackFail)
                            .error(&e)
                    );
                }
            }
        }

        let known_version = self.local_state.highest_version_in_local_storage();

        // if coordinator didn't make progress by expected time or did not send a request for current
        // local synced version, issue new request
        if self.request_manager.check_timeout(known_version) {
            // log and count timeout
            counters::TIMEOUT.inc();
            warn!(LogSchema::new(LogEntry::Timeout).version(known_version));
            if let Err(e) = self.send_chunk_request(known_version, self.local_state.epoch()) {
                error!(
                    LogSchema::event_log(LogEntry::Timeout, LogEvent::SendChunkRequestFail)
                        .version(known_version)
                        .error(&e)
                );
            }
        }
    }

    /// Sends a chunk request with a given `known_version` and `known_epoch`
    /// (might be chosen optimistically).
    fn send_chunk_request(&mut self, known_version: u64, known_epoch: u64) -> Result<()> {
        if self.request_manager.no_available_peers() {
            warn!(LogSchema::event_log(
                LogEntry::SendChunkRequest,
                LogEvent::MissingPeers
            ));
            bail!("No peers to send chunk request to");
        }

        let target = if !self.is_initialized() {
            let waypoint_version = self.waypoint.version();
            TargetType::Waypoint(waypoint_version)
        } else {
            match self.sync_request.as_ref() {
                None => {
                    TargetType::HighestAvailable {
                        // here, we need to ensure pending_ledger_infos is up-to-date with storage
                        // this is the responsibility of the caller of send_chunk_request
                        target_li: self.pending_ledger_infos.target_li(),
                        timeout_ms: self.config.long_poll_timeout_ms,
                    }
                }
                Some(sync_req) => {
                    let sync_target_version = sync_req.target.ledger_info().version();
                    if sync_target_version <= known_version {
                        // sync request is already fulfilled, so don't send chunk requests with this request as target
                        debug!(LogSchema::event_log(
                            LogEntry::SendChunkRequest,
                            LogEvent::OldSyncRequest
                        )
                        .target_version(sync_target_version)
                        .local_synced_version(known_version), "Sync request is already fulfilled, so no need to send chunk requests for this sync request");
                        return Ok(());
                    }
                    TargetType::TargetLedgerInfo(sync_req.target.clone())
                }
            }
        };

        let req = GetChunkRequest::new(known_version, known_epoch, self.config.chunk_limit, target);
        self.request_manager.send_chunk_request(req)
    }

    fn deliver_subscription(
        &mut self,
        peer: PeerNetworkId,
        request_info: PendingRequestInfo,
    ) -> Result<()> {
        let response_li = self.choose_response_li(request_info.request_epoch, None)?;
        self.deliver_chunk(
            peer,
            request_info.known_version,
            ResponseLedgerInfo::VerifiableLedgerInfo(response_li),
            request_info.limit,
        )
    }

    /// The function is called after the local storage is updated with new transactions:
    /// it might deliver chunks for the subscribers that have been waiting with the long polls.
    ///
    /// Note that it is possible to help the subscribers only with the transactions that match
    /// the highest ledger info in the local storage (some committed transactions are ahead of the
    /// latest ledger info and are not going to be used for helping the remote subscribers).
    /// The function assumes that the local state has been synced with storage.
    fn check_subscriptions(&mut self) {
        let highest_li_version = self.local_state.highest_local_li.ledger_info().version();

        let mut ready = vec![];
        self.subscriptions.retain(|peer, request_info| {
            // filter out expired peer requests
            if SystemTime::now()
                .duration_since(request_info.expiration_time)
                .is_ok()
            {
                return false;
            }
            if request_info.known_version < highest_li_version {
                ready.push((peer.clone(), request_info.clone()));
                false
            } else {
                true
            }
        });

        ready.into_iter().for_each(|(peer, request_info)| {
            let result_label =
                if let Err(err) = self.deliver_subscription(peer.clone(), request_info) {
                    error!(LogSchema::new(LogEntry::SubscriptionDeliveryFail)
                        .peer(&peer)
                        .error(&err));
                    counters::FAIL_LABEL
                } else {
                    counters::SUCCESS_LABEL
                };
            counters::SUBSCRIPTION_DELIVERY_COUNT
                .with_label_values(&[
                    &peer.raw_network_id().to_string(),
                    &peer.peer_id().to_string(),
                    result_label,
                ])
                .inc();
        });
    }

    fn send_sync_req_callback(sync_req: SyncRequest, msg: Result<()>) -> Result<()> {
        sync_req.callback.send(msg).map_err(|failed_msg| {
            counters::FAILED_CHANNEL_SEND
                .with_label_values(&[counters::CONSENSUS_SYNC_REQ_CALLBACK])
                .inc();
            format_err!(
                "Consensus sync request callback error - failed to send following msg: {:?}",
                failed_msg
            )
        })
    }

    fn send_initialization_callback(
        cb: oneshot::Sender<Result<()>>,
        msg: Result<()>,
    ) -> Result<()> {
        cb.send(msg).map_err(|failed_msg| {
            counters::FAILED_CHANNEL_SEND
                .with_label_values(&[counters::WAYPOINT_INIT_CALLBACK])
                .inc();
            format_err!(
                "Waypoint initialization callback error - failed to send following msg: {:?}",
                failed_msg
            )
        })
    }
}
