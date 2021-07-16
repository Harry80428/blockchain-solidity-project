// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    client::Client,
    error::{Error, Result, WaitForTransactionError},
    request::MethodRequest,
    response::{MethodResponse, Response},
    verifying_client::{
        methods::{verify_latest_li_matches_state, VerifyingBatch},
        state_store::{Storage, TrustedStateStore},
    },
};
use diem_crypto::hash::{CryptoHash, HashValue};
use diem_json_rpc_types::views::{
    AccountView, CurrencyInfoView, EventView, MetadataView, TransactionView,
};
use diem_types::{
    account_address::AccountAddress,
    event::EventKey,
    proof::{AccumulatorConsistencyProof, TransactionAccumulatorSummary},
    state_proof::StateProof,
    transaction::{SignedTransaction, Transaction, Version},
    trusted_state::TrustedState,
    waypoint::Waypoint,
};
use std::{
    convert::TryFrom,
    fmt::Debug,
    sync::{Arc, RwLock},
    time::Duration,
};

// TODO(philiphayes): figure out retry strategy
// TODO(philiphayes): real on-disk waypoint persistence
// TODO(philiphayes): fill out rest of the methods
// TODO(philiphayes): all clients should validate chain id (allow users to trust-on-first-use or pre-configure)
// TODO(philiphayes): we could abstract the async client so VerifyingClient takes a dyn Trait?

/// The `VerifyingClient` is a [Diem JSON-RPC client] that verifies Diem's
/// cryptographic proofs when it makes API calls.
///
/// ## Concurrency
///
/// When issuing multiple concurrent requests with the `VerifyingClient`, we guarantee:
///
/// 1. Each response batch is fulfilled and verified at a ledger version that
///    is greater than or equal to the current trusted version _at the time we
///    made the request batch_, though not necessarily the globally most
///    recent trusted ledger version.
///
/// 2. Requests made serially within a single thread of execution appear
///    strictly ordered, i.e., they were fulfilled and verified at
///    monotonically increasing ledger versions (`v1 <= v2 <= ...`).
///
/// Consequently, without any other effort, multiple concurrent requests may have
/// responses that appear inconsistent or out-of-order. For example, naively
/// making concurrent `get_account(..)` requests will (most likely) show accounts
/// at different ledger versions; even further, the first response you receive may
/// show a more recent ledger version than the second response.
///
/// To avoid this issue, users should pin a concurrent batch of requests to the
/// same ledger version if they want to avoid an inconsistent ledger view.
///
/// [Diem JSON-RPC client]: https://github.com/diem/diem/blob/master/json-rpc/json-rpc-spec.md
#[derive(Clone, Debug)]
pub struct VerifyingClient<S> {
    inner: Client,
    trusted_state_store: Arc<RwLock<TrustedStateStore<S>>>,
}

impl<S: Storage> VerifyingClient<S> {
    // TODO(philiphayes): construct the client ourselves? we probably want to
    // control the retries out here. For example, during sync, if we get a stale
    // state proof the retry logic should include that and not just fail immediately.
    pub fn new(inner: Client, storage: S) -> Result<Self> {
        let trusted_state_store = TrustedStateStore::new(storage)?;
        Ok(Self {
            inner,
            trusted_state_store: Arc::new(RwLock::new(trusted_state_store)),
        })
    }

    pub fn new_with_state(inner: Client, trusted_state: TrustedState, storage: S) -> Self {
        let trusted_state_store = TrustedStateStore::new_with_state(trusted_state, storage);
        Self {
            inner,
            trusted_state_store: Arc::new(RwLock::new(trusted_state_store)),
        }
    }

    /// Get a snapshot of our current trusted ledger [`Version`].
    pub fn version(&self) -> Version {
        self.trusted_state_store.read().unwrap().version()
    }

    /// Get a snapshot of our current trusted [`Waypoint`].
    pub fn waypoint(&self) -> Waypoint {
        self.trusted_state_store.read().unwrap().waypoint()
    }

    /// Get a snapshot of our current [`TrustedState`].
    pub fn trusted_state(&self) -> TrustedState {
        self.trusted_state_store
            .read()
            .unwrap()
            .trusted_state()
            .clone()
    }

    pub async fn wait_for_signed_transaction(
        &self,
        txn: &SignedTransaction,
        timeout: Option<Duration>,
        delay: Option<Duration>,
    ) -> Result<Response<TransactionView>, WaitForTransactionError> {
        let response = self
            .wait_for_transaction(
                txn.sender(),
                txn.sequence_number(),
                txn.expiration_timestamp_secs(),
                Transaction::UserTransaction(txn.clone()).hash(),
                timeout,
                delay,
            )
            .await?;

        if !response.inner().vm_status.is_executed() {
            return Err(WaitForTransactionError::TransactionExecutionFailed(
                response.into_inner(),
            ));
        }

        Ok(response)
    }

    pub async fn wait_for_transaction(
        &self,
        address: AccountAddress,
        seq: u64,
        expiration_time_secs: u64,
        txn_hash: HashValue,
        timeout: Option<Duration>,
        delay: Option<Duration>,
    ) -> Result<Response<TransactionView>, WaitForTransactionError> {
        const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
        const DEFAULT_DELAY: Duration = Duration::from_millis(50);

        let start = std::time::Instant::now();
        while start.elapsed() < timeout.unwrap_or(DEFAULT_TIMEOUT) {
            let txn_resp = self
                .get_account_transaction(address, seq, true)
                .await
                .map_err(WaitForTransactionError::GetTransactionError)?;

            let (maybe_txn, state) = txn_resp.into_parts();

            if let Some(txn) = maybe_txn {
                if txn.hash != txn_hash {
                    return Err(WaitForTransactionError::TransactionHashMismatchError(txn));
                }

                return Ok(Response::new(txn, state));
            }

            if expiration_time_secs <= state.timestamp_usecs / 1_000_000 {
                return Err(WaitForTransactionError::TransactionExpired);
            }

            tokio::time::sleep(delay.unwrap_or(DEFAULT_DELAY)).await;
        }

        Err(WaitForTransactionError::Timeout)
    }

    /// Issue `get_state_proof` requests until we successfully sync to the remote
    /// node's current version (unless we experience a verification error or other
    /// I/O error).
    pub async fn sync(&self) -> Result<()> {
        // TODO(philiphayes): retries
        while self.sync_one_step().await? {}
        Ok(())
    }

    /// Issue a single `get_state_proof` request and try to verify it. Returns
    /// `Ok(true)` if, after verification, we still need to sync more. Returns
    /// `Ok(false)` if we have finished syncing.
    pub async fn sync_one_step(&self) -> Result<bool> {
        let request_trusted_state = self.trusted_state();
        let need_initial_accumulator = request_trusted_state.accumulator_summary().is_none();
        let current_version = request_trusted_state.version();

        // request a state proof from remote and an optional initial transaction
        // accumulator if we're just starting out from an epoch waypoint.
        let (state_proof, maybe_accumulator) = self
            .get_state_proof_and_maybe_accumulator(current_version, need_initial_accumulator)
            .await?;

        // try to ratchet our trusted state using the state proof
        self.verify_and_ratchet(
            &request_trusted_state,
            &state_proof,
            maybe_accumulator.as_ref(),
        )?;

        // return true if we need to sync more epoch changes
        Ok(state_proof.epoch_changes().more)
    }

    async fn get_state_proof_and_maybe_accumulator(
        &self,
        current_version: Version,
        need_initial_accumulator: bool,
    ) -> Result<(StateProof, Option<TransactionAccumulatorSummary>)> {
        let (state_proof_view, state, maybe_consistency_proof_view) = if !need_initial_accumulator {
            // just request the state proof, since we don't need the initial accumulator
            let (state_proof_view, state) = self
                .inner
                .get_state_proof(current_version)
                .await?
                .into_parts();
            (state_proof_view, state, None)
        } else {
            // request both a state proof and an initial accumulator from genesis
            // to the current version.
            let batch = vec![
                MethodRequest::get_accumulator_consistency_proof(None, Some(current_version)),
                MethodRequest::get_state_proof(current_version),
            ];
            let resps = self.inner.batch(batch).await?;
            let mut resps_iter = resps.into_iter();

            // inner client guarantees us that # responses matches # requests
            let (resp1, state1) = resps_iter.next().unwrap()?.into_parts();
            let (resp2, state2) = resps_iter.next().unwrap()?.into_parts();

            if state1 != state2 {
                return Err(Error::rpc_response(format!(
                    "expected batch response states equal: state1={:?}, state2={:?}",
                    state1, state2
                )));
            }

            let consistency_proof_view = resp1.try_into_get_accumulator_consistency_proof()?;
            let state_proof_view = resp2.try_into_get_state_proof()?;
            (state_proof_view, state1, Some(consistency_proof_view))
        };

        // deserialize responses
        let state_proof = StateProof::try_from(&state_proof_view).map_err(Error::decode)?;
        let maybe_accumulator = maybe_consistency_proof_view
            .map(|consistency_proof_view| {
                let consistency_proof =
                    AccumulatorConsistencyProof::try_from(&consistency_proof_view)
                        .map_err(Error::decode)?;
                TransactionAccumulatorSummary::try_from_genesis_proof(
                    consistency_proof,
                    current_version,
                )
                .map_err(Error::invalid_proof)
            })
            .transpose()?;

        // check the response metadata matches the state proof
        verify_latest_li_matches_state(state_proof.latest_ledger_info(), &state)?;

        Ok((state_proof, maybe_accumulator))
    }

    /// Verify and ratchet forward a request's trusted state using a state proof
    /// and potentially persisting the new trusted state if it is the newest.
    pub fn verify_and_ratchet(
        &self,
        request_trusted_state: &TrustedState,
        state_proof: &StateProof,
        maybe_accumulator: Option<&TransactionAccumulatorSummary>,
    ) -> Result<()> {
        // Verify the response's state proof starting from the trusted state when
        // we first made the request. If successful, this means the potential new
        // trusted state is at least a descendent of the request trusted state,
        // though not necessarily the globally most-recent trusted state.
        let change = request_trusted_state
            .verify_and_ratchet(state_proof, maybe_accumulator)
            .map_err(Error::invalid_proof)?;

        self.ratchet(change.new_state())
    }

    /// Try to compare-and-swap a verified trusted state change into the state store.
    /// If the client is issuing muiltiple concurrent requests, the potential
    /// new trusted state might not be newer than the current trusted state,
    /// in which case we just don't persist this change.
    pub fn ratchet(&self, new_state: Option<TrustedState>) -> Result<()> {
        if let Some(new_state) = new_state {
            self.trusted_state_store
                .write()
                .unwrap()
                .ratchet(new_state)?;
        }
        Ok(())
    }

    /// Submit a new signed user transaction.
    ///
    /// Note: we don't verify anything about the server's response here. If the
    /// server is behaving maliciously, they can claim our transaction is
    /// malformed when it is not, they can broadcast our valid transaction but
    /// tell us it is too old, or they can accept our invalid transaction without
    /// giving us any indication that it's bad.
    ///
    /// Unfortunately, there's nothing for us to verify that their response is
    /// correct or not; the only way to get around this is by broadcasting our
    /// transaction to multiple different servers. As long as one is honest, our
    /// valid transaction will eventually be committed. This client handles a
    /// connection to a single server, so the broadcasting needs to happen at a
    /// higher layer.
    pub async fn submit(&self, txn: &SignedTransaction) -> Result<Response<()>> {
        self.request(MethodRequest::submit(txn).map_err(Error::request)?)
            .await?
            .and_then(MethodResponse::try_into_submit)
    }

    pub async fn get_metadata_by_version(
        &self,
        version: Version,
    ) -> Result<Response<MetadataView>> {
        self.request(MethodRequest::get_metadata_by_version(version))
            .await?
            .and_then(MethodResponse::try_into_get_metadata)
    }

    pub async fn get_metadata(&self) -> Result<Response<MetadataView>> {
        self.request(MethodRequest::get_metadata())
            .await?
            .and_then(MethodResponse::try_into_get_metadata)
    }

    pub async fn get_account(
        &self,
        address: AccountAddress,
    ) -> Result<Response<Option<AccountView>>> {
        self.request(MethodRequest::get_account(address))
            .await?
            .and_then(MethodResponse::try_into_get_account)
    }

    pub async fn get_account_by_version(
        &self,
        address: AccountAddress,
        version: Version,
    ) -> Result<Response<Option<AccountView>>> {
        self.request(MethodRequest::get_account_by_version(address, version))
            .await?
            .and_then(MethodResponse::try_into_get_account)
    }

    pub async fn get_transactions(
        &self,
        start_version: Version,
        limit: u64,
        include_events: bool,
    ) -> Result<Response<Vec<TransactionView>>> {
        self.request(MethodRequest::get_transactions(
            start_version,
            limit,
            include_events,
        ))
        .await?
        .and_then(MethodResponse::try_into_get_transactions)
    }

    pub async fn get_account_transaction(
        &self,
        address: AccountAddress,
        seq_num: u64,
        include_events: bool,
    ) -> Result<Response<Option<TransactionView>>> {
        self.request(MethodRequest::get_account_transaction(
            address,
            seq_num,
            include_events,
        ))
        .await?
        .and_then(MethodResponse::try_into_get_account_transaction)
    }

    pub async fn get_account_transactions(
        &self,
        address: AccountAddress,
        start_seq_num: u64,
        limit: u64,
        include_events: bool,
    ) -> Result<Response<Vec<TransactionView>>> {
        self.request(MethodRequest::get_account_transactions(
            address,
            start_seq_num,
            limit,
            include_events,
        ))
        .await?
        .and_then(MethodResponse::try_into_get_account_transactions)
    }

    pub async fn get_events(
        &self,
        key: EventKey,
        start_seq: u64,
        limit: u64,
    ) -> Result<Response<Vec<EventView>>> {
        self.request(MethodRequest::get_events(key, start_seq, limit))
            .await?
            .and_then(MethodResponse::try_into_get_events)
    }

    pub async fn get_currencies(&self) -> Result<Response<Vec<CurrencyInfoView>>> {
        self.request(MethodRequest::get_currencies())
            .await?
            .and_then(MethodResponse::try_into_get_currencies)
    }

    pub async fn get_network_status(&self) -> Result<Response<u64>> {
        self.request(MethodRequest::get_network_status())
            .await?
            .and_then(MethodResponse::try_into_get_network_status)
    }

    /// Send a single request via `VerifyingClient::batch`.
    pub async fn request(&self, request: MethodRequest) -> Result<Response<MethodResponse>> {
        let mut responses = self.batch(vec![request]).await?.into_iter();
        let response = match responses.next() {
            Some(response) => response,
            None => {
                return Err(Error::rpc_response(
                    "expected one response, received empty response batch",
                ))
            }
        };
        let rest = responses.as_slice();
        if !rest.is_empty() {
            return Err(Error::rpc_response(format!(
                "expected one response, received unexpected responses: {:?}",
                rest,
            )));
        }
        response
    }

    pub fn actual_batch_size(requests: &[MethodRequest]) -> usize {
        VerifyingBatch::from_batch(requests.to_vec()).num_subrequests()
    }

    pub async fn batch(
        &self,
        requests: Vec<MethodRequest>,
    ) -> Result<Vec<Result<Response<MethodResponse>>>> {
        let request_trusted_state = self.trusted_state();
        let request_version = request_trusted_state.version();

        // if we haven't built an accumulator yet, we need to do a sync first.
        if request_trusted_state.accumulator_summary().is_none() {
            // TODO(philiphayes): sync fallback
            return Err(Error::unknown(
                "our client is too far behind, we need to sync",
            ));
        }

        // transform each request into verifying sub-request batches
        let batch = VerifyingBatch::from_batch(requests);
        // flatten and collect sub-request batches into flat list of requests
        let requests = batch.collect_requests(request_version);
        // actually send the batch
        let responses = self.inner.batch(requests).await?;
        // validate responses and state proof w.r.t. request trusted state
        let (new_state, responses) = batch.validate_responses(&request_trusted_state, responses)?;
        // try to ratchet our trusted state in our state store
        self.ratchet(new_state)?;

        Ok(responses)
    }
}
