//! Route definitions for the withdrawal endpoint.
use warp::Filter;

use crate::context::EmilyContext;

use super::handlers;

/// Withdrawal routes.
pub fn routes(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    get_withdrawal(context.clone())
        .or(get_withdrawals(context.clone()))
        .or(get_withdrawals_for_recipient(context.clone()))
        .or(get_withdrawals_for_sender(context.clone()))
        .or(create_withdrawal(context.clone()))
        .or(update_withdrawals_sidecar(context.clone()))
        .or(update_withdrawals_signer(context))
}

/// Get withdrawal endpoint.
fn get_withdrawal(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::any()
        .map(move || context.clone())
        .and(warp::path!("withdrawal" / u64))
        .and(warp::get())
        .then(handlers::withdrawal::get_withdrawal)
}

/// Get withdrawals endpoint.
fn get_withdrawals(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::any()
        .map(move || context.clone())
        .and(warp::path("withdrawal"))
        .and(warp::get())
        .and(warp::query())
        .then(handlers::withdrawal::get_withdrawals)
}

/// Get withdrawals for recipient endpoint.
fn get_withdrawals_for_recipient(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::any()
        .map(move || context.clone())
        .and(warp::path!("withdrawal" / "recipient" / String))
        .and(warp::get())
        .and(warp::query())
        .then(handlers::withdrawal::get_withdrawals_for_recipient)
}

/// Get withdrawals for sender endpoint.
fn get_withdrawals_for_sender(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::any()
        .map(move || context.clone())
        .and(warp::path!("withdrawal" / "sender" / String))
        .and(warp::get())
        .and(warp::query())
        .then(handlers::withdrawal::get_withdrawals_for_sender)
}

/// Create withdrawal endpoint.
fn create_withdrawal(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::any()
        .map(move || context.clone())
        .and(warp::path("withdrawal"))
        .and(warp::post())
        .and(warp::body::json())
        .then(handlers::withdrawal::create_withdrawal)
}

/// Update withdrawals from signer endpoint.
fn update_withdrawals_signer(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::any()
        .map(move || context.clone())
        .and(warp::path("withdrawal"))
        .and(warp::put())
        .and(warp::body::json())
        .then(handlers::withdrawal::update_withdrawals_signer)
}

/// Update withdrawals from sidecar endpoint.
fn update_withdrawals_sidecar(
    context: EmilyContext,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    warp::any()
        .map(move || context.clone())
        .and(warp::path("withdrawal_private"))
        .and(warp::put())
        .and(warp::body::json())
        .then(handlers::withdrawal::update_withdrawals_sidecar)
}

// TODO(387): Add route unit tests.
