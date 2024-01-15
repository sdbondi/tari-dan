//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};

#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    #[error("Error: {0}")]
    Anyhow(#[from] anyhow::Error),
    #[error("JRPC error: {0}")]
    JrpcError(#[from] JsonRpcError),
    #[error("Not found")]
    NotFound,
}

pub fn json_rpc_error<T: Into<String>>(reason: JsonRpcErrorReason, message: T) -> anyhow::Error {
    JsonRpcError::new(reason, message.into(), serde_json::Value::Null).into()
}

pub const APP_ERR_NOT_FOUND: JsonRpcErrorReason = JsonRpcErrorReason::ApplicationError(404);
pub const APP_ERR_TRANSACTION_REJECTED: JsonRpcErrorReason = JsonRpcErrorReason::ApplicationError(1);
pub const APP_ERR_UNKNOWN: JsonRpcErrorReason = JsonRpcErrorReason::ApplicationError(500);
pub const APP_ERR_UNAUTHORIZED: JsonRpcErrorReason = JsonRpcErrorReason::ApplicationError(401);
