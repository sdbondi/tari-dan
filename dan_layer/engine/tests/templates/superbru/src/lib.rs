//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use std::collections::{BTreeMap, BTreeSet};

use tari_template_lib::prelude::*;

const FIRST_PRIZE: NonFungibleId = NonFungibleId::from_u32(1);
const SECOND_PRIZE: NonFungibleId = NonFungibleId::from_u32(2);
const THIRD_PRIZE: NonFungibleId = NonFungibleId::from_u32(3);

/// An event between two teams
pub struct Event {
    pub team_a: String,
    pub team_b: String,
}

pub struct Prediction {
    pub difference: i64,
}

pub struct FinalScore {
    pub team_a: u32,
    pub team_b: u32,
}

impl FinalScore {
    pub fn difference(&self) -> i64 {
        (self.team_a as i64) - (self.team_b as i64)
    }
}

// TODO: perhaps we should have a special ComponentAddress type just for accounts? Which implies native accounts.
pub type AccountAddress = ComponentAddress;

pub struct User {
    pub account_address: AccountAddress,
}
#[template]
mod state_template {
    use super::*;

    pub struct Superbru {
        game_id: NonFungibleId,
        event: Event,
        prediction_token: Vault,
        registered_players: BTreeSet<AccountAddress>,
        admin_badge: Vault,
        prizes: Vault,
        prize_pool: Vault,
        is_open: bool,
        // HashMap should be without a random seed, otherwise validators will not agree on the state hash
        predictions: BTreeMap<AccountAddress, Prediction>,
    }

    impl Superbru {
        pub fn create_pool(event: Event) -> SuperbruComponent {
            let game_id = NonFungibleId::random();

            let prizes = ResourceBuilder::non_fungible()
                .with_non_fungibles([
                    (FIRST_PRIZE, &(), &()),
                    (SECOND_PRIZE, &(), &()),
                    (THIRD_PRIZE, &(), &()),
                ])
                .build_bucket();

            let admin_bucket = ResourceBuilder::non_fungible()
                .with_non_fungible(NonFungibleId::from_u32(1), &(), &())
                .build_bucket();

            let prediction_token = ResourceBuilder::non_fungible()
                .mintable(Requires(admin_bucket.resource_address()))
                .mutate_metadata(Requires(admin_bucket.resource_address()))
                .default_rule(Deny)
                .no_initial_supply();

            let access_rules = AccessRules::new()
                .add_method_rule("register_user", AccessRule::AllowAll)
                .add_method_rule(
                    "make_prediction",
                    AccessRule::Restricted(Require(prediction_token.resource_address())),
                )
                .default(AccessRule::Restricted(Require(admin_bucket.resource_address())));

            Self {
                game_id,
                event,
                prediction_token: Vault::new_empty(prediction_token.resource_address()),
                registered_players: BTreeSet::new(),
                admin_badge: Vault::from_bucket(admin_bucket),
                prizes: Vault::from_bucket(prizes),
                prize_pool: Vault::new_empty(CONFIDENTIAL_TARI_RESOURCE_ADDRESS),
                is_open: true,
            }
            .with_access_rules(access_rules)
            .create()
        }

        pub fn register_for_game(&mut self, account_address: AccountAddress) -> Bucket {
            if !self.is_open {
                panic!("Pool is not open");
            }

            if self.registered_players.insert(account_address) {
                panic!("User has already registered");
            }

            ResourceManager::get(self.prediction_token.resource_address()).mint_non_fungible(
                NonFungibleId::random(),
                &User { account_address },
                &(),
            )
        }

        pub fn make_prediction(&mut self, token_proof: Proof, prediction: Prediction, payment: Bucket) {
            if !self.is_open {
                panic!("Pool is not open for predictions");
            }
            // Perhaps it is impossible to create Proof unless you have non-zero of them
            token_proof.verify_for_resource(&self.prediction_token.resource_address());

            let _auth = token.authorize().expect("not authorized");
            let user = token.get_immutable_metadata::<User>();

            // Or
            let access = token.authorize().expect("not authorized");
            let user = access.get_immutable_metadata::<User>();

            if self.predictions.contains_key(&user.account_address) {
                panic!("User has already made a prediction");
            }

            token.update_metadata("superbru-prediction", &prediction);
            // or
            access.update_metadata("superbru-prediction", &prediction);

            self.predictions.insert(user.account_address, prediction);
            self.prize_pool.deposit(payment);
        }

        pub fn complete_game(&mut self, score: Score) {
            if !self.is_open {
                panic!("Pool is not open");
            }
            self.is_open = false;
            let winners = self.calculate_winners(final_score);
            for (place, winner) in winners.iter().enumerate() {
                emit_event("superbru.game_won_by", &winner);
                self.award_winner(place, winner);
            }
        }

        fn award_winner(&mut self, place: usize, winner: &AccountAddress) {
            let prize = match place {
                0 => FIRST_PRIZE,
                1 => SECOND_PRIZE,
                2 => THIRD_PRIZE,
                _ => panic!("Invalid place"),
            };
            let prize_bucket = self.prizes.withdraw_non_fungible(prize);
            let user = ResourceManager::get(self.prediction_token.resource_address())
                .get_non_fungible()
                .get_immutable_metadata::<User>();

            ComponentManager::get(user.account_address).call::<()>("deposit", invoke_args![prize_bucket]);
            if place == 0 {
                let prize_pool = self.prize_pool.withdraw_all();
                ComponentManager::get(user.account_address).call::<()>("deposit", invoke_args![prize_pool]);
            }
            // or
            // AccountManager::get(user.account_address).deposit(invoke_args![prize_bucket]);
        }

        fn calculate_winners(&self, final_score: Score) -> Vec<AccountAddress> {
            // TODO: find top 3 predictions with the closest difference to the final score. If there are more than three
            //       ties then just return the winners we have so far.
            self.predictions.iter().take(3).map(|(k, _v)| *k).collect()
        }
    }
}

// Proof doesnt exist in the engine. This is like a bucket in that it can only be created if a token is owned but only
// provides proof of value without transferring that value.
pub struct Proof {
    resource_address: ResourceAddress,
    // do we have to lock up funds??? We arent mutating anything with the proof, so it could conceivably be spent
    // during locked: LockedResource
}

impl Proof {
    pub fn verify_for_resource(&self, resource_address: &ResourceAddress) {
        if self.resource_address != *resource_address {
            panic!("Invalid proof");
        }

        // This should probably be an invariant of a proof
        // if self.locked_funds.amount().is_zero() {
        //     panic!("Proof contained zero funds");
        // }
    }

    pub fn authorize(&self) -> AccessZone {
        AccessZone::new(self.resource_address)
    }

    pub fn get_immutable_metadata<T: DeserializeOwned>(&self) -> T {
        let access = self.authorize().expect("not authorized");
        access.get_immutable_metadata()
    }

    pub fn set_metadata<T: Serialize>(&self, key: &str, value: &T) {
        let access = self.authorize().expect("not authorized");
        access.set_metadata(key, value);
    }
}
