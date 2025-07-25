use std::collections::HashSet;
use std::time::Duration;

use bitcoin::AddressType;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::Txid;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::hashes::Hash as _;
use bitcoincore_rpc_json::Utxo;
use fake::Fake as _;
use futures::future::join_all;
use signer::testing::storage::model::TestBitcoinTxInfo;
use test_case::test_case;
use test_log::test;
use url::Url;

use blockstack_lib::net::api::getpoxinfo::RPCPoxInfoData;
use blockstack_lib::net::api::getsortition::SortitionInfo;
use clarity::types::chainstate::BurnchainHeaderHash;
use emily_client::apis::deposit_api;
use emily_client::models::CreateDepositRequestBody;
use emily_client::models::DepositStatus;
use emily_client::models::DepositUpdate;
use emily_client::models::UpdateDepositsRequestBody;
use sbtc::testing::regtest::Recipient;
use signer::bitcoin::rpc::BitcoinBlockInfo;
use signer::bitcoin::rpc::BitcoinTxInfo;
use signer::bitcoin::rpc::GetTxResponse;
use signer::block_observer;
use signer::context::Context;
use signer::context::RequestDeciderEvent;
use signer::emily_client::EmilyClient;
use signer::emily_client::EmilyInteract;
use signer::error::Error;
use signer::keys;
use signer::keys::PublicKey;
use signer::keys::SignerScriptPubKey as _;
use signer::network;
use signer::stacks::api::TenureBlocks;
use signer::storage::DbRead;
use signer::storage::DbWrite;
use signer::storage::model;
use signer::storage::model::DepositSigner;
use signer::testing;
use signer::testing::context::BuildContext;
use signer::testing::context::ConfigureBitcoinClient;
use signer::testing::context::ConfigureEmilyClient;
use signer::testing::context::ConfigureStacksClient;
use signer::testing::context::ConfigureStorage;
use signer::testing::context::TestContext;
use signer::testing::context::WrappedMock;
use signer::testing::get_rng;
use signer::testing::stacks::DUMMY_SORTITION_INFO;
use signer::testing::stacks::DUMMY_TENURE_INFO;
use signer::testing::storage::model::TestData;
use signer::testing::transaction_coordinator::select_coordinator;
use signer::testing::wsts::SignerSet;
use signer::transaction_coordinator;
use testing_emily_client::apis::testing_api::wipe_databases;

use crate::setup::IntoEmilyTestingConfig as _;
use crate::utxo_construction::make_deposit_request;

async fn run_dkg<Rng, C>(
    ctx: &C,
    rng: &mut Rng,
    signer_set: &mut SignerSet,
) -> (keys::PublicKey, model::BitcoinBlockRef, TestData)
where
    C: Context + Send + Sync,
    Rng: rand::CryptoRng + rand::RngCore,
{
    let storage = ctx.get_storage_mut();
    let signer_keys = signer_set.signer_keys();

    let test_model_parameters = testing::storage::model::Params {
        num_bitcoin_blocks: 20,
        num_stacks_blocks_per_bitcoin_block: 3,
        num_deposit_requests_per_block: 0,
        num_withdraw_requests_per_block: 0,
        num_signers_per_request: 0,
        consecutive_blocks: false,
    };
    let test_data = TestData::generate(rng, &signer_keys, &test_model_parameters);
    test_data.write_to(&storage).await;

    let bitcoin_chain_tip = storage
        .get_bitcoin_canonical_chain_tip()
        .await
        .expect("storage error")
        .expect("no chain tip");

    let bitcoin_chain_tip_ref = storage
        .get_bitcoin_block(&bitcoin_chain_tip)
        .await
        .expect("storage failure")
        .expect("missing block")
        .into();

    let dkg_txid = testing::dummy::txid(&fake::Faker, rng);
    let (aggregate_key, all_dkg_shares) = signer_set
        .run_dkg(
            bitcoin_chain_tip,
            dkg_txid.into(),
            model::DkgSharesStatus::Verified,
        )
        .await;

    let encrypted_dkg_shares = all_dkg_shares.first().unwrap();
    signer_set
        .write_as_rotate_keys_tx(&storage, &bitcoin_chain_tip, encrypted_dkg_shares, rng)
        .await;

    storage
        .write_encrypted_dkg_shares(encrypted_dkg_shares)
        .await
        .expect("failed to write encrypted shares");

    (aggregate_key, bitcoin_chain_tip_ref, test_data)
}

/// End to end test for deposits via Emily: a deposit request is created on Emily,
/// then is picked up by the block observer, inserted into the storage and accepted.
/// After a signing round, the sweep tx for the request is broadcasted and we check
/// that Emily is informed about it.
#[ignore = "this test will be fixed shortly"]
#[test(tokio::test)]
async fn deposit_flow() {
    let num_signers = 7;
    let signing_threshold = 5;
    let context_window = 10;

    let db = testing::storage::new_test_database().await;
    let mut rng = get_rng();
    let network = network::in_memory::InMemoryNetwork::new();
    let signer_info = testing::wsts::generate_signer_info(&mut rng, num_signers);

    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();
    let stacks_client = WrappedMock::default();

    // Wipe the Emily database to start fresh
    wipe_databases(&emily_client.config().as_testing())
        .await
        .expect("Wiping Emily database in test setup failed.");

    let context = TestContext::builder()
        .with_storage(db.clone())
        .with_mocked_bitcoin_client()
        .with_stacks_client(stacks_client.clone())
        .with_emily_client(emily_client.clone())
        .build();

    let mut testing_signer_set =
        testing::wsts::SignerSet::new(&signer_info, signing_threshold, || network.connect());

    let (aggregate_key, bitcoin_chain_tip, mut test_data) =
        run_dkg(&context, &mut rng, &mut testing_signer_set).await;

    let original_test_data = test_data.clone();

    // Setup some UTXO for the signers
    let signers_utxo_tx = Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(1_337_000_000_000),
            script_pubkey: aggregate_key.signers_script_pubkey(),
        }],
        input: vec![TestBitcoinTxInfo::random_prevout(&mut rng)],
    };
    let signer_script_pubkeys = HashSet::from([aggregate_key.signers_script_pubkey()]);
    let tx_info = TestBitcoinTxInfo {
        tx: signers_utxo_tx.clone(),
        prevouts: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(1000),
            script_pubkey: aggregate_key.signers_script_pubkey(),
        }],
    };
    test_data.push_bitcoin_txs(&bitcoin_chain_tip, vec![tx_info], &signer_script_pubkeys);
    test_data.remove(original_test_data);
    test_data.write_to(&context.get_storage_mut()).await;

    // Setup deposit request
    let deposit_amount = fake::Faker.fake_with_rng(&mut rng);
    let deposit_max_fee = fake::Faker.fake_with_rng(&mut rng);
    let depositor = Recipient::new(AddressType::P2tr);
    let depositor_utxo = Utxo {
        txid: Txid::all_zeros(),
        vout: 0,
        script_pub_key: ScriptBuf::new(),
        descriptor: "".to_string(),
        amount: Amount::from_sat(deposit_amount + deposit_max_fee),
        height: 0,
    };
    let max_fee = deposit_amount / 2;
    let (deposit_tx, deposit_request, _) = make_deposit_request(
        &depositor,
        deposit_amount,
        depositor_utxo,
        max_fee,
        aggregate_key.into(),
    );

    let emily_request = CreateDepositRequestBody {
        bitcoin_tx_output_index: deposit_request.outpoint.vout,
        bitcoin_txid: deposit_request.outpoint.txid.to_string(),
        deposit_script: deposit_request.deposit_script.to_hex_string(),
        reclaim_script: deposit_request.reclaim_script.to_hex_string(),
        transaction_hex: serialize_hex(&deposit_tx),
    };

    // Create a fresh block for the block observer to process
    let mut deposit_block: BitcoinBlockInfo = fake::Faker.fake_with_rng(&mut rng);
    deposit_block
        .transactions
        .push(deposit_tx.fake_with_rng(&mut rng));

    let deposit_block_hash = deposit_block.block_hash;

    // Mock required bitcoin client functions
    context
        .with_bitcoin_client(|client| {
            client
                .expect_estimate_fee_rate()
                .once()
                // Dummy value
                .returning(|| Box::pin(async { Ok(1.3) }));
        })
        .await;

    // Create a channel to log all transactions broadcasted by the coordinator.
    // The receiver is created by this method but not used as it is held as a
    // handle to ensure that the channel is alive until the end of the test.
    // This is because the coordinator will produce multiple transactions after
    // the first, and it will panic trying to send to the channel if it is closed
    // (even though we don't use those transactions).
    let (broadcasted_transaction_tx, _broadcasted_transaction_rxeiver) =
        tokio::sync::broadcast::channel(1);

    // This task logs all transactions broadcasted by the coordinator.
    let mut wait_for_transaction_rx = broadcasted_transaction_tx.subscribe();
    let wait_for_transaction_task =
        tokio::spawn(async move { wait_for_transaction_rx.recv().await });

    context
        .with_bitcoin_client(|client| {
            // Setup the bitcoin client mock to broadcast the transaction to our
            // channel.
            client
                .expect_broadcast_transaction()
                .once()
                .returning(move |tx| {
                    let tx = tx.clone();
                    let broadcasted_transaction_tx = broadcasted_transaction_tx.clone();
                    Box::pin(async move {
                        broadcasted_transaction_tx
                            .send(tx)
                            .expect("Failed to send result");
                        Ok(())
                    })
                });

            // Return the deposit tx
            let deposit_tx_ = deposit_tx.clone();
            client.expect_get_tx().once().returning(move |txid| {
                let res = if *txid == deposit_tx_.compute_txid() {
                    Ok(Some(GetTxResponse {
                        tx: deposit_tx_.clone(),
                        block_hash: Some(deposit_block_hash),
                        confirmations: None,
                        block_time: None,
                    }))
                } else {
                    // We may get queried for unrelated txids if Emily state
                    // was not reset; returning an error will ignore those
                    // deposit requests (as desired).
                    Err(Error::BitcoinTxMissing(*txid, None))
                };
                Box::pin(async move { res })
            });

            // Return the deposit tx
            client
                .expect_get_tx_info()
                .once()
                .returning(move |txid, _| {
                    let res = if *txid == deposit_tx.compute_txid() {
                        Ok(Some(BitcoinTxInfo {
                            fee: Some(bitcoin::Amount::from_sat(deposit_max_fee)),
                            tx: deposit_tx.clone(),
                            vin: Vec::new(),
                        }))
                    } else {
                        // We may get queried for unrelated txids if Emily state
                        // was not reset; returning an error will ignore those
                        // deposit requests (as desired).
                        Err(Error::BitcoinTxMissing(*txid, None))
                    };
                    Box::pin(async move { res })
                });

            // Return the deposit tx block, when the block observer will query us for it
            // when processing the new block; as its parent is already in storage
            // we don't need to provide any other blocks.
            client
                .expect_get_block()
                .once()
                .returning(move |block_hash| {
                    let res = if *block_hash == deposit_block_hash {
                        Ok(Some(deposit_block.clone()))
                    } else {
                        Err(Error::MissingBlock)
                    };
                    Box::pin(async move { res })
                });
        })
        .await;

    // Also mock stacks client (to return no new blocks)
    context
        .with_stacks_client(|client| {
            client
                .expect_get_tenure_info()
                .once()
                .returning(move || Box::pin(async move { Ok(DUMMY_TENURE_INFO.clone()) }));

            client
                .expect_get_tenure()
                .once()
                .returning(|_| Box::pin(std::future::ready(TenureBlocks::nearly_empty())));

            client.expect_get_pox_info().once().returning(|| {
                let raw_json_response =
                    include_str!("../../tests/fixtures/stacksapi-get-pox-info-test-data.json");
                Box::pin(async move {
                    serde_json::from_str::<RPCPoxInfoData>(raw_json_response)
                        .map_err(Error::JsonSerialize)
                })
            });

            // The coordinator may try to further process the deposit to submit
            // the stacks tx, but we are not interested (for the current test iteration).
            client
                .expect_get_account()
                .returning(|_| Box::pin(async move { Err(Error::InvalidStacksResponse("mock")) }));

            client.expect_get_sortition_info().returning(move |_| {
                let response = Ok(SortitionInfo {
                    burn_block_hash: BurnchainHeaderHash::from_bytes_be(
                        bitcoin_chain_tip.block_hash.as_byte_array(),
                    )
                    .unwrap(),
                    ..DUMMY_SORTITION_INFO.clone()
                });
                Box::pin(std::future::ready(response))
            });

            client
                .expect_get_current_signers_aggregate_key()
                .returning(move |_| Box::pin(std::future::ready(Ok(Some(aggregate_key)))));
        })
        .await;

    let (block_observer_stream_tx, block_observer_stream_rx) = tokio::sync::mpsc::channel(1);
    let block_stream: tokio_stream::wrappers::ReceiverStream<Result<bitcoin::BlockHash, Error>> =
        block_observer_stream_rx.into();

    let block_observer = block_observer::BlockObserver {
        context: context.clone(),
        bitcoin_blocks: block_stream,
    };

    let block_observer_handle = tokio::spawn(async move { block_observer.run().await });

    // Get the private key of the coordinator of the signer set.
    let private_key = select_coordinator(&deposit_block_hash.into(), &signer_info);

    // Bootstrap the tx coordinator event loop
    context.state().set_sbtc_contracts_deployed();
    let tx_coordinator = transaction_coordinator::TxCoordinatorEventLoop {
        context: context.clone(),
        network: network.connect(),
        private_key,
        context_window,
        threshold: signing_threshold as u16,
        signing_round_max_duration: Duration::from_secs(10),
        dkg_max_duration: Duration::from_secs(10),
        bitcoin_presign_request_max_duration: Duration::from_secs(10),
        is_epoch3: true,
    };
    let tx_coordinator_handle = tokio::spawn(async move { tx_coordinator.run().await });

    // There shouldn't be any request yet
    let signer_public_key = PublicKey::from_private_key(&context.config().signer.private_key);
    let chain_tip = bitcoin_chain_tip.block_hash;
    assert!(
        context
            .get_storage()
            .get_pending_deposit_requests(&chain_tip, context_window, &signer_public_key)
            .await
            .unwrap()
            .is_empty()
    );

    // Create deposit in Emily
    deposit_api::create_deposit(emily_client.config(), emily_request.clone())
        .await
        .expect("cannot create emily deposit");

    // Wake up block observer to process the new block
    block_observer_stream_tx
        .send(Ok(deposit_block_hash))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Ensure we picked up the new tip
    assert_eq!(
        context
            .get_storage()
            .get_bitcoin_canonical_chain_tip()
            .await
            .unwrap()
            .unwrap(),
        deposit_block_hash.into()
    );
    // and that now we have the deposit request
    let deposit_block = deposit_block_hash.into();
    assert!(
        !context
            .get_storage()
            .get_pending_deposit_requests(&deposit_block, context_window, &signer_public_key)
            .await
            .unwrap()
            .is_empty()
    );

    let stacks_tip = context
        .get_storage()
        .get_stacks_chain_tip(&bitcoin_chain_tip.block_hash)
        .await
        .unwrap()
        .unwrap();

    // We also need to accept the request, so let's pick some signer to accept it
    let public_keys = signer_info[0]
        .signer_public_keys
        .iter()
        .take(signing_threshold as usize);
    for signer_pub_key in public_keys {
        context
            .get_storage_mut()
            .write_deposit_signer_decision(&DepositSigner {
                txid: deposit_request.outpoint.txid.into(),
                output_index: deposit_request.outpoint.vout,
                signer_pub_key: *signer_pub_key,
                can_accept: true,
                can_sign: true,
            })
            .await
            .expect("failed to write deposit decision");
    }

    // Start the in-memory signer set.
    let _signers_handle = tokio::spawn(async move {
        testing_signer_set
            .participate_in_signing_rounds_forever()
            .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check emily api the request is still pending
    let fetched_deposit = deposit_api::get_deposit(
        emily_client.config(),
        &emily_request.bitcoin_txid,
        &emily_request.bitcoin_tx_output_index.to_string(),
    )
    .await
    .expect("cannot get deposit from emily");

    assert_eq!(
        fetched_deposit.status,
        emily_client::models::DepositStatus::Pending
    );

    // Wake coordinator up (again)
    context
        .signal(RequestDeciderEvent::NewRequestsHandled.into())
        .expect("failed to signal");

    // Await the `wait_for_tx_task` to receive the first transaction broadcasted.
    let broadcasted_tx = tokio::time::timeout(Duration::from_secs(10), wait_for_transaction_task)
        .await
        .unwrap()
        .expect("failed to receive message")
        .expect("no message received");

    // Ensure we have time to send the emily api call before stopping everything
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Stop the event loops
    tx_coordinator_handle.abort();
    block_observer_handle.abort();

    // Extract the first script pubkey from the broadcasted transaction.
    let first_script_pubkey = broadcasted_tx
        .tx_out(0)
        .expect("missing tx output")
        .script_pubkey
        .clone();

    assert_eq!(first_script_pubkey, aggregate_key.signers_script_pubkey());

    // Check emily api for the updated request
    let fetched_deposit = deposit_api::get_deposit(
        emily_client.config(),
        &emily_request.bitcoin_txid,
        &emily_request.bitcoin_tx_output_index.to_string(),
    )
    .await
    .expect("cannot get deposit from emily");

    assert_eq!(
        fetched_deposit.status,
        emily_client::models::DepositStatus::Accepted
    );
    assert_eq!(
        fetched_deposit.last_update_block_hash,
        stacks_tip.block_hash.to_string()
    );
    assert_eq!(fetched_deposit.last_update_height, *stacks_tip.block_height);

    testing::storage::drop_db(db).await;
}

#[tokio::test]
async fn get_deposit_request_works() {
    let max_fee: u64 = 15000;
    let amount_sats = 49_900_000;
    let lock_time = 150;

    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(1),
        None,
    )
    .unwrap();

    wipe_databases(&emily_client.config().as_testing())
        .await
        .expect("Wiping Emily database in test setup failed.");

    let setup = sbtc::testing::deposits::tx_setup(lock_time, max_fee, &[amount_sats]);
    let deposit = setup.deposits.first().unwrap();
    let reclaim = setup.reclaims.first().unwrap();

    let emily_request = CreateDepositRequestBody {
        bitcoin_tx_output_index: 0,
        bitcoin_txid: setup.tx.compute_txid().to_string(),
        deposit_script: deposit.deposit_script().to_hex_string(),
        reclaim_script: reclaim.reclaim_script().to_hex_string(),
        transaction_hex: serialize_hex(&setup.tx),
    };

    deposit_api::create_deposit(emily_client.config(), emily_request.clone())
        .await
        .expect("cannot create emily deposit");

    let txid = setup.tx.compute_txid().into();
    let request = emily_client.get_deposit(&txid, 0).await.unwrap().unwrap();

    assert_eq!(request.deposit_script, deposit.deposit_script());
    assert_eq!(request.reclaim_script, reclaim.reclaim_script());
    assert_eq!(request.outpoint.txid, setup.tx.compute_txid());
    assert_eq!(request.outpoint.vout, 0);

    // This one doesn't exist
    let request = emily_client.get_deposit(&txid, 50).await.unwrap();
    assert!(request.is_none());
}

#[test_case(3, 10, Some(2), 3; "handles paging")]
#[test_case(3, 0, Some(2), 2; "handles timeout")]
#[tokio::test]
async fn test_get_deposits_with_status_request_paging(
    num_deposits: usize,
    timeout_secs: u64,
    page_size: Option<u16>,
    expected_result: usize,
) {
    let max_fee: u64 = 15000;
    let amount_sats = 49_900_000;
    let lock_time = 150;

    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(timeout_secs),
        page_size,
    )
    .unwrap();

    wipe_databases(&emily_client.config().as_testing())
        .await
        .expect("Wiping Emily database in test setup failed.");

    let futures = (0..num_deposits).map(|_| {
        let setup = sbtc::testing::deposits::tx_setup(lock_time, max_fee, &[amount_sats]);
        let create_deposit_request_body = CreateDepositRequestBody {
            bitcoin_tx_output_index: 0,
            bitcoin_txid: setup.tx.compute_txid().to_string(),
            deposit_script: setup
                .deposits
                .first()
                .unwrap()
                .deposit_script()
                .to_hex_string(),
            reclaim_script: setup
                .reclaims
                .first()
                .unwrap()
                .reclaim_script()
                .to_hex_string(),
            transaction_hex: serialize_hex(&setup.tx),
        };
        deposit_api::create_deposit(emily_client.config(), create_deposit_request_body)
    });

    let results = join_all(futures).await;
    for result in results {
        result.expect("cannot create emily deposit");
    }

    let deposits = emily_client
        .get_deposits_with_status(DepositStatus::Pending)
        .await
        .unwrap();
    assert_eq!(deposits.len(), expected_result);
}

#[tokio::test]
async fn test_get_deposits_returns_pending_and_accepted() {
    let max_fee: u64 = 15000;
    let amount_sats = 49_900_000;
    let lock_time = 150;
    let num_deposits = 5;
    let num_accepted = 2;

    let emily_client = EmilyClient::try_new(
        &Url::parse("http://testApiKey@localhost:3031").unwrap(),
        Duration::from_secs(10),
        None,
    )
    .unwrap();

    wipe_databases(&emily_client.config().as_testing())
        .await
        .expect("Wiping Emily database in test setup failed.");

    // Create deposits
    let tx_setups: Vec<sbtc::testing::deposits::TxSetup> = (0..num_deposits)
        .map(|_| sbtc::testing::deposits::tx_setup(lock_time, max_fee, &[amount_sats]))
        .collect();
    let futures = tx_setups.iter().map(|setup| {
        let create_deposit_request_body = CreateDepositRequestBody {
            bitcoin_tx_output_index: 0,
            bitcoin_txid: setup.tx.compute_txid().to_string(),
            deposit_script: setup
                .deposits
                .first()
                .unwrap()
                .deposit_script()
                .to_hex_string(),
            reclaim_script: setup
                .reclaims
                .first()
                .unwrap()
                .reclaim_script()
                .to_hex_string(),
            transaction_hex: serialize_hex(&setup.tx),
        };
        deposit_api::create_deposit(emily_client.config(), create_deposit_request_body)
    });

    let results = join_all(futures).await;
    for result in results {
        result.expect("cannot create emily deposit");
    }

    // Update some deposits to accepted
    let deposits = tx_setups[0..num_accepted]
        .iter()
        .map(|setup| DepositUpdate {
            bitcoin_tx_output_index: 0,
            bitcoin_txid: setup.tx.compute_txid().to_string(),
            fulfillment: None,
            status: DepositStatus::Accepted,
            status_message: "accepted".to_string(),
            replaced_by_tx: None,
        })
        .collect();

    deposit_api::update_deposits_signer(
        emily_client.config(),
        UpdateDepositsRequestBody { deposits },
    )
    .await
    .expect("cannot update deposits");

    // Check that we get all deposits
    let deposits = emily_client.get_deposits().await.unwrap();
    let accepted_deposits = emily_client
        .get_deposits_with_status(DepositStatus::Accepted)
        .await
        .unwrap();
    let pending_deposits = emily_client
        .get_deposits_with_status(DepositStatus::Pending)
        .await
        .unwrap();

    assert_eq!(deposits.len(), num_deposits);
    assert_eq!(accepted_deposits.len(), num_accepted);
    assert_eq!(pending_deposits.len(), num_deposits - num_accepted);
}
