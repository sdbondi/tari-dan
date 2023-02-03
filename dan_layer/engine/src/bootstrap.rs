//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use tari_engine_types::{
    resource::Resource,
    substate::{Substate, SubstateAddress},
};
use tari_template_lib::{constants::ED25519_RESOURCE, prelude::ResourceType};

use crate::state_store::{StateStoreError, StateWriter};

pub fn bootstrap_state<T: StateWriter>(state_db: &mut T) -> Result<(), StateStoreError> {
    let address = SubstateAddress::Resource(ED25519_RESOURCE);
    state_db.set_state(
        &address.clone(),
        Substate::new(
            address,
            0,
            Resource::new(ResourceType::NonFungible, ED25519_RESOURCE, Default::default()),
        ),
    )?;

    Ok(())
}