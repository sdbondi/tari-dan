//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use crate::{models::ResourceAddress, Hash};

// TODO: This is set pretty arbitrarily.
/// Resource address for all ED25519-based non-fungible tokens.
/// This resource provides a space for a virtual token representing ownership based on a ED25519 public key.
pub const ED25519_RESOURCE: ResourceAddress = ResourceAddress::new(Hash::from_array([
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]));
