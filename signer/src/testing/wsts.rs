//! Test utilities for running a wsts signer and coordinator.

use std::collections::{BTreeMap, BTreeSet};

use crate::message;
use crate::network;
use crate::storage;
use crate::storage::model;
use crate::wsts_state_machine;

use wsts::state_machine::coordinator;
use wsts::state_machine::coordinator::frost;

use crate::ecdsa::SignEcdsa as _;
use crate::network::MessageTransfer as _;

use wsts::state_machine::coordinator::Coordinator as _;
use wsts::state_machine::StateMachine as _;

/// Signer info
#[derive(Debug, Clone)]
pub struct SignerInfo {
    /// Private key of the signer
    pub signer_private_key: p256k1::scalar::Scalar,
    /// Public keys of all signers in the signer set
    pub signer_public_keys: BTreeSet<p256k1::ecdsa::PublicKey>,
}

/// Generate a new signer set
pub fn generate_signer_info<Rng: rand::RngCore + rand::CryptoRng>(
    rng: &mut Rng,
    num_signers: usize,
) -> Vec<SignerInfo> {
    let signer_keys: BTreeMap<_, _> = (0..num_signers)
        .map(|_| {
            let private = p256k1::scalar::Scalar::random(rng);
            let public =
                p256k1::ecdsa::PublicKey::new(&private).expect("failed to generate public key");

            (public, private)
        })
        .collect();

    let signer_public_keys: BTreeSet<_> = signer_keys.keys().cloned().collect();

    signer_keys
        .into_values()
        .map(|signer_private_key| SignerInfo {
            signer_private_key,
            signer_public_keys: signer_public_keys.clone(),
        })
        .collect()
}

/// Test coordinator that can operate over an `in_memory` network
pub struct Coordinator {
    network: network::in_memory::MpmcBroadcaster,
    wsts_coordinator: frost::Coordinator<wsts::v2::Aggregator>,
    private_key: p256k1::scalar::Scalar,
    num_signers: u32,
}

impl Coordinator {
    /// Construct a new coordinator
    pub fn new(
        network: network::in_memory::MpmcBroadcaster,
        signer_info: SignerInfo,
        threshold: u32,
    ) -> Self {
        let num_signers = signer_info.signer_public_keys.len().try_into().unwrap();
        let message_private_key = signer_info.signer_private_key;
        let signer_public_keys: hashbrown::HashMap<u32, _> = signer_info
            .signer_public_keys
            .into_iter()
            .enumerate()
            .map(|(idx, key)| {
                (
                    idx.try_into().unwrap(),
                    (&p256k1::point::Compressed::from(key.to_bytes()))
                        .try_into()
                        .expect("failed to convert public key"),
                )
            })
            .collect();
        let num_keys = num_signers;
        let dkg_threshold = num_keys;
        let signer_key_ids = (0..num_signers)
            .map(|signer_id| (signer_id, std::iter::once(signer_id).collect()))
            .collect();
        let config = wsts::state_machine::coordinator::Config {
            num_signers,
            num_keys,
            threshold,
            dkg_threshold,
            message_private_key,
            dkg_public_timeout: None,
            dkg_private_timeout: None,
            dkg_end_timeout: None,
            nonce_timeout: None,
            sign_timeout: None,
            signer_key_ids,
            signer_public_keys,
        };

        let wsts_coordinator = frost::Coordinator::new(config);

        Self {
            network,
            wsts_coordinator,
            private_key: message_private_key,
            num_signers,
        }
    }

    /// Run DKG
    pub async fn run_dkg(
        &mut self,
        bitcoin_chain_tip: bitcoin::BlockHash,
        txid: bitcoin::Txid,
    ) -> p256k1::point::Point {
        self.wsts_coordinator
            .move_to(coordinator::State::DkgPublicDistribute)
            .expect("failed to move state machine");

        let outbound = self
            .wsts_coordinator
            .start_public_shares()
            .expect("failed to start public shares");

        self.send_packet(bitcoin_chain_tip, txid, outbound).await;

        match self.loop_until_result(bitcoin_chain_tip, txid).await {
            wsts::state_machine::OperationResult::Dkg(aggregate_key) => aggregate_key,
            _ => panic!("unexpected operation result"),
        }
    }

    /// Request a transaction to be signed
    pub async fn request_sign_transaction(
        &mut self,
        bitcoin_chain_tip: bitcoin::BlockHash,
        tx: bitcoin::Transaction,
        aggregate_key: p256k1::point::Point,
    ) {
        let payload: message::Payload =
            message::BitcoinTransactionSignRequest { tx, aggregate_key }.into();

        let msg = payload
            .to_message(bitcoin_chain_tip)
            .sign_ecdsa(&self.private_key)
            .expect("failed to sign message");

        self.network
            .broadcast(msg)
            .await
            .expect("failed to broadcast dkg begin msg");

        let mut responses = 0;

        loop {
            let msg = self.network.receive().await.expect("network error");

            let message::Payload::BitcoinTransactionSignAck(_) = msg.inner.payload else {
                continue;
            };

            responses += 1;

            if responses >= self.num_signers {
                break;
            }
        }
    }

    /// Run a signing round
    pub async fn run_signing_round(
        &mut self,
        bitcoin_chain_tip: bitcoin::BlockHash,
        txid: bitcoin::Txid,
        msg: &[u8],
    ) -> wsts::taproot::SchnorrProof {
        let outbound = self
            .wsts_coordinator
            .start_signing_round(msg, true, None)
            .expect("failed to start signing round");

        self.send_packet(bitcoin_chain_tip, txid, outbound).await;

        match self.loop_until_result(bitcoin_chain_tip, txid).await {
            wsts::state_machine::OperationResult::SignTaproot(signature) => signature,
            _ => panic!("unexpected operation result"),
        }
    }

    async fn loop_until_result(
        &mut self,
        bitcoin_chain_tip: bitcoin::BlockHash,
        txid: bitcoin::Txid,
    ) -> wsts::state_machine::OperationResult {
        loop {
            let msg = self.network.receive().await.expect("network error");

            let message::Payload::WstsMessage(wsts_msg) = msg.inner.payload else {
                continue;
            };

            let packet = wsts::net::Packet {
                msg: wsts_msg.inner,
                sig: Vec::new(),
            };

            let (outbound_packet, operation_result) = self
                .wsts_coordinator
                .process_message(&packet)
                .expect("message processing failed");

            if let Some(packet) = outbound_packet {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                self.send_packet(bitcoin_chain_tip, txid, packet).await;
            }

            if let Some(result) = operation_result {
                return result;
            }
        }
    }
}

/// Test signer that can operate over an `in_memory` network
pub struct Signer {
    network: network::in_memory::MpmcBroadcaster,
    wsts_signer: wsts_state_machine::SignerStateMachine,
    private_key: p256k1::scalar::Scalar,
}

impl Signer {
    /// Construct a new signer
    pub fn new(
        network: network::in_memory::MpmcBroadcaster,
        signer_info: SignerInfo,
        threshold: u32,
    ) -> Self {
        let wsts_signer = wsts_state_machine::SignerStateMachine::new(
            signer_info.signer_public_keys,
            threshold,
            signer_info.signer_private_key,
        )
        .expect("failed to construct state machine");

        Self {
            network,
            wsts_signer,
            private_key: signer_info.signer_private_key,
        }
    }

    /// Participate in a DKG round and return the result
    pub async fn run_until_dkg_end(mut self) -> Self {
        loop {
            let msg = self.network.receive().await.expect("network error");
            let bitcoin_chain_tip = msg.bitcoin_chain_tip;

            let message::Payload::WstsMessage(wsts_msg) = msg.inner.payload else {
                continue;
            };

            let packet = wsts::net::Packet {
                msg: wsts_msg.inner,
                sig: Vec::new(),
            };

            let outbound_packets = self
                .wsts_signer
                .process_inbound_messages(&[packet])
                .expect("message processing failed");

            for packet in outbound_packets {
                self.wsts_signer
                    .process_inbound_messages(&[packet.clone()])
                    .expect("message processing failed");

                self.send_packet(bitcoin_chain_tip, wsts_msg.txid, packet.clone())
                    .await;

                if let wsts::net::Message::DkgEnd(_) = packet.msg {
                    return self;
                }
            }
        }
    }

    fn pub_key(&self) -> p256k1::ecdsa::PublicKey {
        p256k1::ecdsa::PublicKey::new(&self.private_key).expect("failed to generate pub key")
    }
}

trait WstsEntity {
    fn network(&mut self) -> &mut network::in_memory::MpmcBroadcaster;
    fn private_key(&self) -> &p256k1::scalar::Scalar;

    async fn send_packet(
        &mut self,
        bitcoin_chain_tip: bitcoin::BlockHash,
        txid: bitcoin::Txid,
        packet: wsts::net::Packet,
    ) {
        let payload: message::Payload = message::WstsMessage { txid, inner: packet.msg }.into();

        let msg = payload
            .to_message(bitcoin_chain_tip)
            .sign_ecdsa(self.private_key())
            .expect("failed to sign message");

        self.network()
            .broadcast(msg)
            .await
            .expect("failed to broadcast dkg begin msg");
    }
}

impl WstsEntity for Coordinator {
    fn network(&mut self) -> &mut network::in_memory::MpmcBroadcaster {
        &mut self.network
    }

    fn private_key(&self) -> &p256k1::scalar::Scalar {
        &self.private_key
    }
}

impl WstsEntity for Signer {
    fn network(&mut self) -> &mut network::in_memory::MpmcBroadcaster {
        &mut self.network
    }

    fn private_key(&self) -> &p256k1::scalar::Scalar {
        &self.private_key
    }
}

/// A set of signers and a coordinator
pub struct SignerSet {
    signers: Vec<Signer>,
    coordinator: Coordinator,
}

impl SignerSet {
    /// Construct a new signer set
    pub fn new<F>(signer_info: &[SignerInfo], threshold: u32, connect: F) -> Self
    where
        F: Fn() -> network::in_memory::MpmcBroadcaster,
    {
        let coordinator_info = signer_info.first().unwrap().clone();
        let coordinator = Coordinator::new(connect(), coordinator_info, threshold);
        let signers = signer_info
            .iter()
            .cloned()
            .map(|signer_info| Signer::new(connect(), signer_info, threshold))
            .collect();

        Self { signers, coordinator }
    }

    /// Run DKG and return the private and public shares
    /// for all signers
    pub async fn run_dkg<Rng: rand::RngCore + rand::CryptoRng>(
        &mut self,
        bitcoin_chain_tip: bitcoin::BlockHash,
        txid: bitcoin::Txid,
        rng: &mut Rng,
    ) -> (p256k1::point::Point, Vec<model::EncryptedDkgShares>) {
        let mut signer_handles = Vec::new();
        for signer in self.signers.drain(..) {
            let handle = tokio::spawn(async { signer.run_until_dkg_end().await });
            signer_handles.push(handle);
        }

        let aggregate_key = self.coordinator.run_dkg(bitcoin_chain_tip, txid).await;

        for handle in signer_handles {
            let signer = handle.await.expect("signer crashed");
            self.signers.push(signer)
        }

        (
            aggregate_key,
            self.signers
                .iter()
                .map(|signer| {
                    signer
                        .wsts_signer
                        .get_encrypted_dkg_shares(rng)
                        .expect("failed to get encrypted shares")
                })
                .collect(),
        )
    }

    /// Dump the current signer set as a dummy rotate-keys transaction to the given storage
    pub async fn write_as_rotate_keys_tx<S, Rng>(
        &self,
        storage: &mut S,
        chain_tip: &model::BitcoinBlockHash,
        aggregate_key: p256k1::point::Point,
        rng: &mut Rng,
    ) where
        S: storage::DbWrite + storage::DbRead,
        Rng: rand::RngCore + rand::CryptoRng,
    {
        let aggregate_key = aggregate_key.x().to_bytes().to_vec();
        let signer_set = self
            .signers
            .iter()
            .map(|signer| signer.pub_key().to_bytes().to_vec())
            .collect();

        let stacks_chain_tip = storage
            .get_stacks_chain_tip(chain_tip)
            .await
            .expect("storage error")
            .expect("no stacks chain tip");

        let txid: model::StacksTxId = (0..32).map(|_| rng.next_u32() as u8).collect();
        let stacks_transaction = model::StacksTransaction {
            txid: txid.clone(),
            block_hash: stacks_chain_tip.block_hash.clone(),
        };

        let transaction = model::Transaction {
            txid: txid.clone(),
            tx: Vec::new(),
            tx_type: model::TransactionType::RotateKeys,
            block_hash: stacks_chain_tip.block_hash,
        };

        let rotate_keys_tx = model::RotateKeysTransaction {
            aggregate_key,
            txid,
            signer_set,
        };

        storage
            .write_transaction(&transaction)
            .await
            .expect("failed to write transaction");

        storage
            .write_stacks_transaction(&stacks_transaction)
            .await
            .expect("failed to write stacks transaction");

        storage
            .write_rotate_keys_transaction(&rotate_keys_tx)
            .await
            .expect("failed to write key rotation");
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use crate::testing::dummy;

    use super::*;

    #[tokio::test]
    async fn should_be_able_to_run_dkg() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(46);
        let network = network::in_memory::Network::new();
        let num_signers = 7;
        let threshold = 5;

        let bitcoin_chain_tip = dummy::block_hash(&fake::Faker, &mut rng);
        let txid = dummy::txid(&fake::Faker, &mut rng);

        let signer_info = generate_signer_info(&mut rng, num_signers);
        let mut signer_set = SignerSet::new(&signer_info, threshold, || network.connect());

        let (_, dkg_shares) = signer_set.run_dkg(bitcoin_chain_tip, txid, &mut rng).await;

        assert_eq!(dkg_shares.len(), num_signers);
    }
}