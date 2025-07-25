//! # Signer storage
//!
//! This module contains the `Read` and `Write` traits representing
//! the interface between the signer and their internal database.
//!
//! The canonical implementation of these traits is the [`postgres::PgStore`]
//! allowing the signer to use a Postgres database to store data.

#[cfg(any(test, feature = "testing"))]
pub mod memory;
pub mod model;
pub mod postgres;
pub mod sqlx;
pub mod util;

use std::collections::BTreeSet;
use std::future::Future;

use blockstack_lib::types::chainstate::StacksBlockId;

use crate::bitcoin::utxo::SignerUtxo;
use crate::bitcoin::validation::DepositRequestReport;
use crate::bitcoin::validation::WithdrawalRequestReport;
use crate::error::Error;
use crate::keys::PublicKey;
use crate::keys::PublicKeyXOnly;
use crate::storage::model::BitcoinBlockHeight;
use crate::storage::model::CompletedDepositEvent;
use crate::storage::model::WithdrawalAcceptEvent;
use crate::storage::model::WithdrawalRejectEvent;

/// Represents a handle to an ongoing database transaction.
pub trait TransactionHandle: DbRead + DbWrite + Send {
    /// Commits the transaction.
    fn commit(self) -> impl Future<Output = Result<(), Error>> + Send;
    /// Rolls back the transaction.
    fn rollback(self) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Trait for storage backends that support initiating transactions.
/// The returned transaction object itself implements `DbRead` and `DbWrite`.
pub trait Transactable {
    /// The type of the transaction object. It must implement `DbRead`, `DbWrite`,
    /// and `TransactionHandle`. The lifetime `'a` ties the transaction to the
    /// lifetime of the `Transactable` implementor (e.g., the `PgStore`).
    type Tx<'a>: DbRead + DbWrite + TransactionHandle + Sync + Send + 'a
    where
        Self: 'a;

    /// Begins a new database transaction.
    fn begin_transaction(&self) -> impl Future<Output = Result<Self::Tx<'_>, Error>> + Send;
}

/// Represents the ability to read data from the signer storage.
pub trait DbRead {
    /// Get the bitcoin block with the given block hash.
    fn get_bitcoin_block(
        &self,
        block_hash: &model::BitcoinBlockHash,
    ) -> impl Future<Output = Result<Option<model::BitcoinBlock>, Error>> + Send;

    /// Get the stacks block with the given block hash.
    fn get_stacks_block(
        &self,
        block_hash: &model::StacksBlockHash,
    ) -> impl Future<Output = Result<Option<model::StacksBlock>, Error>> + Send;

    /// Get the bitcoin canonical chain tip.
    #[cfg(any(test, feature = "testing"))]
    fn get_bitcoin_canonical_chain_tip(
        &self,
    ) -> impl Future<Output = Result<Option<model::BitcoinBlockHash>, Error>> + Send;

    /// Get the bitcoin canonical chain tip.
    #[cfg(any(test, feature = "testing"))]
    fn get_bitcoin_canonical_chain_tip_ref(
        &self,
    ) -> impl Future<Output = Result<Option<model::BitcoinBlockRef>, Error>> + Send;

    /// Get the stacks chain tip, defined as the highest stacks block
    /// confirmed by the bitcoin chain tip.
    fn get_stacks_chain_tip(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
    ) -> impl Future<Output = Result<Option<model::StacksBlock>, Error>> + Send;

    /// Get pending deposit requests
    ///
    /// These are deposit requests that have been added to our database but
    /// where the current signer has not made a decision on whether they
    /// will sign for the deposit and sweep in the funds.
    fn get_pending_deposit_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> impl Future<Output = Result<Vec<model::DepositRequest>, Error>> + Send;

    /// Get pending deposit requests that have been accepted by at least
    /// `signatures_required` signers and has no responses.
    ///
    /// For an individual signer, 'accepted' means their blocklist client
    /// hasn't blocked the request and they are part of the signing set
    /// that generated the aggregate key locking the deposit.
    fn get_pending_accepted_deposit_requests(
        &self,
        chain_tip: &model::BitcoinBlockRef,
        context_window: u16,
        signatures_required: u16,
    ) -> impl Future<Output = Result<Vec<model::DepositRequest>, Error>> + Send;

    /// Check whether we have a record of the deposit request in our
    /// database.
    fn deposit_request_exists(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// This function returns a deposit request report that does the
    /// following:
    ///
    /// 1. Check that the current signer accepted by the deposit request.
    /// 2. Check that this signer can contribute a share of the final
    ///    signature.
    /// 3. Check that the deposit transaction is in a block on the bitcoin
    ///    blockchain identified by the given chain tip.
    /// 4. Check that the deposit has not been included by a sweep
    ///    transaction that has been confirmed by block on the bitcoin
    ///    blockchain identified by the given chain tip.
    /// 5. Return the locktime embedded of the reclaim script in the
    ///    deposit request. It does not check of the value.
    ///
    ///  `Ok(None)` is returned if we do not have a record of the deposit
    /// request.
    fn get_deposit_request_report(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        txid: &model::BitcoinTxId,
        output_index: u32,
        signer_public_key: &PublicKey,
    ) -> impl Future<Output = Result<Option<DepositRequestReport>, Error>> + Send;

    /// Get signer decisions for a deposit request
    fn get_deposit_signers(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
    ) -> impl Future<Output = Result<Vec<model::DepositSigner>, Error>> + Send;

    /// Get all the deposit decisions for the given signer in the given window
    /// of blocks.
    fn get_deposit_signer_decisions(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> impl Future<Output = Result<Vec<model::DepositSigner>, Error>> + Send;

    /// Get all the withdrawal decisions for the given signer in the given window
    /// of blocks.
    fn get_withdrawal_signer_decisions(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> impl Future<Output = Result<Vec<model::WithdrawalSigner>, Error>> + Send;

    /// Returns whether the given `signer_public_key` can provide signature
    /// shares for the deposit transaction.
    ///
    /// This function works by identifying whether the `signer_public_key`
    /// was part of the signer set associated with the public key that was
    /// used to lock the deposit
    ///
    /// This returns None if the deposit request cannot be found in the
    /// database.
    fn can_sign_deposit_tx(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
        signer_public_key: &PublicKey,
    ) -> impl Future<Output = Result<Option<bool>, Error>> + Send;

    /// Get signer decisions for a withdrawal request
    fn get_withdrawal_signers(
        &self,
        request_id: u64,
        block_hash: &model::StacksBlockHash,
    ) -> impl Future<Output = Result<Vec<model::WithdrawalSigner>, Error>> + Send;

    /// Get pending withdrawal requests
    ///
    /// These are withdrawal requests that have been added to our database
    /// but where the current signer has not made a decision on whether
    /// they will sweep out the withdrawal funds and sweep transaction.
    fn get_pending_withdrawal_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
        signer_public_key: &PublicKey,
    ) -> impl Future<Output = Result<Vec<model::WithdrawalRequest>, Error>> + Send;

    /// This function returns withdrawal requests filtered by a portion of the
    /// consensus critera defined in #741.
    ///
    /// ## Filter Criteria
    ///
    /// 1. The withdrawal request transaction (`create-withdrawal-request`
    ///    contract call) is confirmed in a block on the canonical stacks
    ///    blockchain.
    /// 2. The withdrawal request has not been included in a sweep transaction
    ///    that has been confirmed in a block on the canonical bitcoin
    ///    blockchain.
    /// 3. The withdrawal request has been approved by at least
    ///    `signature_threshold` signers.
    /// 4. The withdrawal request bitcoin block height is not older than the
    ///    given `min_bitcoin_height` (_inclusive_).
    /// 5. There is no canonically confirmed withdrawal request rejection event
    ///    (`reject-withdrawal-request` contract call) for the request.
    ///
    /// ## Notes
    ///
    /// -  This does does not filter `signature_threshold` on that the approving
    ///    signers are part of the current signer set and this parameter is only
    ///    used as a pre-filter as the votes themselves are generally also
    ///    needed separately. Use
    ///    [`DbRead::get_withdrawal_request_signer_votes`] to fetch the votes
    ///    and perform this verification separately.
    fn get_pending_accepted_withdrawal_requests(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        stacks_chain_tip: &model::StacksBlockHash,
        min_bitcoin_height: BitcoinBlockHeight,
        signature_threshold: u16,
    ) -> impl Future<Output = Result<Vec<model::WithdrawalRequest>, Error>> + Send;

    /// Get pending rejected withdrawal requests that have failed but are not
    /// rejected yet
    fn get_pending_rejected_withdrawal_requests(
        &self,
        chain_tip: &model::BitcoinBlockRef,
        context_window: u16,
    ) -> impl Future<Output = Result<Vec<model::WithdrawalRequest>, Error>> + Send;

    /// This function returns a withdrawal request report that does the
    /// following:
    ///
    /// 1. Check that the current signer accepted by the withdrawal
    ///    request.
    /// 2. Check that the transaction that created the withdrawal is in a
    ///    stacks block on the stacks blockchain identified by the given
    ///    chain tip.
    /// 3. Check that the withdrawal has not been included in a sweep
    ///    transaction that has been confirmed by block on the bitcoin
    ///    blockchain identified by the given chain tip.
    ///
    /// `Ok(None)` is returned if we do not have a record of the withdrawal
    /// request or if the withdrawal request is confirmed on a stacks block
    /// that we do not know about
    fn get_withdrawal_request_report(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        stacks_chain_tip: &model::StacksBlockHash,
        id: &model::QualifiedRequestId,
        signer_public_key: &PublicKey,
    ) -> impl Future<Output = Result<Option<WithdrawalRequestReport>, Error>> + Send;

    /// This function returns the total amount of BTC (in sats) that has
    /// been swept out and confirmed on the bitcoin blockchain identified
    /// by the given chain tip and context window.
    fn compute_withdrawn_total(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
    ) -> impl Future<Output = Result<u64, Error>> + Send;

    /// Get bitcoin blocks that include a particular transaction
    fn get_bitcoin_blocks_with_transaction(
        &self,
        txid: &model::BitcoinTxId,
    ) -> impl Future<Output = Result<Vec<model::BitcoinBlockHash>, Error>> + Send;

    /// Returns whether the given block ID is stored.
    fn stacks_block_exists(
        &self,
        block_id: StacksBlockId,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Return the applicable DKG shares for the
    /// given aggregate key
    fn get_encrypted_dkg_shares<X>(
        &self,
        aggregate_key: X,
    ) -> impl Future<Output = Result<Option<model::EncryptedDkgShares>, Error>> + Send
    where
        X: Into<PublicKeyXOnly> + Send;

    /// Return the most recent DKG shares, and return None if the table is
    /// empty.
    fn get_latest_encrypted_dkg_shares(
        &self,
    ) -> impl Future<Output = Result<Option<model::EncryptedDkgShares>, Error>> + Send;

    /// Return the most recent DKG shares that have passed verification,
    /// and return None if no such shares exist.
    fn get_latest_verified_dkg_shares(
        &self,
    ) -> impl Future<Output = Result<Option<model::EncryptedDkgShares>, Error>> + Send;

    /// Returns the number of non-failed DKG shares entries in the database.
    fn get_encrypted_dkg_shares_count(&self) -> impl Future<Output = Result<u32, Error>> + Send;

    /// Return the latest rotate-keys transaction confirmed by the given `chain-tip`.
    #[cfg(any(test, feature = "testing"))]
    fn get_last_key_rotation(
        &self,
        chain_tip: &model::BitcoinBlockHash,
    ) -> impl Future<Output = Result<Option<model::KeyRotationEvent>, Error>> + Send;

    /// Checks if a key rotation exists on the canonical chain
    fn key_rotation_exists(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        signer_set: &BTreeSet<PublicKey>,
        aggregate_key: &PublicKey,
        signatures_required: u16,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Get the last 365 days worth of the signers' `scriptPubkey`s. If no
    /// keys are available within the last 365, then return the most recent
    /// key.
    fn get_signers_script_pubkeys(
        &self,
    ) -> impl Future<Output = Result<Vec<model::Bytes>, Error>> + Send;

    /// Get the outstanding signer UTXO.
    ///
    /// Under normal conditions, the signer will have only one UTXO they
    /// can spend. The specific UTXO we want is one such that:
    /// 1. The transaction is in a block on the canonical bitcoin
    ///    blockchain.
    /// 2. The output is the first output in the transaction.
    /// 3. The output is unspent. It is possible for more than one
    ///    transaction within the same block to satisfy points 1-3, but if
    ///    the signers have one or more transactions within a block,
    ///    exactly one output satisfying points 1-2 will be unspent.
    fn get_signer_utxo(
        &self,
        chain_tip: &model::BitcoinBlockHash,
    ) -> impl Future<Output = Result<Option<SignerUtxo>, Error>> + Send;

    /// For the given outpoint and aggregate key, get the list all signer
    /// votes in the signer set.
    fn get_deposit_request_signer_votes(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
        aggregate_key: &PublicKey,
    ) -> impl Future<Output = Result<model::SignerVotes, Error>> + Send;

    /// For the given withdrawal request identifier, and aggregate key, get
    /// the list for how the signers voted against the request.
    fn get_withdrawal_request_signer_votes(
        &self,
        id: &model::QualifiedRequestId,
        aggregate_key: &PublicKey,
    ) -> impl Future<Output = Result<model::SignerVotes, Error>> + Send;

    /// Check for whether  the given block hash is in the database.
    fn is_known_bitcoin_block_hash(
        &self,
        block_hash: &model::BitcoinBlockHash,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Check that the given block hash is included in the canonical
    /// bitcoin blockchain, where the canonical blockchain is identified by
    /// the given `chain_tip`.
    fn in_canonical_bitcoin_blockchain(
        &self,
        chain_tip: &model::BitcoinBlockRef,
        block_ref: &model::BitcoinBlockRef,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Checks whether the given scriptPubKey is one of the signers'
    /// scriptPubKeys.
    fn is_signer_script_pub_key(
        &self,
        script: &model::ScriptPubKey,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Returns whether the identified withdrawal may be included in a
    /// sweep transaction that is in the bitcoin mempool.
    ///
    /// # Notes
    ///
    /// At this time, we cannot use the bitcoin mempool for this
    /// information since we cannot match withdrawals with transaction
    /// outputs without a lot of computational effort. Instead, we use our
    /// database, where the query is straightforward. The tables that are
    /// be able to answer whether a withdrawal is potentially in the
    /// mempool are populated during validation of pre-sign requests.
    fn is_withdrawal_inflight(
        &self,
        id: &model::QualifiedRequestId,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Returns whether we should consider the withdrawal active. A
    /// withdrawal request is considered active if there is a reasonable
    /// risk of the withdrawal being confirmed from a fork of blocks less
    /// than `min_confirmations`.
    fn is_withdrawal_active(
        &self,
        id: &model::QualifiedRequestId,
        bitcoin_chain_tip: &model::BitcoinBlockRef,
        min_confirmations: u64,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Fetch bitcoin transactions that have fulfilled a deposit request
    /// but where we have not confirmed a stacks transaction finalizing the
    /// request.
    ///
    /// These requests only require a `complete-deposit` contract call
    /// before they are considered fulfilled.
    fn get_swept_deposit_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
    ) -> impl Future<Output = Result<Vec<model::SweptDepositRequest>, Error>> + Send;

    /// Fetch bitcoin transactions that have fulfilled a withdrawal request
    /// but where we have not confirmed a stacks transaction finalizing the
    /// request.
    ///
    /// These requests only require a `accept-withdrawal-request` contract
    /// call before they are considered fulfilled.
    fn get_swept_withdrawal_requests(
        &self,
        chain_tip: &model::BitcoinBlockHash,
        context_window: u16,
    ) -> impl Future<Output = Result<Vec<model::SweptWithdrawalRequest>, Error>> + Send;

    /// Get the deposit request given the transaction id and output index.
    fn get_deposit_request(
        &self,
        txid: &model::BitcoinTxId,
        output_index: u32,
    ) -> impl Future<Output = Result<Option<model::DepositRequest>, Error>> + Send;

    /// Get the bitcoin sighash output.
    fn will_sign_bitcoin_tx_sighash(
        &self,
        sighash: &model::SigHash,
    ) -> impl Future<Output = Result<Option<(bool, PublicKeyXOnly)>, Error>> + Send;
}

/// Represents the ability to write data to the signer storage.
pub trait DbWrite {
    /// Write a bitcoin block.
    fn write_bitcoin_block(
        &self,
        block: &model::BitcoinBlock,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write a stacks block.
    fn write_stacks_block(
        &self,
        block: &model::StacksBlock,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write a deposit request.
    fn write_deposit_request(
        &self,
        deposit_request: &model::DepositRequest,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write many deposit requests.
    fn write_deposit_requests(
        &self,
        deposit_requests: Vec<model::DepositRequest>,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write a withdrawal request.
    fn write_withdrawal_request(
        &self,
        request: &model::WithdrawalRequest,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write a signer decision for a deposit request.
    fn write_deposit_signer_decision(
        &self,
        decision: &model::DepositSigner,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write a signer decision for a withdrawal request.
    fn write_withdrawal_signer_decision(
        &self,
        decision: &model::WithdrawalSigner,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write a connection between a bitcoin block and a transaction
    fn write_bitcoin_transaction(
        &self,
        bitcoin_transaction: &model::BitcoinTxRef,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the bitcoin transactions to the data store.
    fn write_bitcoin_transactions(
        &self,
        txs: Vec<model::BitcoinTxRef>,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the stacks block ids and their parent block ids.
    fn write_stacks_block_headers(
        &self,
        headers: Vec<model::StacksBlock>,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write encrypted DKG shares
    fn write_encrypted_dkg_shares(
        &self,
        shares: &model::EncryptedDkgShares,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write rotate-keys transaction
    fn write_rotate_keys_transaction(
        &self,
        key_rotation: &model::KeyRotationEvent,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the withdrawal-reject event to the database.
    fn write_withdrawal_reject_event(
        &self,
        event: &WithdrawalRejectEvent,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the withdrawal-accept event to the database.
    fn write_withdrawal_accept_event(
        &self,
        event: &WithdrawalAcceptEvent,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the completed deposit event to the database.
    fn write_completed_deposit_event(
        &self,
        event: &CompletedDepositEvent,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the bitcoin transaction output to the database.
    fn write_tx_output(
        &self,
        output: &model::TxOutput,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the withdrawal bitcoin transaction output to the database.
    fn write_withdrawal_tx_output(
        &self,
        output: &model::WithdrawalTxOutput,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the bitcoin transaction input to the database.
    fn write_tx_prevout(
        &self,
        prevout: &model::TxPrevout,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the bitcoin transactions sighashes to the database.
    fn write_bitcoin_txs_sighashes(
        &self,
        sighashes: &[model::BitcoinTxSigHash],
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Write the bitcoin withdrawals outputs to the database.
    fn write_bitcoin_withdrawals_outputs(
        &self,
        withdrawals_outputs: &[model::BitcoinWithdrawalOutput],
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Marks the stored DKG shares for the provided aggregate key as revoked
    /// and thus should no longer be used.
    ///
    /// This can be due to a failed DKG process, the key having been
    /// compromised, or any other reason that would require the shares for the
    /// provided aggregate key to not be used in the signing of transactions.
    fn revoke_dkg_shares<X>(
        &self,
        aggregate_key: X,
    ) -> impl Future<Output = Result<bool, Error>> + Send
    where
        X: Into<PublicKeyXOnly> + Send;

    /// Marks the stored DKG shares as verified, meaning that the shares have
    /// been used to sign a transaction input spending a UTXO locked by itself.
    fn verify_dkg_shares<X>(
        &self,
        aggregate_key: X,
    ) -> impl Future<Output = Result<bool, Error>> + Send
    where
        X: Into<PublicKeyXOnly> + Send;
}
