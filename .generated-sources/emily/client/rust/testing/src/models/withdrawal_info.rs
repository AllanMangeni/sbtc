/*
 * emily-openapi-spec
 *
 * No description provided (generated by Openapi Generator https://github.com/openapitools/openapi-generator)
 *
 * The version of the OpenAPI document: 0.1.0
 *
 * Generated by: https://openapi-generator.tech
 */

use crate::models;
use serde::{Deserialize, Serialize};

/// WithdrawalInfo : Reduced version of the Withdrawal.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct WithdrawalInfo {
    /// Amount of BTC being withdrawn in satoshis.
    #[serde(rename = "amount")]
    pub amount: u64,
    /// The most recent Stacks block hash the API was aware of when the withdrawal was last updated. If the most recent update is tied to an artifact on the Stacks blockchain then this hash is the Stacks block hash that contains that artifact.
    #[serde(rename = "lastUpdateBlockHash")]
    pub last_update_block_hash: String,
    /// The most recent Stacks block height the API was aware of when the withdrawal was last updated. If the most recent update is tied to an artifact on the Stacks blockchain then this height is the Stacks block height that contains that artifact.
    #[serde(rename = "lastUpdateHeight")]
    pub last_update_height: u64,
    /// The recipient's hex-encoded Bitcoin scriptPubKey.
    #[serde(rename = "recipient")]
    pub recipient: String,
    /// The id of the Stacks withdrawal request that initiated the sBTC operation.
    #[serde(rename = "requestId")]
    pub request_id: u64,
    /// The sender's hex-encoded Stacks principal.
    #[serde(rename = "sender")]
    pub sender: String,
    /// The stacks block hash in which this request id was initiated.
    #[serde(rename = "stacksBlockHash")]
    pub stacks_block_hash: String,
    /// The height of the Stacks block in which this request id was initiated.
    #[serde(rename = "stacksBlockHeight")]
    pub stacks_block_height: u64,
    #[serde(rename = "status")]
    pub status: models::WithdrawalStatus,
    /// The hex encoded txid of the stacks transaction that generated this event.
    #[serde(rename = "txid")]
    pub txid: String,
}

impl WithdrawalInfo {
    /// Reduced version of the Withdrawal.
    pub fn new(
        amount: u64,
        last_update_block_hash: String,
        last_update_height: u64,
        recipient: String,
        request_id: u64,
        sender: String,
        stacks_block_hash: String,
        stacks_block_height: u64,
        status: models::WithdrawalStatus,
        txid: String,
    ) -> WithdrawalInfo {
        WithdrawalInfo {
            amount,
            last_update_block_hash,
            last_update_height,
            recipient,
            request_id,
            sender,
            stacks_block_hash,
            stacks_block_height,
            status,
            txid,
        }
    }
}
