// Copyright 2022 The Tari Project
// SPDX-License-Identifier: BSD-3-Clause

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .format(false)
        .compile(&["proto/validator_node.proto"], &["proto"])?;

    Ok(())
}