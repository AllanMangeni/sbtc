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

/// DepositWithStatus : Wrapper for deposit with status code. Used for multi-status responses. Note: logically, exactly one field among `error` and `deposit` should be `None`, and exactly one should be `Some`, so, storing them as `Result` would be more correct. However, utopia, which we use for openAPI schema generation, does not allow `Result` usage in its structs, and we have to use two `Option`s
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositWithStatus {
    #[serde(
        rename = "deposit",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub deposit: Option<Option<Box<models::Deposit>>>,
    /// A string explaining the error that occurred during the deposit update.
    #[serde(
        rename = "error",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub error: Option<Option<String>>,
    /// HTTP status code for the deposit processing result.
    #[serde(rename = "status")]
    pub status: u32,
}

impl DepositWithStatus {
    /// Wrapper for deposit with status code. Used for multi-status responses. Note: logically, exactly one field among `error` and `deposit` should be `None`, and exactly one should be `Some`, so, storing them as `Result` would be more correct. However, utopia, which we use for openAPI schema generation, does not allow `Result` usage in its structs, and we have to use two `Option`s
    pub fn new(status: u32) -> DepositWithStatus {
        DepositWithStatus {
            deposit: None,
            error: None,
            status,
        }
    }
}
