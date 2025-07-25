use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoincore_rpc::RpcApi;
use fake::Fake as _;
use fake::Faker;
use lru::LruCache;
use rand::SeedableRng as _;
use rand::rngs::OsRng;
use signer::bitcoin::MockBitcoinInteract;
use signer::emily_client::MockEmilyInteract;
use signer::message::Payload;
use signer::network::in_memory2::SignerNetworkInstance;
use signer::stacks::api::MockStacksInteract;
use signer::stacks::api::SignerSetInfo;
use signer::stacks::wallet::MultisigTx;
use signer::stacks::wallet::SignerWallet;
use signer::storage::DbRead;
use signer::storage::DbWrite;
use signer::storage::postgres::PgStore;
use signer::testing::IterTestExt;
use signer::testing::btc::get_canonical_chain_tip;
use signer::transaction_signer::STACKS_SIGN_REQUEST_LRU_SIZE;
use test_case::test_case;

use signer::bitcoin::utxo::RequestRef;
use signer::bitcoin::utxo::Requests;
use signer::bitcoin::utxo::UnsignedTransaction;
use signer::bitcoin::validation::TxRequestIds;
use signer::context::Context;
use signer::context::SbtcLimits;
use signer::error::Error;
use signer::keys::PrivateKey;
use signer::keys::PublicKey;
use signer::message::BitcoinPreSignRequest;
use signer::message::StacksTransactionSignRequest;
use signer::message::WstsMessage;
use signer::message::WstsMessageId;
use signer::network::InMemoryNetwork;
use signer::network::MessageTransfer;
use signer::network::in_memory2::WanNetwork;
use signer::stacks::contracts::ContractCall;
use signer::storage::model;
use signer::storage::model::BitcoinBlockHash;
use signer::storage::model::BitcoinBlockRef;
use signer::storage::model::BitcoinTxId;
use signer::storage::model::BitcoinTxSigHash;
use signer::storage::model::DkgSharesStatus;
use signer::storage::model::SigHash;
use signer::storage::model::StacksTxId;
use signer::testing;
use signer::testing::context::*;
use signer::testing::get_rng;
use signer::transaction_signer::ChainTipStatus;
use signer::transaction_signer::MsgChainTipReport;
use signer::transaction_signer::TxSignerEventLoop;
use signer::wsts_state_machine::StateMachineId;
use wsts::net::DkgBegin;
use wsts::net::Message as WstsNetMessage;
use wsts::net::NonceRequest;

use crate::setup::SweepAmounts;
use crate::setup::TestSignerSet;
use crate::setup::TestSweepSetup;
use crate::setup::TestSweepSetup2;
use crate::setup::backfill_bitcoin_blocks;
use crate::setup::fetch_canonical_bitcoin_blockchain;
use crate::setup::fill_signers_utxo;
use crate::setup::set_deposit_incomplete;
use crate::setup::set_verification_status;

type MockedTxSigner = TxSignerEventLoop<
    TestContext<
        PgStore,
        WrappedMock<MockBitcoinInteract>,
        WrappedMock<MockStacksInteract>,
        WrappedMock<MockEmilyInteract>,
    >,
    SignerNetworkInstance,
    OsRng,
>;

/// Test that [`TxSignerEventLoop::assert_valid_stacks_tx_sign_request`]
/// errors when the signer is not in the signer set.
#[tokio::test]
async fn signing_set_validation_check_for_stacks_transactions() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();

    let mut ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_first_bitcoin_core_client()
        .with_mocked_emily_client()
        .with_mocked_stacks_client()
        .build();
    let (rpc, faucet) = sbtc::testing::regtest::initialize_blockchain();

    // This confirms a deposit transaction, and has a nice helper function
    // for storing a real deposit.
    let mut setup = TestSweepSetup::new_setup(rpc, faucet, 10000, &mut rng);

    // Let's get the blockchain data into the database.
    let chain_tip = BitcoinBlockRef {
        block_hash: setup.sweep_block_hash.into(),
        block_height: setup.sweep_block_height,
    };
    backfill_bitcoin_blocks(&db, rpc, &chain_tip.block_hash).await;

    // This is all normal things that need to happen in order to pass
    // validation.
    setup.store_happy_path_data(&db).await;

    let (mut req, _) = crate::complete_deposit::make_complete_deposit(&setup);

    req.deployer = ctx.config().signer.deployer;
    let network = InMemoryNetwork::new();
    let mut tx_signer = TxSignerEventLoop {
        network: network.connect(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        signer_private_key: setup.aggregated_signer.keypair.secret_key().into(),
        threshold: 2,
        last_presign_block: None,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    // Let's create a proper sign request.
    let request = StacksTransactionSignRequest {
        aggregate_key: None,
        contract_tx: ContractCall::CompleteDepositV1(Box::new(req)).into(),
        // The nonce and tx_fee aren't really validated against anything at
        // the moment.
        nonce: 1,
        tx_fee: 100_000,
        txid: Faker.fake_with_rng::<StacksTxId, _>(&mut rng).into(),
    };

    // We need this or the contract call will fail validation with an
    // unrelated error, since we mock reaching out to the stacks node.
    set_deposit_incomplete(&mut ctx).await;

    // We can sign a transaction generated by a coordinator who is not in
    // the signer set, so the origin doesn't matter much for this function
    // call.
    let origin_public_key: PublicKey = Faker.fake_with_rng(&mut rng);
    // This is all happy path, there shouldn't be any errors here
    tx_signer
        .assert_valid_stacks_tx_sign_request(&request, &chain_tip, &origin_public_key)
        .await
        .unwrap();

    // Now we make sure that the current signer is not in the current
    // signing set.
    tx_signer.signer_private_key = PrivateKey::new(&mut rng);

    // Okay now that we have changed the fact that we are not in the
    // signing set, we should get an error now.
    let validation = tx_signer
        .assert_valid_stacks_tx_sign_request(&request, &chain_tip, &origin_public_key)
        .await
        .unwrap_err();
    assert!(matches!(validation, Error::ValidationSignerSet(_)));

    testing::storage::drop_db(db).await;
}

/// Test that [`TxSignerEventLoop::assert_valid_stacks_tx_sign_request`]
/// ignores the `aggregate_key` field in the
/// [`StacksTransactionSignRequest`].
#[tokio::test]
async fn signing_set_validation_ignores_aggregate_key_in_request() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();

    let mut ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_first_bitcoin_core_client()
        .with_mocked_emily_client()
        .with_mocked_stacks_client()
        .build();
    let (rpc, faucet) = sbtc::testing::regtest::initialize_blockchain();

    // This confirms a deposit transaction, and has a nice helper function
    // for storing a real deposit.
    let mut setup = TestSweepSetup::new_setup(rpc, faucet, 10000, &mut rng);

    // Let's get the blockchain data into the database.
    let chain_tip = BitcoinBlockRef {
        block_hash: setup.sweep_block_hash.into(),
        block_height: setup.sweep_block_height,
    };
    backfill_bitcoin_blocks(&db, rpc, &chain_tip.block_hash).await;

    // This is all normal things that need to happen in order to pass
    // validation.
    setup.store_happy_path_data(&db).await;

    let (mut req, _) = crate::complete_deposit::make_complete_deposit(&setup);

    req.deployer = ctx.config().signer.deployer;
    let network = InMemoryNetwork::new();
    let mut tx_signer = TxSignerEventLoop {
        network: network.connect(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        signer_private_key: setup.aggregated_signer.keypair.secret_key().into(),
        threshold: 2,
        last_presign_block: None,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    // Let's create a proper sign request.
    let mut request = StacksTransactionSignRequest {
        aggregate_key: None,
        contract_tx: ContractCall::CompleteDepositV1(Box::new(req)).into(),
        // The nonce and tx_fee aren't really validated against anything at
        // the moment.
        nonce: 1,
        tx_fee: 100_000,
        txid: Faker.fake_with_rng::<StacksTxId, _>(&mut rng).into(),
    };

    // We need this or the contract call will fail validation with an
    // unrelated error, since we mock reaching out to the stacks node.
    set_deposit_incomplete(&mut ctx).await;

    // We can sign a transaction generated by a coordinator who is not in
    // the signer set, so the origin doesn't matter much for this function
    // call.
    let origin_public_key: PublicKey = Faker.fake_with_rng(&mut rng);
    // This is all happy path, there shouldn't be any errors here
    tx_signer
        .assert_valid_stacks_tx_sign_request(&request, &chain_tip, &origin_public_key)
        .await
        .unwrap();

    // Now we make sure that the aggregate key is not one that the
    // signer knows about.
    request.aggregate_key = Some(PublicKey::from_private_key(&PrivateKey::new(&mut rng)));

    // Okay now that we have changed the request to include a bogus
    // aggregate key, but this should still validate because we should
    // ignore the aggregate key field.
    tx_signer
        .assert_valid_stacks_tx_sign_request(&request, &chain_tip, &origin_public_key)
        .await
        .unwrap();

    testing::storage::drop_db(db).await;
}

#[test_case(1, false ; "fee-too-high")]
#[test_case(0, true ; "fee-okay")]
#[tokio::test]
async fn signer_rejects_stacks_txns_with_too_high_a_fee(
    fee_relative_to_configured_limit: u64,
    should_accept: bool,
) {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();

    let mut ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_first_bitcoin_core_client()
        .with_mocked_emily_client()
        .with_mocked_stacks_client()
        .build();

    // We need this or the contract call will fail validation with an
    // unrelated error, since we mock reaching out to the stacks node.
    set_deposit_incomplete(&mut ctx).await;

    let (rpc, faucet) = sbtc::testing::regtest::initialize_blockchain();

    // This confirms a deposit transaction, and has a nice helper function
    // for storing a real deposit.
    let mut setup = TestSweepSetup::new_setup(rpc, faucet, 10000, &mut rng);

    // Let's get the blockchain data into the database.
    let chain_tip = BitcoinBlockRef {
        block_hash: setup.sweep_block_hash.into(),
        block_height: setup.sweep_block_height,
    };
    backfill_bitcoin_blocks(&db, rpc, &chain_tip.block_hash).await;

    // This is all normal things that need to happen in order to pass
    // validation.
    setup.store_happy_path_data(&db).await;

    let (mut req, _) = crate::complete_deposit::make_complete_deposit(&setup);

    req.deployer = ctx.config().signer.deployer;
    let network = InMemoryNetwork::new();
    let mut tx_signer = TxSignerEventLoop {
        network: network.connect(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        signer_private_key: setup.aggregated_signer.keypair.secret_key().into(),
        threshold: 2,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        last_presign_block: None,
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    // Setup the transaction fee to be the maximum fee configured plus one, so that it
    // exceeds the configured value.
    let stacks_fees_max_ustx = ctx.config().signer.stacks_fees_max_ustx;
    let tx_fee = stacks_fees_max_ustx.get() + fee_relative_to_configured_limit;

    // Let's create a proper sign request.
    let request = StacksTransactionSignRequest {
        aggregate_key: None,
        contract_tx: ContractCall::CompleteDepositV1(Box::new(req)).into(),
        // The nonce isn't really validated against anything at the moment.
        nonce: 1,
        tx_fee,
        txid: Faker.fake_with_rng::<StacksTxId, _>(&mut rng).into(),
    };

    // We can sign a transaction generated by a coordinator who is not in
    // the signer set, so the origin doesn't matter much for this function
    // call.
    let origin_public_key: PublicKey = Faker.fake_with_rng(&mut rng);
    let result = tx_signer
        .assert_valid_stacks_tx_sign_request(&request, &chain_tip, &origin_public_key)
        .await;

    if should_accept {
        assert!(matches!(result, Ok(())));
    } else {
        // We cannot enable partial eq for the error type because it contains many
        // internal types that don't implement it, so we have to match on the error
        // and ensure that it is the correct one.
        assert!(matches!(result, Err(Error::StacksFeeLimitExceeded(_, _))));
    }

    testing::storage::drop_db(db).await;
}

#[tokio::test]
async fn signer_rejects_multiple_attempts_in_tenure() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();

    let mut ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_first_bitcoin_core_client()
        .with_mocked_emily_client()
        .with_mocked_stacks_client()
        .build();

    // We need this or the contract call will fail validation with an
    // unrelated error, since we mock reaching out to the stacks node.
    set_deposit_incomplete(&mut ctx).await;

    let (rpc, faucet) = sbtc::testing::regtest::initialize_blockchain();

    // This confirms a deposit transaction, and has a nice helper function
    // for storing a real deposit.
    let mut setup = TestSweepSetup::new_setup(rpc, faucet, 10000, &mut rng);

    // Let's get the blockchain data into the database.
    let chain_tip = BitcoinBlockRef {
        block_hash: setup.sweep_block_hash.into(),
        block_height: setup.sweep_block_height,
    };
    backfill_bitcoin_blocks(&db, rpc, &chain_tip.block_hash).await;

    // This is all normal things that need to happen in order to pass
    // validation.
    setup.store_happy_path_data(&db).await;

    let (mut req, _) = crate::complete_deposit::make_complete_deposit(&setup);

    req.deployer = ctx.config().signer.deployer;
    let network = InMemoryNetwork::new();
    let mut tx_signer = TxSignerEventLoop {
        network: network.connect(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        signer_private_key: setup.aggregated_signer.keypair.secret_key().into(),
        threshold: 2,
        last_presign_block: None,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    // We need this so that there is a live "network". Otherwise will error when
    // trying to send a message at the end.
    let _rec = ctx.get_signal_receiver();

    // We need the wallet to get the real txid to pass the final checks
    let wallet = SignerWallet::load(&ctx).await.unwrap();

    // We can sign a transaction generated by a coordinator who is not in
    // the signer set, so the origin doesn't matter much for this function
    // call.
    let origin_public_key: PublicKey = Faker.fake_with_rng(&mut rng);

    // Let's create a proper sign request.
    let mut request = StacksTransactionSignRequest {
        aggregate_key: None,
        contract_tx: ContractCall::CompleteDepositV1(Box::new(req.clone())).into(),
        nonce: 1,
        tx_fee: 100_000,
        txid: Faker.fake_with_rng::<StacksTxId, _>(&mut rng).into(),
    };
    wallet.set_nonce(request.nonce);
    request.txid = MultisigTx::new_tx(&request.contract_tx, &wallet, request.tx_fee)
        .tx()
        .txid();

    // Try to sign the sign request for the first time in this tenure
    let result = tx_signer
        .handle_stacks_transaction_sign_request(&request, &chain_tip, &origin_public_key)
        .await;
    assert!(result.is_ok());

    // Now create a different sign request for the same deposit
    let mut new_request = StacksTransactionSignRequest {
        aggregate_key: None,
        contract_tx: ContractCall::CompleteDepositV1(Box::new(req)).into(),
        nonce: 2,
        tx_fee: 123_000,
        txid: Faker.fake_with_rng::<StacksTxId, _>(&mut rng).into(),
    };
    wallet.set_nonce(new_request.nonce);
    new_request.txid = MultisigTx::new_tx(&new_request.contract_tx, &wallet, new_request.tx_fee)
        .tx()
        .txid();

    assert_ne!(new_request.tx_fee, request.tx_fee);
    assert_ne!(new_request.nonce, request.nonce);
    assert_ne!(new_request.txid, request.txid);

    // And try to sign it in the same tenure as the previous attempt
    let result = tx_signer
        .handle_stacks_transaction_sign_request(&new_request, &chain_tip, &origin_public_key)
        .await;
    assert!(matches!(
        result,
        Err(Error::StacksRequestAlreadySigned(_, _))
    ));

    // Now generate a new block and backfill
    faucet.generate_block();
    fetch_canonical_bitcoin_blockchain(&db, rpc).await;
    let new_chain_tip = db
        .get_bitcoin_canonical_chain_tip_ref()
        .await
        .unwrap()
        .unwrap();
    assert_ne!(new_chain_tip, chain_tip);

    // Then try to sign it again in a new tenure
    let result = tx_signer
        .handle_stacks_transaction_sign_request(&new_request, &new_chain_tip, &origin_public_key)
        .await;
    assert!(result.is_ok());

    testing::storage::drop_db(db).await;
}

#[tokio::test]
async fn assert_should_be_able_to_handle_sbtc_requests() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();
    let fee_rate = 1.3;
    // Build the test context with mocked clients
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .build();
    ctx.state().update_current_limits(SbtcLimits::unlimited());

    let (rpc, faucet) = sbtc::testing::regtest::initialize_blockchain();

    // Create a test setup with a confirmed deposit transaction
    let setup = TestSweepSetup::new_setup(rpc, faucet, 10000, &mut rng);
    // Backfill the blockchain data into the database
    let chain_tip = BitcoinBlockRef {
        block_hash: setup.sweep_block_hash.into(),
        block_height: setup.sweep_block_height,
    };
    backfill_bitcoin_blocks(&db, rpc, &chain_tip.block_hash).await;
    let bitcoin_block = db.get_bitcoin_block(&chain_tip.block_hash).await.unwrap();

    let public_aggregate_key = setup.aggregated_signer.keypair.public_key().into();

    // // Fill the signer's UTXO in the database
    fill_signers_utxo(&db, bitcoin_block.unwrap(), &public_aggregate_key, &mut rng).await;

    // Store the necessary data for passing validation
    setup.store_stacks_genesis_block(&db).await;
    setup.store_deposit_tx(&db).await;
    setup.store_dkg_shares(&db).await;
    setup.store_rotate_keys_event(&db).await;
    setup.store_deposit_request(&db).await;
    setup.store_deposit_decisions(&db).await;

    let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
    let signer_set_info = SignerSetInfo::from(shares);

    let state = ctx.state();
    state.update_current_signer_set(signer_set_info.signer_set.clone());
    state.update_registry_signer_set_info(signer_set_info);

    // Initialize the transaction signer event loop
    let network = WanNetwork::default();

    let net = network.connect(&ctx);
    let mut tx_signer = TxSignerEventLoop {
        network: net.spawn(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        signer_private_key: setup.aggregated_signer.keypair.secret_key().into(),
        threshold: 2,
        last_presign_block: None,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    let sbtc_requests: TxRequestIds = TxRequestIds {
        deposits: vec![setup.deposit_request.outpoint],
        withdrawals: vec![],
    };

    let sbtc_context = BitcoinPreSignRequest {
        request_package: vec![sbtc_requests],
        fee_rate,
        last_fees: None,
    };

    let sbtc_state = signer::bitcoin::utxo::SignerBtcState {
        utxo: ctx
            .get_storage()
            .get_signer_utxo(&chain_tip.block_hash)
            .await
            .unwrap()
            .unwrap(),
        fee_rate,
        last_fees: None,
        public_key: setup.aggregated_signer.keypair.public_key().into(),
        magic_bytes: [b'T', b'3'],
    };

    // Create an unsigned transaction with the deposit request
    // to obtain the sighashes and corresponding txid that should
    // be stored in the database
    let unsigned_tx = UnsignedTransaction::new(
        Requests::new(vec![RequestRef::Deposit(&setup.deposit_request)]),
        &sbtc_state,
    )
    .unwrap();

    let digests = unsigned_tx.construct_digests().unwrap();
    let signer_digest = digests.signer_sighash();
    let deposit_digest = digests.deposit_sighashes();
    assert_eq!(deposit_digest.len(), 1);
    let deposit_digest = deposit_digest[0];

    let mut handle = network.connect(&ctx).spawn();

    tx_signer
        .handle_bitcoin_pre_sign_request(&sbtc_context, &chain_tip)
        .await
        .unwrap();

    // Check if we are receiving an Ack from the signer
    tokio::time::timeout(Duration::from_secs(2), async move {
        handle.receive().await.unwrap();
    })
    .await
    .unwrap();

    // Check that the intentions to sign the requests sighashes
    // are stored in the database
    let (will_sign, _) = db
        .will_sign_bitcoin_tx_sighash(&signer_digest.sighash.into())
        .await
        .expect("query to check if signer sighash is stored failed")
        .expect("signer sighash not stored");

    assert!(will_sign);
    let (will_sign, _) = db
        .will_sign_bitcoin_tx_sighash(&deposit_digest.sighash.into())
        .await
        .expect("query to check if deposit sighash is stored failed")
        .expect("deposit sighash not stored");

    assert!(will_sign);

    testing::storage::drop_db(db).await;
}

#[test_case(DkgSharesStatus::Verified, true ; "verified-shares-okay")]
#[test_case(DkgSharesStatus::Unverified, false ; "unverified-shares-not-okay")]
#[test_case(DkgSharesStatus::Failed, false ; "failed-shares-not-okay")]
#[tokio::test]
async fn presign_requests_with_dkg_shares_status(status: DkgSharesStatus, is_ok: bool) {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();
    // Build the test context with mocked clients
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .build();

    let (rpc, faucet) = sbtc::testing::regtest::initialize_blockchain();

    let signers = TestSignerSet::new(&mut rng);
    // Create a test setup object so that we can easily create proper DKG
    // shares in the database. Note that calling TestSweepSetup2::new_setup
    // creates two bitcoin blocks.
    let amounts = SweepAmounts {
        amount: 100000,
        max_fee: 10000,
        is_deposit: true,
    };
    let setup = TestSweepSetup2::new_setup(signers, faucet, &[amounts]);

    let block_header = rpc
        .get_block_header_info(&setup.deposit_block_hash)
        .unwrap();
    let chain_tip = BitcoinBlockRef {
        block_hash: block_header.hash.into(),
        block_height: block_header.height.into(),
    };

    // Store the necessary data for passing validation
    let aggregate_key = setup.signers.aggregate_key();

    backfill_bitcoin_blocks(&db, rpc, &setup.deposit_block_hash).await;

    setup.store_stacks_genesis_block(&db).await;
    setup.store_dkg_shares(&db).await;
    setup.store_rotate_keys_event(&db).await;
    setup.store_donation(&db).await;
    setup.store_deposit_txs(&db).await;
    setup.store_deposit_request(&db).await;
    setup.store_deposit_decisions(&db).await;

    set_verification_status(&db, aggregate_key, status).await;

    let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
    let signer_set_info = SignerSetInfo::from(shares);

    ctx.state().update_registry_signer_set_info(signer_set_info);
    ctx.state().update_current_limits(SbtcLimits::unlimited());

    // Initialize the transaction signer event loop
    let network = WanNetwork::default();

    let net = network.connect(&ctx);
    let mut tx_signer = TxSignerEventLoop {
        network: net.spawn(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        // We use this private key because it needs to be associated with
        // one of the public keys that we stored in the DKG shares table.
        signer_private_key: setup.signers.private_key(),
        threshold: 2,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        last_presign_block: None,
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    let sbtc_requests: TxRequestIds = TxRequestIds {
        deposits: setup.deposit_outpoints(),
        withdrawals: vec![],
    };

    let sbtc_context = BitcoinPreSignRequest {
        request_package: vec![sbtc_requests],
        fee_rate: 2.0,
        last_fees: None,
    };

    let result = tx_signer
        .handle_bitcoin_pre_sign_request(&sbtc_context, &chain_tip)
        .await;

    match result {
        Ok(()) => assert!(is_ok),
        Err(Error::NoVerifiedDkgShares) => assert!(!is_ok),
        Err(error) => panic!("{error}, got an unexpected result"),
    }

    testing::storage::drop_db(db).await;
}

#[tokio::test]
pub async fn presign_request_ignore_request_if_already_processed_this_block() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();
    // Build the test context with mocked clients
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .build();

    let (rpc, faucet) = sbtc::testing::regtest::initialize_blockchain();

    let signers = TestSignerSet::new(&mut rng);
    // Create a test setup object so that we can easily create proper DKG
    // shares in the database. Note that calling TestSweepSetup2::new_setup
    // creates two bitcoin blocks.
    let amounts = SweepAmounts {
        amount: 100000,
        max_fee: 10000,
        is_deposit: true,
    };
    let setup = TestSweepSetup2::new_setup(signers, faucet, &[amounts]);

    let block_header = rpc
        .get_block_header_info(&setup.deposit_block_hash)
        .unwrap();
    let chain_tip = BitcoinBlockRef {
        block_hash: block_header.hash.into(),
        block_height: block_header.height.into(),
    };

    // Store the necessary data for passing validation
    let aggregate_key = setup.signers.aggregate_key();

    backfill_bitcoin_blocks(&db, rpc, &setup.deposit_block_hash).await;

    setup.store_stacks_genesis_block(&db).await;
    setup.store_dkg_shares(&db).await;
    setup.store_rotate_keys_event(&db).await;
    setup.store_donation(&db).await;
    setup.store_deposit_txs(&db).await;
    setup.store_deposit_request(&db).await;
    setup.store_deposit_decisions(&db).await;

    set_verification_status(&db, aggregate_key, DkgSharesStatus::Verified).await;

    let shares = db.get_latest_encrypted_dkg_shares().await.unwrap().unwrap();
    let signer_set_info = SignerSetInfo::from(shares);

    ctx.state().update_registry_signer_set_info(signer_set_info);
    ctx.state().update_current_limits(SbtcLimits::unlimited());

    // Initialize the transaction signer event loop
    let network = WanNetwork::default();

    let net = network.connect(&ctx);
    let mut tx_signer = TxSignerEventLoop {
        network: net.spawn(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        // We use this private key because it needs to be associated with
        // one of the public keys that we stored in the DKG shares table.
        signer_private_key: setup.signers.private_key(),
        threshold: 2,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        last_presign_block: None,
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    let sbtc_requests: TxRequestIds = TxRequestIds {
        deposits: setup.deposit_outpoints(),
        withdrawals: vec![],
    };

    let sbtc_context = BitcoinPreSignRequest {
        request_package: vec![sbtc_requests],
        fee_rate: 2.0,
        last_fees: None,
    };

    let result = tx_signer
        .handle_bitcoin_pre_sign_request(&sbtc_context, &chain_tip)
        .await;

    // We are processing this block with verified shares and thus it should be ok.
    assert!(result.is_ok());

    // Check that we store information that we processed this block
    assert_eq!(tx_signer.last_presign_block, Some(chain_tip.block_hash));

    // Now we will try to process the same block again, but since we already
    // processed it, we should get an error.
    let result = tx_signer
        .handle_bitcoin_pre_sign_request(&sbtc_context, &chain_tip)
        .await;

    let err = result.unwrap_err();
    match err {
        Error::InvalidPresignRequest(hash) => {
            assert_eq!(hash, chain_tip.block_hash)
        }
        _ => panic!("Expected InvalidPresignRequest error, got: {err}"),
    }

    // Sanity check: if we clear information about already processed blocks this is ok again

    tx_signer.last_presign_block = None;
    let result = tx_signer
        .handle_bitcoin_pre_sign_request(&sbtc_context, &chain_tip)
        .await;

    assert!(result.is_ok());

    testing::storage::drop_db(db).await;
}

#[test_log::test(tokio::test)]
async fn new_state_machine_per_valid_sighash() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();
    // Build the test context with mocked clients
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .build();

    let (_, faucet) = sbtc::testing::regtest::initialize_blockchain();

    let signers = TestSignerSet::new(&mut rng);
    // Create a test setup object so that we can easily create proper DKG
    // shares in the database. Note that calling TestSweepSetup2::new_setup
    // creates two bitcoin blocks.
    let setup = TestSweepSetup2::new_setup(signers, faucet, &[]);

    setup.store_dkg_shares(&db).await;

    // Initialize the transaction signer event loop
    let network = WanNetwork::default();

    let net = network.connect(&ctx);
    let mut tx_signer = TxSignerEventLoop {
        network: net.spawn(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        // We use this private key because it needs to be associated with
        // one of the public keys that we stored in the DKG shares table.
        signer_private_key: setup.signers.private_key(),
        threshold: 2,
        last_presign_block: None,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    // We need to convince the signer event loop that it should accept the
    // message that we are going to send it.
    let report = MsgChainTipReport {
        sender_is_coordinator: true,
        chain_tip_status: ChainTipStatus::Canonical,
        chain_tip: BitcoinBlockRef {
            block_hash: BitcoinBlockHash::from([0; 32]),
            block_height: 0u64.into(),
        },
    };

    // The message that we will send is for the following sighash. We'll
    // need to make sure that it is in our database first
    let txid: BitcoinTxId = Faker.fake_with_rng(&mut rng);
    let sighash: SigHash = Faker.fake_with_rng(&mut rng);

    let row = BitcoinTxSigHash {
        txid,
        chain_tip: BitcoinBlockHash::from([0; 32]),
        prevout_txid: BitcoinTxId::from([0; 32]),
        prevout_output_index: 0,
        sighash,
        prevout_type: model::TxPrevoutType::Deposit,
        validation_result: signer::bitcoin::validation::InputValidationResult::Ok,
        is_valid_tx: true,
        will_sign: true,
        aggregate_key: PublicKey::from_private_key(&tx_signer.signer_private_key).into(),
    };

    db.write_bitcoin_txs_sighashes(&[row]).await.unwrap();

    // Now for the nonce request message
    let mut nonce_request_msg = WstsMessage {
        id: WstsMessageId::Sweep(*txid),
        inner: wsts::net::Message::NonceRequest(NonceRequest {
            dkg_id: 1,
            sign_id: 1,
            sign_iter_id: 1,
            message: sighash.to_byte_array().to_vec(),
            signature_type: wsts::net::SignatureType::Schnorr,
        }),
    };
    let msg_public_key = PublicKey::from_private_key(&PrivateKey::new(&mut rng));

    // Sanity check, the state machines cache should be empty.
    assert!(tx_signer.wsts_state_machines.is_empty());

    tx_signer
        .handle_wsts_message(&nonce_request_msg, msg_public_key, &report)
        .await
        .unwrap();

    // We should have a state machine associated with the sighash nonce
    // request message that we just received.
    let id1 = StateMachineId::BitcoinSign(sighash);
    assert!(tx_signer.wsts_state_machines.contains(&id1));
    assert_eq!(tx_signer.wsts_state_machines.len(), 1);

    // Now let's see what happens when we receive a nonce request message
    // for a sighash that we do not know about. Since the nonce request is
    // not in the database we should return an error, and the state machine
    // should not be in the local cache.
    let random_sighash: SigHash = Faker.fake_with_rng(&mut rng);
    match &mut nonce_request_msg.inner {
        wsts::net::Message::NonceRequest(NonceRequest { message, .. }) => {
            *message = random_sighash.as_byte_array().to_vec();
        }
        _ => panic!("You forgot to update the variant"),
    };

    let response = tx_signer
        .handle_wsts_message(&nonce_request_msg, msg_public_key, &report)
        .await;

    let id2 = StateMachineId::BitcoinSign(random_sighash);
    assert!(response.is_err());
    assert!(tx_signer.wsts_state_machines.contains(&id1));
    assert!(!tx_signer.wsts_state_machines.contains(&id2));
    assert_eq!(tx_signer.wsts_state_machines.len(), 1);

    testing::storage::drop_db(db).await;
}

/// Let's check that we always generate unique nonces for each sign
/// request.
#[test_log::test(tokio::test)]
async fn nonce_response_unique_nonces() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();
    // Build the test context with mocked clients
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .build();

    let (_, faucet) = sbtc::testing::regtest::initialize_blockchain();

    let signers = TestSignerSet::new(&mut rng);
    // Create a test setup object so that we can simply create proper DKG
    // shares in the database. Note that calling TestSweepSetup2::new_setup
    // creates two bitcoin blocks.
    let setup = TestSweepSetup2::new_setup(signers, faucet, &[]);

    setup.store_dkg_shares(&db).await;

    // Initialize the transaction signer event loop
    let network = WanNetwork::default();

    let net = network.connect(&ctx);
    let mut tx_signer = TxSignerEventLoop {
        network: net.spawn(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        // We use this private key because it needs to be associated with
        // one of the public keys that we stored in the DKG shares table.
        signer_private_key: setup.signers.private_key(),
        threshold: 2,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        dkg_begin_pause: None,
        last_presign_block: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    // We need to convince the signer event loop that it should accept the
    // message that we are going to send it.
    let report = MsgChainTipReport {
        sender_is_coordinator: true,
        chain_tip_status: ChainTipStatus::Canonical,
        chain_tip: BitcoinBlockRef {
            block_hash: BitcoinBlockHash::from([0; 32]),
            block_height: 0u64.into(),
        },
    };

    // The message that we will send is for the following sighash. We'll
    // need to make sure that it is in our database first
    let txid: BitcoinTxId = Faker.fake_with_rng(&mut rng);
    let sighash: SigHash = Faker.fake_with_rng(&mut rng);

    let row = BitcoinTxSigHash {
        txid,
        chain_tip: BitcoinBlockHash::from([0; 32]),
        prevout_txid: BitcoinTxId::from([0; 32]),
        prevout_output_index: 0,
        sighash,
        prevout_type: model::TxPrevoutType::Deposit,
        validation_result: signer::bitcoin::validation::InputValidationResult::Ok,
        is_valid_tx: true,
        will_sign: true,
        aggregate_key: PublicKey::from_private_key(&tx_signer.signer_private_key).into(),
    };

    db.write_bitcoin_txs_sighashes(&[row]).await.unwrap();

    // Now for the nonce request message
    let nonce_request_msg = WstsMessage {
        id: WstsMessageId::Sweep(*txid),
        inner: wsts::net::Message::NonceRequest(NonceRequest {
            dkg_id: 1,
            sign_id: 1,
            sign_iter_id: 1,
            message: sighash.to_byte_array().to_vec(),
            signature_type: wsts::net::SignatureType::Schnorr,
        }),
    };
    let msg_public_key = PublicKey::from_private_key(&PrivateKey::new(&mut rng));

    // Sanity check, the state machines cache should be empty.
    assert!(tx_signer.wsts_state_machines.is_empty());

    // This function listens for WSTS NonceResponse messages broadcast from
    // the TxSignerEventLoop and returns them.
    let func = |mut handle: SignerNetworkInstance| async move {
        let signed_message = handle.receive().await.unwrap();
        let Payload::WstsMessage(WstsMessage {
            inner: WstsNetMessage::NonceResponse(response),
            ..
        }) = signed_message.inner.payload
        else {
            panic!("incorrect payload, test creator error");
        };

        response
    };

    // Let's create a handle to listen to any messages generated during the
    // handle_wsts_message call.
    let handle = network.connect(&ctx).spawn();

    tx_signer
        .handle_wsts_message(&nonce_request_msg, msg_public_key, &report)
        .await
        .unwrap();

    // Let's listen for the nonce response.
    let response1 = tokio::time::timeout(Duration::from_secs(2), func(handle))
        .await
        .unwrap();

    // Okay, let's try this again using the same message. This checks the
    // case where we may be using a state machine stored in the
    // TxSignerEventLoop. Although we currently do not reuse an existing
    // state machine when we receive a nonce request, this is a check for
    // any future code.
    let handle = network.connect(&ctx).spawn();
    tx_signer
        .handle_wsts_message(&nonce_request_msg, msg_public_key, &report)
        .await
        .unwrap();

    // Okay this one could be using the same signer state machine as the
    // previous call; although, as mentioned above, it shouldn't.
    let response2 = tokio::time::timeout(Duration::from_secs(2), func(handle))
        .await
        .unwrap();

    // Let's clear all state machines so that we know that a new one is
    // being created.
    tx_signer.wsts_state_machines.clear();

    let handle = network.connect(&ctx).spawn();
    tx_signer
        .handle_wsts_message(&nonce_request_msg, msg_public_key, &report)
        .await
        .unwrap();

    // This one is for nonces generated by a fresh state machine.
    let response3 = tokio::time::timeout(Duration::from_secs(2), func(handle))
        .await
        .unwrap();

    // The signer has only one key ID for their DKG shares, so they should
    // only generate one nonce in their nonce response.
    let nonces1 = response1.nonces.single();
    let nonces2 = response2.nonces.single();
    let nonces3 = response3.nonces.single();
    // All of these nonces should be unique, so let's check. We compress
    // the public nonces so that we can easily hash them in a set.
    let nonces_list: [[u8; 33]; 6] = [
        nonces1.D.compress().data,
        nonces1.E.compress().data,
        nonces2.D.compress().data,
        nonces2.E.compress().data,
        nonces3.D.compress().data,
        nonces3.E.compress().data,
    ];
    let nonces_set = nonces_list.iter().copied().collect::<BTreeSet<[u8; 33]>>();

    // If we have duplicates then we should have fewer entries in the set
    // than in the list.
    assert_eq!(nonces_set.len(), nonces_list.len());

    testing::storage::drop_db(db).await;
}

#[tokio::test]
async fn max_one_state_machine_per_bitcoin_block_hash_for_dkg() {
    let db = testing::storage::new_test_database().await;

    let mut rng = get_rng();
    // Build the test context with mocked clients
    let ctx = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_clients()
        .build();

    // Let's make sure that the database has the chain tip.
    let (rpc, _) = sbtc::testing::regtest::initialize_blockchain();
    let headers = &get_canonical_chain_tip(rpc);
    let chain_tip = BitcoinBlockRef {
        block_hash: headers.hash.into(),
        block_height: headers.height.into(),
    };
    backfill_bitcoin_blocks(&db, rpc, &chain_tip.block_hash).await;

    let signer_set_public_keys = ctx.config().signer.bootstrap_signing_set.clone();

    ctx.state()
        .update_current_signer_set(signer_set_public_keys);

    // Initialize the transaction signer event loop
    let network = WanNetwork::default();
    let net = network.connect(&ctx);
    let mut tx_signer = TxSignerEventLoop {
        network: net.spawn(),
        context: ctx.clone(),
        context_window: 10000,
        wsts_state_machines: LruCache::new(NonZeroUsize::new(100).unwrap()),
        signer_private_key: ctx.config().signer.private_key,
        threshold: 2,
        last_presign_block: None,
        rng: rand::rngs::StdRng::seed_from_u64(51),
        dkg_begin_pause: None,
        dkg_verification_state_machines: LruCache::new(NonZeroUsize::new(5).unwrap()),
        stacks_sign_request: LruCache::new(STACKS_SIGN_REQUEST_LRU_SIZE),
    };

    // We need to convince the signer event loop that it should accept the
    // message that we are going to send it. DkgBegin messages are only
    // accepted from the coordinator on the canonical chain tip.
    let mut report = MsgChainTipReport {
        sender_is_coordinator: true,
        chain_tip_status: ChainTipStatus::Canonical,
        chain_tip,
    };

    // Now for the DKG begin message. We pick an arbitrary dkg_id, and an
    // arbitrary transaction ID.
    let dkg_id = 2;
    let dkg_begin_msg = WstsMessage {
        id: bitcoin::Txid::all_zeros().into(),
        inner: wsts::net::Message::DkgBegin(DkgBegin { dkg_id }),
    };
    let msg_public_key = PublicKey::from_private_key(&PrivateKey::new(&mut rng));

    // Sanity check, the state machines cache should be empty.
    assert!(tx_signer.wsts_state_machines.is_empty());

    tx_signer
        .handle_wsts_message(&dkg_begin_msg, msg_public_key, &report)
        .await
        .unwrap();

    // We should have a state machine associated with the current chain tip
    // request message that we just received.
    let id1 = StateMachineId::from(&chain_tip);
    let state_machine = tx_signer.wsts_state_machines.get(&id1).unwrap();
    assert_eq!(state_machine.dkg_id(), dkg_id);
    assert_eq!(tx_signer.wsts_state_machines.len(), 1);

    // Now let's see what happens when we receive another dkg message with
    // a different `dkg_id`. The expected behavior is that a new state
    // machine gets created, overwriting any existing one.
    let dkg_id = 1234;
    let dkg_begin_msg = WstsMessage {
        id: bitcoin::Txid::from_byte_array(Faker.fake_with_rng(&mut rng)).into(),
        inner: wsts::net::Message::DkgBegin(DkgBegin { dkg_id }),
    };

    tx_signer
        .handle_wsts_message(&dkg_begin_msg, msg_public_key, &report)
        .await
        .unwrap();

    let state_machine = tx_signer.wsts_state_machines.get(&id1).unwrap();
    assert_eq!(state_machine.dkg_id(), dkg_id);
    assert_eq!(tx_signer.wsts_state_machines.len(), 1);

    // If we say the current chain tip is something else, a new state
    // machine will be created associated with that chain tip
    report.chain_tip = Faker.fake_with_rng(&mut rng);

    tx_signer
        .handle_wsts_message(&dkg_begin_msg, msg_public_key, &report)
        .await
        .unwrap();

    let id2 = StateMachineId::from(&report.chain_tip);
    let state_machine = tx_signer.wsts_state_machines.get(&id2).unwrap();
    assert_eq!(state_machine.dkg_id(), dkg_id);
    assert_eq!(tx_signer.wsts_state_machines.len(), 2);

    testing::storage::drop_db(db).await;
}

/// Module containing tests for the
/// [`MockedTxSigner::validate_dkg_verification_message`] function. See
/// [`MockedTxSigner`] for information on the validations that these tests
/// are asserting.
mod validate_dkg_verification_message {
    use secp256k1::Keypair;

    use signer::{
        bitcoin::utxo::UnsignedMockTransaction, keys::PublicKeyXOnly,
        storage::model::EncryptedDkgShares,
    };

    use super::*;

    /// Helper struct for testing
    /// [`MockedTxSigner::validate_dkg_verification_message`].
    struct TestParams {
        pub new_aggregate_key: PublicKeyXOnly,
        pub dkg_verification_window: u16,
        pub bitcoin_chain_tip: BitcoinBlockRef,
        pub message: Option<Vec<u8>>,
    }

    impl Default for TestParams {
        fn default() -> Self {
            let new_aggregate_key = Keypair::new_global(&mut OsRng).x_only_public_key().into();
            Self {
                new_aggregate_key,
                dkg_verification_window: 0,
                bitcoin_chain_tip: BitcoinBlockRef {
                    block_hash: BitcoinBlockHash::from([0; 32]),
                    block_height: 0u64.into(),
                },
                message: None,
            }
        }
    }

    impl TestParams {
        fn new(new_aggregate_key: PublicKeyXOnly) -> Self {
            Self {
                new_aggregate_key,
                ..Self::default()
            }
        }
        /// Executes [`MockedTxSigner::validate_dkg_verification_message`] with
        /// the values in this [`TestParams`] instance.
        async fn execute(&self, db: &PgStore) -> Result<(), Error> {
            MockedTxSigner::validate_dkg_verification_message::<PgStore>(
                db,
                &self.new_aggregate_key,
                self.message.as_deref(),
                self.dkg_verification_window,
                &self.bitcoin_chain_tip,
            )
            .await
        }
    }

    #[tokio::test]
    async fn no_dkg_shares() {
        let db = testing::storage::new_test_database().await;

        // Just use default since we don't even have stored shares.
        let params = TestParams::default();

        let result = params.execute(&db).await.unwrap_err();
        assert!(matches!(result, Error::NoDkgShares));

        testing::storage::drop_db(db).await;
    }

    #[tokio::test]
    async fn latest_key_mismatch() {
        let mut rng = get_rng();
        let db = testing::storage::new_test_database().await;
        let latest_aggregate_key = Keypair::new_global(&mut rng).public_key().into();
        let new_aggregate_key = Keypair::new_global(&mut rng).x_only_public_key().into();

        // Create new DKG shares and store them in the database. We expect the
        // aggregate keys to not match, so we set them to values we explicitly
        // know won't match.
        let shares = EncryptedDkgShares {
            aggregate_key: latest_aggregate_key,
            ..Faker.fake()
        };
        db.write_encrypted_dkg_shares(&shares).await.unwrap();

        // Just to show that these two aren't equal.
        assert_ne!(new_aggregate_key, shares.aggregate_key.into());

        // New params with the new aggregate key which won't match.
        let params = TestParams::new(new_aggregate_key);

        let result = params.execute(&db).await.unwrap_err();

        if let Error::AggregateKeyMismatch { actual, expected } = result {
            let actual = *actual;
            let expected = *expected;

            assert_eq!(actual, latest_aggregate_key.into());
            assert_eq!(expected, new_aggregate_key);
            assert_ne!(actual, expected);
        } else {
            panic!("Expected an AggregateKeyMismatch error, got: {:?}", result);
        }

        testing::storage::drop_db(db).await;
    }

    #[tokio::test]
    async fn latest_key_in_failed_state() {
        let db = testing::storage::new_test_database().await;
        let aggregate_key: PublicKey = Keypair::new_global(&mut OsRng).public_key().into();
        let aggregate_key_x_only = aggregate_key.into();

        // Create new DKG shares and store them in the database. We expect the
        // aggregate keys to match but validation to fail due to the latest shares
        // being marked as `Failed`.
        let shares = EncryptedDkgShares {
            aggregate_key,
            dkg_shares_status: DkgSharesStatus::Failed,
            ..Faker.fake()
        };
        db.write_encrypted_dkg_shares(&shares).await.unwrap();

        // Setup the test parameters.
        let params = TestParams::new(aggregate_key_x_only);

        let result = params.execute(&db).await.unwrap_err();

        assert!(matches!(
            result,
            Error::DkgVerificationFailed(key) if aggregate_key_x_only == key
        ));

        testing::storage::drop_db(db).await;
    }

    #[tokio::test]
    async fn verification_window_elapsed() {
        let db = testing::storage::new_test_database().await;
        let aggregate_key: PublicKey = Keypair::new_global(&mut OsRng).public_key().into();

        // Create new DKG shares and store them in the database. We expect the
        // aggregate keys to match and the status to be allowed. We use 0 as the
        // starting block.
        let shares = EncryptedDkgShares {
            aggregate_key,
            dkg_shares_status: DkgSharesStatus::Unverified,
            started_at_bitcoin_block_height: 0u64.into(),
            ..Faker.fake()
        };
        db.write_encrypted_dkg_shares(&shares).await.unwrap();

        // Setup the test parameters with a verification window of 10 blocks and
        // the actual time elapsed being 11 blocks.
        let params = TestParams {
            new_aggregate_key: aggregate_key.into(),
            dkg_verification_window: 10,
            bitcoin_chain_tip: BitcoinBlockRef {
                block_hash: BitcoinBlockHash::from([0; 32]),
                block_height: 11u64.into(),
            },
            ..Default::default()
        };

        let result = params.execute(&db).await.unwrap_err();

        assert!(matches!(
            result,
            Error::DkgVerificationWindowElapsed(key) if aggregate_key == key
        ));

        testing::storage::drop_db(db).await;
    }

    #[tokio::test]
    async fn verification_window_is_inclusive() {
        let db = testing::storage::new_test_database().await;
        let aggregate_key: PublicKey = Keypair::new_global(&mut OsRng).public_key().into();

        // Create new DKG shares and store them in the database. We expect the
        // aggregate keys to match and the status to be allowed. We use 0 as the
        // starting block.
        let shares = EncryptedDkgShares {
            aggregate_key,
            dkg_shares_status: DkgSharesStatus::Unverified,
            started_at_bitcoin_block_height: 0u64.into(),
            ..Faker.fake()
        };
        db.write_encrypted_dkg_shares(&shares).await.unwrap();

        // Setup the test parameters with a verification window of 10 blocks and
        // the actual time elapsed being 10 blocks. Tests that the verification
        // window is inclusive.
        let params = TestParams {
            new_aggregate_key: aggregate_key.into(),
            dkg_verification_window: 10,
            bitcoin_chain_tip: BitcoinBlockRef {
                block_hash: BitcoinBlockHash::from([0; 32]),
                block_height: 10u64.into(),
            },
            ..Default::default()
        };

        params.execute(&db).await.unwrap();

        testing::storage::drop_db(db).await;
    }

    #[tokio::test]
    async fn expected_sighash_succeeds() {
        let db = testing::storage::new_test_database().await;
        let aggregate_key: PublicKey = Keypair::new_global(&mut OsRng).public_key().into();

        // Create new DKG shares and store them in the database. We expect
        // all other verifications to succeed.
        let shares = EncryptedDkgShares {
            aggregate_key,
            dkg_shares_status: DkgSharesStatus::Unverified,
            started_at_bitcoin_block_height: 0u64.into(),
            ..Faker.fake()
        };
        db.write_encrypted_dkg_shares(&shares).await.unwrap();

        let sighash = UnsignedMockTransaction::new(aggregate_key.into())
            .compute_sighash()
            .unwrap();

        // Setup the test parameters using the expected sighash.
        let params = TestParams {
            new_aggregate_key: aggregate_key.into(),
            message: Some(sighash.as_byte_array().to_vec()),
            ..Default::default()
        };

        params.execute(&db).await.unwrap();

        testing::storage::drop_db(db).await;
    }

    #[tokio::test]
    async fn unexpected_sighash_fails() {
        let db = testing::storage::new_test_database().await;
        let aggregate_key: PublicKey = Keypair::new_global(&mut OsRng).public_key().into();

        // Create new DKG shares and store them in the database. We expect
        // all other verifications to succeed.
        let shares = EncryptedDkgShares {
            aggregate_key,
            dkg_shares_status: DkgSharesStatus::Unverified,
            started_at_bitcoin_block_height: 0u64.into(),
            ..Faker.fake()
        };
        db.write_encrypted_dkg_shares(&shares).await.unwrap();

        // Setup the test parameters using a random sighash, which we expect
        // to fail validation.
        let params = TestParams {
            new_aggregate_key: aggregate_key.into(),
            dkg_verification_window: 10,
            bitcoin_chain_tip: BitcoinBlockRef {
                block_hash: BitcoinBlockHash::from([0; 32]),
                block_height: 10u64.into(),
            },
            message: Some(Faker.fake()),
        };

        let result = params.execute(&db).await.unwrap_err();

        assert!(matches!(result, Error::InvalidSigHash(_)));

        testing::storage::drop_db(db).await;
    }
}
