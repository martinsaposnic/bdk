// Bitcoin Dev Kit
// Written in 2020 by Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020-2021 Bitcoin Dev Kit Developers
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Coin selection
//!
//! This module provides the trait [`CoinSelectionAlgorithm`] that can be implemented to
//! define custom coin selection algorithms.
//!
//! You can specify a custom coin selection algorithm through the [`coin_selection`] method on
//! [`TxBuilder`]. [`DefaultCoinSelectionAlgorithm`] aliases the coin selection algorithm that will
//! be used if it is not explicitly set.
//!
//! [`TxBuilder`]: super::tx_builder::TxBuilder
//! [`coin_selection`]: super::tx_builder::TxBuilder::coin_selection
//!
//! ## Example
//!
//! ```
//! # use std::str::FromStr;
//! # use bitcoin::*;
//! # use bdk_wallet::{self, ChangeSet, coin_selection::*, coin_selection};
//! # use bdk_wallet::error::CreateTxError;
//! # use bdk_wallet::*;
//! # use bdk_wallet::coin_selection::decide_change;
//! # use anyhow::Error;
//! # use rand_core::RngCore;
//! #[derive(Debug)]
//! struct AlwaysSpendEverything;
//!
//! impl CoinSelectionAlgorithm for AlwaysSpendEverything {
//!     fn coin_select<R: RngCore>(
//!         &self,
//!         params: CoinSelectionParams<'_, R>,
//!     ) -> Result<CoinSelectionResult, coin_selection::InsufficientFunds> {
//!         let CoinSelectionParams {
//!             required_utxos,
//!             optional_utxos,
//!             fee_rate,
//!             target_amount,
//!             drain_script,
//!             rand: _,
//!             avoid_partial_spends,
//!         } = params;
//!         let mut selected_amount = Amount::ZERO;
//!         let mut additional_weight = Weight::ZERO;
//!         let all_utxos_selected = required_utxos
//!             .into_iter()
//!             .chain(optional_utxos)
//!             .scan(
//!                 (&mut selected_amount, &mut additional_weight),
//!                 |(selected_amount, additional_weight), weighted_utxo| {
//!                     **selected_amount += weighted_utxo.utxo.txout().value;
//!                     **additional_weight += TxIn::default()
//!                         .segwit_weight()
//!                         .checked_add(weighted_utxo.satisfaction_weight)
//!                         .expect("`Weight` addition should not cause an integer overflow");
//!                     Some(weighted_utxo.utxo)
//!                 },
//!             )
//!             .collect::<Vec<_>>();
//!         let additional_fees = fee_rate * additional_weight;
//!         let amount_needed_with_fees = additional_fees + target_amount;
//!         if selected_amount < amount_needed_with_fees {
//!             return Err(coin_selection::InsufficientFunds {
//!                 needed: amount_needed_with_fees,
//!                 available: selected_amount,
//!             });
//!         }
//!
//!         let remaining_amount = selected_amount - amount_needed_with_fees;
//!
//!         let excess = decide_change(remaining_amount, fee_rate, drain_script);
//!
//!         Ok(CoinSelectionResult {
//!             selected: all_utxos_selected,
//!             fee_amount: additional_fees,
//!             excess,
//!         })
//!     }
//! }
//!
//! # let mut wallet = doctest_wallet!();
//! // create wallet, sync, ...
//!
//! let to_address = Address::from_str("2N4eQYCbKUHCCTUjBJeHcJp9ok6J2GZsTDt")
//!     .unwrap()
//!     .require_network(Network::Testnet)
//!     .unwrap();
//! let psbt = {
//!     let mut builder = wallet.build_tx().coin_selection(AlwaysSpendEverything);
//!     builder.add_recipient(to_address.script_pubkey(), Amount::from_sat(50_000));
//!     builder.finish()?
//! };
//!
//! // inspect, sign, broadcast, ...
//!
//! # Ok::<(), anyhow::Error>(())
//! ```

use crate::chain::collections::HashSet;
use crate::wallet::utils::IsDust;
use crate::Utxo;
use crate::WeightedUtxo;
use bitcoin::{Amount, FeeRate, SignedAmount};

use alloc::vec::Vec;
use bitcoin::consensus::encode::serialize;
use bitcoin::OutPoint;
use bitcoin::TxIn;
use bitcoin::{Script, Weight};

use chain::bdk_core::collections::HashMap;
use core::convert::TryInto;
use core::fmt::{self, Formatter};
use rand_core::RngCore;

use super::utils::shuffle_slice;
/// Default coin selection algorithm used by [`TxBuilder`](super::tx_builder::TxBuilder) if not
/// overridden
pub type DefaultCoinSelectionAlgorithm = BranchAndBoundCoinSelection<SingleRandomDraw>;

/// Wallet's UTXO set is not enough to cover recipient's requested plus fee.
///
/// This is thrown by [`CoinSelectionAlgorithm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsufficientFunds {
    /// Amount needed for the transaction
    pub needed: Amount,
    /// Amount available for spending
    pub available: Amount,
}

impl fmt::Display for InsufficientFunds {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Insufficient funds: {} available of {} needed",
            self.available, self.needed
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InsufficientFunds {}

#[derive(Debug)]
/// Remaining amount after performing coin selection
pub enum Excess {
    /// It's not possible to create spendable output from excess using the current drain output
    NoChange {
        /// Threshold to consider amount as dust for this particular change script_pubkey
        dust_threshold: Amount,
        /// Exceeding amount of current selection over outgoing value and fee costs
        remaining_amount: Amount,
        /// The calculated fee for the drain TxOut with the selected script_pubkey
        change_fee: Amount,
    },
    /// It's possible to create spendable output from excess using the current drain output
    Change {
        /// Effective amount available to create change after deducting the change output fee
        amount: Amount,
        /// The deducted change output fee
        fee: Amount,
    },
}

/// Result of a successful coin selection
#[derive(Debug)]
pub struct CoinSelectionResult {
    /// List of outputs selected for use as inputs
    pub selected: Vec<Utxo>,
    /// Total fee amount for the selected utxos
    pub fee_amount: Amount,
    /// Remaining amount after deducing fees and outgoing outputs
    pub excess: Excess,
}

impl CoinSelectionResult {
    /// The total value of the inputs selected.
    pub fn selected_amount(&self) -> Amount {
        self.selected.iter().map(|u| u.txout().value).sum()
    }

    /// The total value of the inputs selected from the local wallet.
    pub fn local_selected_amount(&self) -> Amount {
        self.selected
            .iter()
            .filter_map(|u| match u {
                Utxo::Local(_) => Some(u.txout().value),
                _ => None,
            })
            .sum()
    }
}

/// Params for coin selection
#[derive(Debug)]
pub struct CoinSelectionParams<'a, R: RngCore> {
    /// - `required_utxos`: the utxos that must be spent regardless of `target_amount` with their weight cost
    pub required_utxos: Vec<WeightedUtxo>,
    /// - `optional_utxos`: the remaining available utxos to satisfy `target_amount` with their weight cost
    pub optional_utxos: Vec<WeightedUtxo>,
    /// - `fee_rate`: fee rate to use
    pub fee_rate: FeeRate,
    /// - `target_amount`: the outgoing amount and the fees already accumulated from adding outputs and transaction’s header.
    pub target_amount: Amount,
    /// - `drain_script`: the script to use in case of change
    pub drain_script: &'a Script,
    /// - `rand`: random number generated used by some coin selection algorithms such as [`SingleRandomDraw`]
    pub rand: &'a mut R,
    /// - `avoid_partial_spends`: if true, the algorithm should try to avoid partial spends
    pub avoid_partial_spends: bool,
}

/// Trait for generalized coin selection algorithms
///
/// This trait can be implemented to make the [`Wallet`](super::Wallet) use a customized coin
/// selection algorithm when it creates transactions.
///
/// For an example see [this module](crate::wallet::coin_selection)'s documentation.
pub trait CoinSelectionAlgorithm: core::fmt::Debug {
    /// Perform the coin selection
    fn coin_select<R: RngCore>(
        &self,
        params: CoinSelectionParams<'_, R>,
    ) -> Result<CoinSelectionResult, InsufficientFunds>;
}

// See https://github.com/bitcoin/bitcoin/pull/18418/files
// https://bitcoincore.reviews/17824.html#l-339
const OUTPUT_GROUP_MAX_ENTRIES: usize = 100;

/// Group weighted UTXOs based on their script_pubkey if partial spends should be avoided.
///
/// If avoid_partial_spends is false each UTXO is kept in its own group.
/// If true, UTXOs sharing the same script_pubkey are grouped together, and if a group
/// would exceed OUTPUT_GROUP_MAX_ENTRIES the group is split into chunks.
fn group_utxos_if_applies(
    utxos: Vec<WeightedUtxo>,
    avoid_partial_spends: bool,
) -> Vec<Vec<WeightedUtxo>> {
    if !avoid_partial_spends {
        // No grouping: every UTXO is its own group.
        return utxos.into_iter().map(|u| vec![u]).collect();
    }

    // Group UTXOs by their scriptPubKey bytes.
    let mut groups_by_spk: HashMap<Vec<u8>, Vec<WeightedUtxo>> = HashMap::new();
    for weighted_utxo in utxos {
        let spk = weighted_utxo.utxo.txout().script_pubkey.as_bytes().to_vec();
        groups_by_spk.entry(spk).or_default().push(weighted_utxo);
    }
    // For each group, split into multiple groups if needed.
    let mut final_groups = Vec::new();
    for (_spk, group) in groups_by_spk {
        if group.len() > OUTPUT_GROUP_MAX_ENTRIES {
            for chunk in group.chunks(OUTPUT_GROUP_MAX_ENTRIES) {
                final_groups.push(chunk.to_vec());
            }
        } else {
            final_groups.push(group);
        }
    }
    final_groups
}

/// Simple and dumb coin selection
///
/// This coin selection algorithm sorts the available UTXOs by value and then picks them starting
/// from the largest ones until the required amount is reached.
#[derive(Debug, Default, Clone, Copy)]
pub struct LargestFirstCoinSelection;

impl CoinSelectionAlgorithm for LargestFirstCoinSelection {
    fn coin_select<R: RngCore>(
        &self,
        params: CoinSelectionParams<'_, R>,
    ) -> Result<CoinSelectionResult, InsufficientFunds> {
        let CoinSelectionParams {
            required_utxos,
            optional_utxos,
            fee_rate,
            target_amount,
            drain_script,
            rand: _,
            avoid_partial_spends,
        } = params;
        let required_utxo_group =
            group_utxos_if_applies(required_utxos.clone(), avoid_partial_spends);
        let mut optional_utxos_group = group_utxos_if_applies(optional_utxos, avoid_partial_spends);
        // We put the "required UTXOs" first and make sure the optional UTXOs are sorted,
        // initially smallest to largest, before being reversed with `.rev()`.
        let utxos = {
            optional_utxos_group.sort_unstable_by_key(|group| {
                group.iter().map(|wu| wu.utxo.txout().value).sum::<Amount>()
            });
            required_utxo_group
                .into_iter()
                .map(|utxo| (true, utxo))
                .chain(
                    optional_utxos_group
                        .into_iter()
                        .rev()
                        .map(|utxo| (false, utxo)),
                )
        };

        select_sorted_utxos(utxos, fee_rate, target_amount, drain_script)
    }
}

/// OldestFirstCoinSelection always picks the utxo with the smallest blockheight to add to the selected coins next
///
/// This coin selection algorithm sorts the available UTXOs by blockheight and then picks them starting
/// from the oldest ones until the required amount is reached.
#[derive(Debug, Default, Clone, Copy)]
pub struct OldestFirstCoinSelection;

impl CoinSelectionAlgorithm for OldestFirstCoinSelection {
    fn coin_select<R: RngCore>(
        &self,
        params: CoinSelectionParams<'_, R>,
    ) -> Result<CoinSelectionResult, InsufficientFunds> {
        let CoinSelectionParams {
            required_utxos,
            optional_utxos,
            fee_rate,
            target_amount,
            drain_script,
            rand: _,
            avoid_partial_spends,
        } = params;
        let required_utxo_group =
            group_utxos_if_applies(required_utxos.clone(), avoid_partial_spends);
        let mut optional_utxos_group =
            group_utxos_if_applies(optional_utxos.clone(), avoid_partial_spends);
        // We put the "required UTXOs" first and make sure the optional UTXOs are sorted from
        // oldest to newest according to blocktime
        // For utxo that doesn't exist in DB, they will have lowest priority to be selected
        let utxos = {
            optional_utxos_group.sort_unstable_by_key(|group| match group[0].utxo {
                Utxo::Local(ref local) => Some(local.chain_position),
                Utxo::Foreign { .. } => None,
            });

            required_utxo_group
                .into_iter()
                .map(|utxo| (true, utxo))
                .chain(optional_utxos_group.into_iter().map(|utxo| (false, utxo)))
        };

        select_sorted_utxos(utxos, fee_rate, target_amount, drain_script)
    }
}

/// Decide if change can be created
///
/// - `remaining_amount`: the amount in which the selected coins exceed the target amount
/// - `fee_rate`: required fee rate for the current selection
/// - `drain_script`: script to consider change creation
pub fn decide_change(remaining_amount: Amount, fee_rate: FeeRate, drain_script: &Script) -> Excess {
    // drain_output_len = size(len(script_pubkey)) + len(script_pubkey) + size(output_value)
    let drain_output_len = serialize(drain_script).len() + 8usize;
    let change_fee =
        fee_rate * Weight::from_vb(drain_output_len as u64).expect("overflow occurred");
    let drain_val = remaining_amount.checked_sub(change_fee).unwrap_or_default();

    if drain_val.is_dust(drain_script) {
        let dust_threshold = drain_script.minimal_non_dust();
        Excess::NoChange {
            dust_threshold,
            change_fee,
            remaining_amount,
        }
    } else {
        Excess::Change {
            amount: drain_val,
            fee: change_fee,
        }
    }
}

fn select_sorted_utxos(
    utxos: impl Iterator<Item = (bool, Vec<WeightedUtxo>)>,
    fee_rate: FeeRate,
    target_amount: Amount,
    drain_script: &Script,
) -> Result<CoinSelectionResult, InsufficientFunds> {
    let mut selected_amount = Amount::ZERO;
    let mut fee_amount = Amount::ZERO;
    let selected = utxos
        .scan(
            (&mut selected_amount, &mut fee_amount),
            |(selected_amount, fee_amount), (must_use, group)| {
                if must_use || **selected_amount < target_amount + **fee_amount {
                    for weighted_utxo in &group {
                        **fee_amount += fee_rate
                            * TxIn::default()
                                .segwit_weight()
                                .checked_add(weighted_utxo.satisfaction_weight)
                                .expect("`Weight` addition should not cause an integer overflow");
                        **selected_amount += weighted_utxo.utxo.txout().value;
                    }
                    Some(group.into_iter().map(|wu| wu.utxo).collect::<Vec<_>>())
                } else {
                    None
                }
            },
        )
        .flatten()
        .collect::<Vec<_>>();

    let amount_needed_with_fees = target_amount + fee_amount;
    if selected_amount < amount_needed_with_fees {
        return Err(InsufficientFunds {
            needed: amount_needed_with_fees,
            available: selected_amount,
        });
    }

    let remaining_amount = selected_amount - amount_needed_with_fees;

    let excess = decide_change(remaining_amount, fee_rate, drain_script);

    Ok(CoinSelectionResult {
        selected,
        fee_amount,
        excess,
    })
}

#[derive(Debug, Clone)]
// Adds fee information to an UTXO.
struct OutputGroup {
    weighted_utxo: WeightedUtxo,
    // Amount of fees for spending a certain utxo, calculated using a certain FeeRate
    fee: Amount,
    // The effective value of the UTXO, i.e., the utxo value minus the fee for spending it
    effective_value: SignedAmount,
}

impl OutputGroup {
    fn new(weighted_utxo: WeightedUtxo, fee_rate: FeeRate) -> Self {
        let fee = fee_rate
            * TxIn::default()
                .segwit_weight()
                .checked_add(weighted_utxo.satisfaction_weight)
                .expect("`Weight` addition should not cause an integer overflow");
        let effective_value = weighted_utxo
            .utxo
            .txout()
            .value
            .to_signed()
            .expect("signed amount")
            - fee.to_signed().expect("signed amount");
        OutputGroup {
            weighted_utxo,
            fee,
            effective_value,
        }
    }
}

/// Branch and bound coin selection
///
/// Code adapted from Bitcoin Core's implementation and from Mark Erhardt Master's Thesis: <http://murch.one/wp-content/uploads/2016/11/erhardt2016coinselection.pdf>
#[derive(Debug, Clone)]
pub struct BranchAndBoundCoinSelection<Cs = SingleRandomDraw> {
    size_of_change: u64,
    fallback_algorithm: Cs,
}

/// Error returned by branch and bound coin selection.
#[derive(Debug)]
enum BnbError {
    /// Branch and bound coin selection tries to avoid needing a change by finding the right inputs for
    /// the desired outputs plus fee, if there is not such combination this error is thrown
    NoExactMatch,
    /// Branch and bound coin selection possible attempts with sufficiently big UTXO set could grow
    /// exponentially, thus a limit is set, and when hit, this error is thrown
    TotalTriesExceeded,
}

impl<Cs: Default> Default for BranchAndBoundCoinSelection<Cs> {
    fn default() -> Self {
        Self {
            // P2WPKH cost of change -> value (8 bytes) + script len (1 bytes) + script (22 bytes)
            size_of_change: 8 + 1 + 22,
            fallback_algorithm: Cs::default(),
        }
    }
}

impl<Cs> BranchAndBoundCoinSelection<Cs> {
    /// Create new instance with a target `size_of_change` and `fallback_algorithm`.
    pub fn new(size_of_change: u64, fallback_algorithm: Cs) -> Self {
        Self {
            size_of_change,
            fallback_algorithm,
        }
    }
}

const BNB_TOTAL_TRIES: usize = 100_000;

impl<Cs: CoinSelectionAlgorithm> CoinSelectionAlgorithm for BranchAndBoundCoinSelection<Cs> {
    fn coin_select<R: RngCore>(
        &self,
        params: CoinSelectionParams<'_, R>,
    ) -> Result<CoinSelectionResult, InsufficientFunds> {
        let CoinSelectionParams {
            required_utxos,
            optional_utxos,
            fee_rate,
            target_amount,
            drain_script,
            rand: _,
            avoid_partial_spends,
        } = params;
        let required_utxo_group =
            group_utxos_if_applies(required_utxos.clone(), avoid_partial_spends);
        let optional_utxos_group =
            group_utxos_if_applies(optional_utxos.clone(), avoid_partial_spends);
        // Mapping every (UTXO, usize) to an output group
        let required_ogs: Vec<Vec<OutputGroup>> = required_utxo_group
            .into_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|weighted_utxo| OutputGroup::new(weighted_utxo, fee_rate))
                    .collect()
            })
            .collect();

        // Mapping every (UTXO, usize) to an output group, filtering UTXOs with a negative
        // effective value
        let optional_ogs: Vec<Vec<OutputGroup>> = optional_utxos_group
            .into_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|weighted_utxo| OutputGroup::new(weighted_utxo, fee_rate))
                    .filter(|og| og.effective_value.is_positive())
                    .collect()
            })
            .collect();

        let curr_value = required_ogs
            .iter()
            .flat_map(|group| group.iter())
            .fold(SignedAmount::ZERO, |acc, x| acc + x.effective_value);

        let curr_available_value = optional_ogs
            .iter()
            .flat_map(|group| group.iter())
            .fold(SignedAmount::ZERO, |acc, x| acc + x.effective_value);

        let cost_of_change = (Weight::from_vb(self.size_of_change).expect("overflow occurred")
            * fee_rate)
            .to_signed()
            .expect("signed amount");

        // `curr_value` and `curr_available_value` are both the sum of *effective_values* of
        // the UTXOs. For the optional UTXOs (curr_available_value) we filter out UTXOs with
        // negative effective value, so it will always be positive.
        //
        // Since we are required to spend the required UTXOs (curr_value) we have to consider
        // all their effective values, even when negative, which means that curr_value could
        // be negative as well.
        //
        // If the sum of curr_value and curr_available_value is negative or lower than our target,
        // we can immediately exit with an error, as it's guaranteed we will never find a solution
        // if we actually run the BnB.
        let total_value: Result<Amount, _> = (curr_available_value + curr_value).try_into();
        match total_value {
            Ok(v) if v >= target_amount => {}
            _ => {
                // Assume we spend all the UTXOs we can (all the required + all the optional with
                // positive effective value), sum their value and their fee cost.
                let (utxo_fees, utxo_value) = required_ogs.iter().chain(optional_ogs.iter()).fold(
                    (Amount::ZERO, Amount::ZERO),
                    |(mut fees, mut value), group| {
                        for utxo in group {
                            fees += utxo.fee;
                            value += utxo.weighted_utxo.utxo.txout().value;
                        }
                        (fees, value)
                    },
                );

                // Add to the target the fee cost of the UTXOs
                return Err(InsufficientFunds {
                    needed: target_amount + utxo_fees,
                    available: utxo_value,
                });
            }
        }

        let signed_target_amount = target_amount
            .try_into()
            .expect("Bitcoin amount to fit into i64");

        if curr_value > signed_target_amount {
            // remaining_amount can't be negative as that would mean the
            // selection wasn't successful
            // target_amount = amount_needed + (fee_amount - vin_fees)
            let remaining_amount = (curr_value - signed_target_amount)
                .to_unsigned()
                .expect("remaining amount can't be negative");

            let excess = decide_change(remaining_amount, fee_rate, drain_script);

            return Ok(calculate_cs_result(vec![], required_ogs, excess));
        }

        match self.bnb(
            required_ogs,
            optional_ogs,
            curr_value,
            curr_available_value,
            signed_target_amount,
            cost_of_change,
            drain_script,
            fee_rate,
        ) {
            Ok(r) => Ok(r),
            Err(_) => {
                let params = CoinSelectionParams {
                    required_utxos,
                    optional_utxos,
                    fee_rate,
                    target_amount,
                    drain_script,
                    rand: params.rand,
                    avoid_partial_spends,
                };
                self.fallback_algorithm.coin_select(params)
            }
        }
    }
}

impl<Cs> BranchAndBoundCoinSelection<Cs> {
    // TODO: make this more Rust-onic :)
    // (And perhaps refactor with less arguments?)
    #[allow(clippy::too_many_arguments)]
    fn bnb(
        &self,
        required_utxos: Vec<Vec<OutputGroup>>,
        mut optional_utxos: Vec<Vec<OutputGroup>>,
        mut curr_value: SignedAmount,
        mut curr_available_value: SignedAmount,
        target_amount: SignedAmount,
        cost_of_change: SignedAmount,
        drain_script: &Script,
        fee_rate: FeeRate,
    ) -> Result<CoinSelectionResult, BnbError> {
        // current_selection[i] will contain true if we are using optional_utxos[i],
        // false otherwise. Note that current_selection.len() could be less than
        // optional_utxos.len(), it just means that we still haven't decided if we should keep
        // certain optional_utxos or not.
        let mut current_selection: Vec<bool> = Vec::with_capacity(optional_utxos.len());

        // Sort the utxo_pool
        optional_utxos.sort_unstable_by_key(|group| {
            group
                .iter()
                .map(|og| og.effective_value)
                .sum::<SignedAmount>()
        });
        optional_utxos.reverse();

        // Contains the best selection we found
        let mut best_selection = Vec::new();
        let mut best_selection_value = None;

        // Depth First search loop for choosing the UTXOs
        for _ in 0..BNB_TOTAL_TRIES {
            // Conditions for starting a backtrack
            let mut backtrack = false;
            // Cannot possibly reach target with the amount remaining in the curr_available_value,
            // or the selected value is out of range.
            // Go back and try other branch
            if curr_value + curr_available_value < target_amount
                || curr_value > target_amount + cost_of_change
            {
                backtrack = true;
            } else if curr_value >= target_amount {
                // Selected value is within range, there's no point in going forward. Start
                // backtracking
                backtrack = true;

                // If we found a solution better than the previous one, or if there wasn't previous
                // solution, update the best solution
                if best_selection_value.is_none() || curr_value < best_selection_value.unwrap() {
                    best_selection.clone_from(&current_selection);
                    best_selection_value = Some(curr_value);
                }

                // If we found a perfect match, break here
                if curr_value == target_amount {
                    break;
                }
            }

            // Backtracking, moving backwards
            if backtrack {
                // Walk backwards to find the last included UTXO that still needs to have its omission branch traversed.
                while let Some(false) = current_selection.last() {
                    current_selection.pop();
                    curr_available_value += optional_utxos[current_selection.len()]
                        .iter()
                        .map(|og| og.effective_value)
                        .sum::<SignedAmount>();
                }

                if current_selection.last_mut().is_none() {
                    // We have walked back to the first utxo and no branch is untraversed. All solutions searched
                    // If best selection is empty, then there's no exact match
                    if best_selection.is_empty() {
                        return Err(BnbError::NoExactMatch);
                    }
                    break;
                }

                if let Some(c) = current_selection.last_mut() {
                    // Output was included on previous iterations, try excluding now.
                    *c = false;
                }

                let utxo = &optional_utxos[current_selection.len() - 1];
                curr_value -= utxo
                    .iter()
                    .map(|og| og.effective_value)
                    .sum::<SignedAmount>();
            } else {
                // Moving forwards, continuing down this branch
                let utxo = &optional_utxos[current_selection.len()];

                // Remove this utxo from the curr_available_value utxo amount
                curr_available_value -= utxo
                    .iter()
                    .map(|og| og.effective_value)
                    .sum::<SignedAmount>();

                // Inclusion branch first (Largest First Exploration)
                current_selection.push(true);
                curr_value += utxo
                    .iter()
                    .map(|og| og.effective_value)
                    .sum::<SignedAmount>();
            }
        }

        // Check for solution
        if best_selection.is_empty() {
            return Err(BnbError::TotalTriesExceeded);
        }

        // Set output set
        let selected_utxos = optional_utxos
            .into_iter()
            .zip(best_selection)
            .filter_map(|(optional, is_in_best)| if is_in_best { Some(optional) } else { None })
            .collect::<Vec<Vec<OutputGroup>>>();

        let selected_amount = best_selection_value.unwrap();

        // remaining_amount can't be negative as that would mean the
        // selection wasn't successful
        // target_amount = amount_needed + (fee_amount - vin_fees)
        let remaining_amount = (selected_amount - target_amount)
            .to_unsigned()
            .expect("valid unsigned");

        let excess = decide_change(remaining_amount, fee_rate, drain_script);

        Ok(calculate_cs_result(selected_utxos, required_utxos, excess))
    }
}

/// Pull UTXOs at random until we have enough to meet the target.
#[derive(Debug, Clone, Copy, Default)]
pub struct SingleRandomDraw;

impl CoinSelectionAlgorithm for SingleRandomDraw {
    fn coin_select<R: RngCore>(
        &self,
        params: CoinSelectionParams<'_, R>,
    ) -> Result<CoinSelectionResult, InsufficientFunds> {
        let CoinSelectionParams {
            required_utxos,
            optional_utxos,
            fee_rate,
            target_amount,
            drain_script,
            rand,
            avoid_partial_spends,
        } = params;
        let required_utxo_group = group_utxos_if_applies(required_utxos, avoid_partial_spends);
        let mut optional_utxos_group = group_utxos_if_applies(optional_utxos, avoid_partial_spends);
        // We put the required UTXOs first and then the randomize optional UTXOs to take as needed
        let utxos = {
            shuffle_slice(&mut optional_utxos_group, rand);

            required_utxo_group
                .into_iter()
                .map(|utxo| (true, utxo))
                .chain(optional_utxos_group.into_iter().map(|utxo| (false, utxo)))
        };

        // select required UTXOs and then random optional UTXOs.
        select_sorted_utxos(utxos, fee_rate, target_amount, drain_script)
    }
}

fn calculate_cs_result(
    mut selected_utxos: Vec<Vec<OutputGroup>>,
    mut required_utxos: Vec<Vec<OutputGroup>>,
    excess: Excess,
) -> CoinSelectionResult {
    selected_utxos.append(&mut required_utxos);
    let fee_amount = selected_utxos
        .iter()
        .flat_map(|group| group.iter())
        .map(|u| u.fee)
        .sum();
    let selected = selected_utxos
        .into_iter()
        .flatten()
        .map(|og| og.weighted_utxo.utxo)
        .collect::<Vec<_>>();

    CoinSelectionResult {
        selected,
        fee_amount,
        excess,
    }
}

/// Remove duplicate UTXOs.
///
/// If a UTXO appears in both `required` and `optional`, the appearance in `required` is kept.
pub(crate) fn filter_duplicates<I>(required: I, optional: I) -> (I, I)
where
    I: IntoIterator<Item = WeightedUtxo> + FromIterator<WeightedUtxo>,
{
    let mut visited = HashSet::<OutPoint>::new();
    let required = required
        .into_iter()
        .filter(|utxo| visited.insert(utxo.utxo.outpoint()))
        .collect::<I>();
    let optional = optional
        .into_iter()
        .filter(|utxo| visited.insert(utxo.utxo.outpoint()))
        .collect::<I>();
    (required, optional)
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use bitcoin::hashes::Hash;
    use chain::{BlockId, ChainPosition, ConfirmationBlockTime};
    use core::str::FromStr;
    use rand::rngs::StdRng;

    use bitcoin::{Amount, BlockHash, ScriptBuf, TxIn, TxOut};

    use super::*;
    use crate::types::*;
    use crate::wallet::coin_selection::filter_duplicates;

    use rand::prelude::SliceRandom;
    use rand::{thread_rng, Rng, RngCore, SeedableRng};

    // signature len (1WU) + signature and sighash (72WU)
    // + pubkey len (1WU) + pubkey (33WU)
    const P2WPKH_SATISFACTION_SIZE: usize = 1 + 72 + 1 + 33;

    const FEE_AMOUNT: Amount = Amount::from_sat(50);

    const DO_NOT_AVOID_PARTIAL_SPENDS: bool = false;

    fn unconfirmed_utxo(value: Amount, index: u32, last_seen: u64) -> WeightedUtxo {
        utxo(
            value,
            index,
            ChainPosition::Unconfirmed {
                last_seen: Some(last_seen),
            },
        )
    }

    fn confirmed_utxo(
        value: Amount,
        index: u32,
        confirmation_height: u32,
        confirmation_time: u64,
    ) -> WeightedUtxo {
        utxo(
            value,
            index,
            ChainPosition::Confirmed {
                anchor: ConfirmationBlockTime {
                    block_id: chain::BlockId {
                        height: confirmation_height,
                        hash: bitcoin::BlockHash::all_zeros(),
                    },
                    confirmation_time,
                },
                transitively: None,
            },
        )
    }

    fn utxo(
        value: Amount,
        index: u32,
        chain_position: ChainPosition<ConfirmationBlockTime>,
    ) -> WeightedUtxo {
        assert!(index < 10);
        let outpoint = OutPoint::from_str(&format!(
            "000000000000000000000000000000000000000000000000000000000000000{}:0",
            index
        ))
        .unwrap();
        WeightedUtxo {
            satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
            utxo: Utxo::Local(LocalOutput {
                outpoint,
                txout: TxOut {
                    value,
                    script_pubkey: ScriptBuf::new(),
                },
                keychain: KeychainKind::External,
                is_spent: false,
                derivation_index: 42,
                chain_position,
            }),
        }
    }

    fn get_test_utxos() -> Vec<WeightedUtxo> {
        vec![
            unconfirmed_utxo(Amount::from_sat(100_000), 0, 0),
            unconfirmed_utxo(FEE_AMOUNT - Amount::from_sat(40), 1, 0),
            unconfirmed_utxo(Amount::from_sat(200_000), 2, 0),
        ]
    }

    fn get_oldest_first_test_utxos() -> Vec<WeightedUtxo> {
        // ensure utxos are from different tx
        let utxo1 = confirmed_utxo(Amount::from_sat(120_000), 1, 1, 1231006505);
        let utxo2 = confirmed_utxo(Amount::from_sat(80_000), 2, 2, 1231006505);
        let utxo3 = confirmed_utxo(Amount::from_sat(300_000), 3, 3, 1231006505);
        vec![utxo1, utxo2, utxo3]
    }

    fn generate_random_utxos(rng: &mut StdRng, utxos_number: usize) -> Vec<WeightedUtxo> {
        let mut res = Vec::new();
        for i in 0..utxos_number {
            res.push(WeightedUtxo {
                satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::from_str(&format!(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:{}",
                        i
                    ))
                    .unwrap(),
                    txout: TxOut {
                        value: Amount::from_sat(rng.gen_range(0..200000000)),
                        script_pubkey: ScriptBuf::new(),
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: rng.next_u32(),
                    chain_position: if rng.gen_bool(0.5) {
                        ChainPosition::Confirmed {
                            anchor: ConfirmationBlockTime {
                                block_id: chain::BlockId {
                                    height: rng.next_u32(),
                                    hash: BlockHash::all_zeros(),
                                },
                                confirmation_time: rng.next_u64(),
                            },
                            transitively: None,
                        }
                    } else {
                        ChainPosition::Unconfirmed { last_seen: Some(0) }
                    },
                }),
            });
        }
        res
    }

    fn generate_same_value_utxos(utxos_value: Amount, utxos_number: usize) -> Vec<WeightedUtxo> {
        (0..utxos_number)
            .map(|i| WeightedUtxo {
                satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::from_str(&format!(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:{}",
                        i
                    ))
                    .unwrap(),
                    txout: TxOut {
                        value: utxos_value,
                        script_pubkey: ScriptBuf::new(),
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: 42,
                    chain_position: ChainPosition::Unconfirmed { last_seen: Some(0) },
                }),
            })
            .collect()
    }

    fn generate_utxos_with_same_address() -> Vec<WeightedUtxo> {
        // Two distinct scripts to simulate two addresses: A and B.
        let script_a = bitcoin::ScriptBuf::from(vec![b'A']);
        let script_b = bitcoin::ScriptBuf::from(vec![b'B']);

        vec![
            // 1.0 btc to A
            WeightedUtxo {
                satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::from_str(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:0",
                    )
                    .unwrap(),
                    txout: TxOut {
                        value: Amount::from_sat(1_000_000_000),
                        script_pubkey: script_a.clone(),
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: 42,
                    chain_position: ChainPosition::Unconfirmed { last_seen: Some(0) },
                }),
            },
            // 0.5 btc to A
            WeightedUtxo {
                satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::from_str(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:1",
                    )
                    .unwrap(),
                    txout: TxOut {
                        value: Amount::from_sat(500_000_000),
                        script_pubkey: script_a,
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: 42,
                    chain_position: ChainPosition::Unconfirmed { last_seen: Some(0) },
                }),
            },
            // 1.0 btc to B
            WeightedUtxo {
                satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::from_str(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:2",
                    )
                    .unwrap(),
                    txout: TxOut {
                        value: Amount::from_sat(1_000_000_000),
                        script_pubkey: script_b.clone(),
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: 42,
                    chain_position: ChainPosition::Unconfirmed { last_seen: Some(0) },
                }),
            },
            // 0.5 btc to B
            WeightedUtxo {
                satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::from_str(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:3",
                    )
                    .unwrap(),
                    txout: TxOut {
                        value: Amount::from_sat(500_000_000),
                        script_pubkey: script_b,
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: 42,
                    chain_position: ChainPosition::Unconfirmed { last_seen: Some(0) },
                }),
            },
        ]
    }

    fn sum_random_utxos(mut rng: &mut StdRng, utxos: &mut [WeightedUtxo]) -> Amount {
        let utxos_picked_len = rng.gen_range(2..utxos.len() / 2);
        utxos.shuffle(&mut rng);
        utxos[..utxos_picked_len]
            .iter()
            .map(|u| u.utxo.txout().value)
            .sum()
    }

    fn calc_target_amount(utxos: &[WeightedUtxo], fee_rate: FeeRate) -> Amount {
        utxos
            .iter()
            .cloned()
            .map(|utxo| OutputGroup::new(utxo, fee_rate).effective_value)
            .sum::<SignedAmount>()
            .to_unsigned()
            .expect("unsigned amount")
    }

    #[test]
    fn test_largest_first_coin_selection_success() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(250_000) + FEE_AMOUNT;
        let result = LargestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: utxos,
                optional_utxos: vec![],
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), Amount::from_sat(300_010));
        assert_eq!(result.fee_amount, Amount::from_sat(204));
    }

    #[test]
    fn test_largest_first_coin_selection_use_all() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(20_000) + FEE_AMOUNT;

        let result = LargestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: utxos,
                optional_utxos: vec![],
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), Amount::from_sat(300_010));
        assert_eq!(result.fee_amount, Amount::from_sat(204));
    }

    #[test]
    fn test_largest_first_coin_selection_use_only_necessary() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(20_000) + FEE_AMOUNT;

        let result = LargestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected_amount(), Amount::from_sat(200_000));
        assert_eq!(result.fee_amount, Amount::from_sat(68));
    }

    #[test]
    fn test_largest_first_coin_selection_insufficient_funds() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(500_000) + FEE_AMOUNT;

        let result = LargestFirstCoinSelection.coin_select(CoinSelectionParams {
            required_utxos: vec![],
            optional_utxos: utxos,
            fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
            target_amount,
            drain_script: &drain_script,
            rand: &mut thread_rng(),
            avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
        });
        assert!(matches!(result, Err(InsufficientFunds { .. })));
    }

    #[test]
    fn test_largest_first_coin_selection_insufficient_funds_high_fees() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(250_000) + FEE_AMOUNT;

        let result = LargestFirstCoinSelection.coin_select(CoinSelectionParams {
            required_utxos: vec![],
            optional_utxos: utxos,
            fee_rate: FeeRate::from_sat_per_vb_unchecked(1000),
            target_amount,
            drain_script: &drain_script,
            rand: &mut thread_rng(),
            avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
        });
        assert!(matches!(result, Err(InsufficientFunds { .. })));
    }

    #[test]
    fn test_oldest_first_coin_selection_success() {
        let utxos = get_oldest_first_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(180_000) + FEE_AMOUNT;

        let result = OldestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 2);
        assert_eq!(result.selected_amount(), Amount::from_sat(200_000));
        assert_eq!(result.fee_amount, Amount::from_sat(136));
    }

    #[test]
    fn test_oldest_first_coin_selection_use_all() {
        let utxos = get_oldest_first_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(20_000) + FEE_AMOUNT;

        let result = OldestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: utxos,
                optional_utxos: vec![],
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), Amount::from_sat(500_000));
        assert_eq!(result.fee_amount, Amount::from_sat(204));
    }

    #[test]
    fn test_oldest_first_coin_selection_use_only_necessary() {
        let utxos = get_oldest_first_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(20_000) + FEE_AMOUNT;

        let result = OldestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected_amount(), Amount::from_sat(120_000));
        assert_eq!(result.fee_amount, Amount::from_sat(68));
    }

    #[test]
    fn test_oldest_first_coin_selection_insufficient_funds() {
        let utxos = get_oldest_first_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(600_000) + FEE_AMOUNT;

        let result = OldestFirstCoinSelection.coin_select(CoinSelectionParams {
            required_utxos: vec![],
            optional_utxos: utxos,
            fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
            target_amount,
            drain_script: &drain_script,
            rand: &mut thread_rng(),
            avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
        });
        assert!(matches!(result, Err(InsufficientFunds { .. })));
    }

    #[test]
    fn test_oldest_first_coin_selection_insufficient_funds_high_fees() {
        let utxos = get_oldest_first_test_utxos();

        let target_amount =
            utxos.iter().map(|wu| wu.utxo.txout().value).sum::<Amount>() - Amount::from_sat(50);
        let drain_script = ScriptBuf::default();

        let result = OldestFirstCoinSelection.coin_select(CoinSelectionParams {
            required_utxos: vec![],
            optional_utxos: utxos,
            fee_rate: FeeRate::from_sat_per_vb_unchecked(1000),
            target_amount,
            drain_script: &drain_script,
            rand: &mut thread_rng(),
            avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
        });
        assert!(matches!(result, Err(InsufficientFunds { .. })));
    }

    #[test]
    fn test_bnb_coin_selection_success() {
        // In this case bnb won't find a suitable match and single random draw will
        // select three outputs
        let utxos = generate_same_value_utxos(Amount::from_sat(100_000), 20);
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(250_000) + FEE_AMOUNT;

        let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default()
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), Amount::from_sat(300_000));
        assert_eq!(result.fee_amount, Amount::from_sat(204));
    }

    #[test]
    fn test_bnb_coin_selection_required_are_enough() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(20_000) + FEE_AMOUNT;

        let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default()
            .coin_select(CoinSelectionParams {
                required_utxos: utxos.clone(),
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), Amount::from_sat(300_010));
        assert_eq!(result.fee_amount, Amount::from_sat(204));
    }

    #[test]
    fn test_bnb_coin_selection_optional_are_enough() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let fee_rate = FeeRate::BROADCAST_MIN;
        // first and third utxo's effective value
        let target_amount = calc_target_amount(&[utxos[0].clone(), utxos[2].clone()], fee_rate);

        let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default()
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate,
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 2);
        assert_eq!(result.selected_amount(), Amount::from_sat(300000));
        assert_eq!(result.fee_amount, Amount::from_sat(136));
    }

    #[test]
    fn test_single_random_draw_function_success() {
        let seed = [0; 32];
        let mut rng: StdRng = SeedableRng::from_seed(seed);
        let mut utxos = generate_random_utxos(&mut rng, 300);
        let target_amount = sum_random_utxos(&mut rng, &mut utxos) + FEE_AMOUNT;
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(1);
        let drain_script = ScriptBuf::default();

        let result = SingleRandomDraw.coin_select(CoinSelectionParams {
            required_utxos: vec![],
            optional_utxos: utxos,
            fee_rate,
            target_amount,
            drain_script: &drain_script,
            rand: &mut thread_rng(),
            avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
        });

        assert!(
            matches!(result, Ok(CoinSelectionResult {selected, fee_amount, ..})
                if selected.iter().map(|u| u.txout().value).sum::<Amount>() > target_amount
                && fee_amount == Amount::from_sat(selected.len() as u64 * 68)
            )
        );
    }

    #[test]
    fn test_single_random_draw_function_error() {
        let seed = [0; 32];
        let mut rng: StdRng = SeedableRng::from_seed(seed);

        // 100_000, 10, 200_000
        let utxos = get_test_utxos();
        let target_amount = Amount::from_sat(300_000) + FEE_AMOUNT;
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(1);
        let drain_script = ScriptBuf::default();

        let result = SingleRandomDraw.coin_select(CoinSelectionParams {
            required_utxos: vec![],
            optional_utxos: utxos,
            fee_rate,
            target_amount,
            drain_script: &drain_script,
            rand: &mut rng,
            avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
        });

        assert!(matches!(result, Err(InsufficientFunds {needed, available})
                if needed == Amount::from_sat(300_254) && available == Amount::from_sat(300_010)));
    }

    #[test]
    fn test_bnb_coin_selection_required_not_enough() {
        let utxos = get_test_utxos();

        let required = vec![utxos[0].clone()];
        let mut optional = utxos[1..].to_vec();
        optional.push(utxo(
            Amount::from_sat(500_000),
            3,
            ChainPosition::<ConfirmationBlockTime>::Unconfirmed { last_seen: Some(0) },
        ));

        // Defensive assertions, for sanity and in case someone changes the test utxos vector.
        let amount = required
            .iter()
            .map(|u| u.utxo.txout().value)
            .sum::<Amount>();
        assert_eq!(amount, Amount::from_sat(100_000));
        let amount = optional
            .iter()
            .map(|u| u.utxo.txout().value)
            .sum::<Amount>();
        assert!(amount > Amount::from_sat(150_000));
        let drain_script = ScriptBuf::default();

        let fee_rate = FeeRate::BROADCAST_MIN;
        // first and third utxo's effective value
        let target_amount = calc_target_amount(&[utxos[0].clone(), utxos[2].clone()], fee_rate);

        let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default()
            .coin_select(CoinSelectionParams {
                required_utxos: required,
                optional_utxos: optional,
                fee_rate,
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 2);
        assert_eq!(result.selected_amount(), Amount::from_sat(300_000));
        assert_eq!(result.fee_amount, Amount::from_sat(136));
    }

    #[test]
    fn test_bnb_coin_selection_insufficient_funds() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(500_000) + FEE_AMOUNT;

        let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
            CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            },
        );

        assert!(matches!(result, Err(InsufficientFunds { .. })));
    }

    #[test]
    fn test_bnb_coin_selection_insufficient_funds_high_fees() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let target_amount = Amount::from_sat(250_000) + FEE_AMOUNT;

        let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
            CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(1000),
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            },
        );
        assert!(matches!(result, Err(InsufficientFunds { .. })));
    }

    #[test]
    fn test_bnb_coin_selection_check_fee_rate() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();
        let fee_rate = FeeRate::BROADCAST_MIN;
        // first utxo's effective value
        let target_amount = calc_target_amount(&utxos[0..1], fee_rate);

        let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default()
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate,
                target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();

        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected_amount(), Amount::from_sat(100_000));
        let input_weight =
            TxIn::default().segwit_weight().to_wu() + P2WPKH_SATISFACTION_SIZE as u64;
        // the final fee rate should be exactly the same as the fee rate given
        let result_feerate = result.fee_amount / Weight::from_wu(input_weight);
        assert_eq!(result_feerate, fee_rate);
    }

    #[test]
    fn test_bnb_coin_selection_exact_match() {
        let seed = [0; 32];
        let mut rng: StdRng = SeedableRng::from_seed(seed);

        for _i in 0..200 {
            let mut optional_utxos = generate_random_utxos(&mut rng, 16);
            let target_amount = sum_random_utxos(&mut rng, &mut optional_utxos);
            let drain_script = ScriptBuf::default();
            let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default()
                .coin_select(CoinSelectionParams {
                    required_utxos: vec![],
                    optional_utxos: optional_utxos,
                    fee_rate: FeeRate::ZERO,
                    target_amount,
                    drain_script: &drain_script,
                    rand: &mut thread_rng(),
                    avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
                })
                .unwrap();
            assert_eq!(result.selected_amount(), target_amount);
        }
    }

    #[test]
    fn test_bnb_function_no_exact_match() {
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(10);
        let utxos: Vec<OutputGroup> = get_test_utxos()
            .into_iter()
            .map(|u| OutputGroup::new(u, fee_rate))
            .collect();

        let curr_available_value = utxos
            .iter()
            .fold(SignedAmount::ZERO, |acc, x| acc + x.effective_value);

        let size_of_change = 31;
        let cost_of_change = (Weight::from_vb_unchecked(size_of_change) * fee_rate)
            .to_signed()
            .unwrap();

        let drain_script = ScriptBuf::default();
        let target_amount = SignedAmount::from_sat(20_000) + FEE_AMOUNT.to_signed().unwrap();
        let result = BranchAndBoundCoinSelection::new(size_of_change, SingleRandomDraw).bnb(
            vec![],
            utxos.into_iter().map(|u| vec![u]).collect(),
            SignedAmount::ZERO,
            curr_available_value,
            target_amount,
            cost_of_change,
            &drain_script,
            fee_rate,
        );
        assert!(matches!(result, Err(BnbError::NoExactMatch)));
    }

    #[test]
    fn test_bnb_function_tries_exceeded() {
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(10);
        let utxos: Vec<OutputGroup> = generate_same_value_utxos(Amount::from_sat(100_000), 100_000)
            .into_iter()
            .map(|u| OutputGroup::new(u, fee_rate))
            .collect();

        let curr_available_value = utxos
            .iter()
            .fold(SignedAmount::ZERO, |acc, x| acc + x.effective_value);

        let size_of_change = 31;
        let cost_of_change = (Weight::from_vb_unchecked(size_of_change) * fee_rate)
            .to_signed()
            .unwrap();
        let target_amount = SignedAmount::from_sat(20_000) + FEE_AMOUNT.to_signed().unwrap();

        let drain_script = ScriptBuf::default();

        let result = BranchAndBoundCoinSelection::new(size_of_change, SingleRandomDraw).bnb(
            vec![],
            utxos.into_iter().map(|u| vec![u]).collect(),
            SignedAmount::ZERO,
            curr_available_value,
            target_amount,
            cost_of_change,
            &drain_script,
            fee_rate,
        );
        assert!(matches!(result, Err(BnbError::TotalTriesExceeded)));
    }

    // The match won't be exact but still in the range
    #[test]
    fn test_bnb_function_almost_exact_match_with_fees() {
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(1);
        let size_of_change = 31;
        let cost_of_change = (Weight::from_vb_unchecked(size_of_change) * fee_rate)
            .to_signed()
            .unwrap();

        let utxos: Vec<_> = generate_same_value_utxos(Amount::from_sat(50_000), 10)
            .into_iter()
            .map(|u| OutputGroup::new(u, fee_rate))
            .collect();

        let curr_value = SignedAmount::ZERO;

        let curr_available_value = utxos
            .iter()
            .fold(SignedAmount::ZERO, |acc, x| acc + x.effective_value);

        // 2*(value of 1 utxo)  - 2*(1 utxo fees with 1.0sat/vbyte fee rate) -
        // cost_of_change + 5.
        let target_amount = 2 * 50_000 - 2 * 67 - cost_of_change.to_sat() + 5;
        let target_amount = SignedAmount::from_sat(target_amount);

        let drain_script = ScriptBuf::default();

        let result = BranchAndBoundCoinSelection::new(size_of_change, SingleRandomDraw)
            .bnb(
                vec![],
                utxos.into_iter().map(|u| vec![u]).collect(),
                curr_value,
                curr_available_value,
                target_amount,
                cost_of_change,
                &drain_script,
                fee_rate,
            )
            .unwrap();
        assert_eq!(result.selected_amount(), Amount::from_sat(100_000));
        assert_eq!(result.fee_amount, Amount::from_sat(136));
    }

    // TODO: bnb() function should be optimized, and this test should be done with more utxos
    #[test]
    fn test_bnb_function_exact_match_more_utxos() {
        let seed = [0; 32];
        let mut rng: StdRng = SeedableRng::from_seed(seed);
        let fee_rate = FeeRate::ZERO;

        for _ in 0..200 {
            let optional_utxos: Vec<_> = generate_random_utxos(&mut rng, 40)
                .into_iter()
                .map(|u| OutputGroup::new(u, fee_rate))
                .collect();

            let curr_value = SignedAmount::ZERO;

            let curr_available_value = optional_utxos
                .iter()
                .fold(SignedAmount::ZERO, |acc, x| acc + x.effective_value);

            let target_amount =
                optional_utxos[3].effective_value + optional_utxos[23].effective_value;

            let drain_script = ScriptBuf::default();

            let result = BranchAndBoundCoinSelection::<SingleRandomDraw>::default()
                .bnb(
                    vec![],
                    optional_utxos.into_iter().map(|u| vec![u]).collect(),
                    curr_value,
                    curr_available_value,
                    target_amount,
                    SignedAmount::ZERO,
                    &drain_script,
                    fee_rate,
                )
                .unwrap();
            assert_eq!(
                result.selected_amount(),
                target_amount.to_unsigned().unwrap()
            );
        }
    }

    #[test]
    fn test_bnb_exclude_negative_effective_value() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();

        let selection = BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
            CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(10),
                target_amount: Amount::from_sat(500_000),
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            },
        );

        assert_matches!(
            selection,
            Err(InsufficientFunds {
                available,
                ..
            }) if available.to_sat() == 300_000
        );
    }

    #[test]
    fn test_bnb_include_negative_effective_value_when_required() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();

        let (required, optional) = utxos.into_iter().partition(
            |u| matches!(u, WeightedUtxo { utxo, .. } if utxo.txout().value.to_sat() < 1000),
        );

        let selection = BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
            CoinSelectionParams {
                required_utxos: required,
                optional_utxos: optional,
                fee_rate: FeeRate::from_sat_per_vb_unchecked(10),
                target_amount: Amount::from_sat(500_000),
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            },
        );

        assert_matches!(
            selection,
            Err(InsufficientFunds {
                available,
                ..
            }) if available.to_sat() == 300_010
        );
    }

    #[test]
    fn test_bnb_sum_of_effective_value_negative() {
        let utxos = get_test_utxos();
        let drain_script = ScriptBuf::default();

        let selection = BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
            CoinSelectionParams {
                required_utxos: utxos,
                optional_utxos: vec![],
                fee_rate: FeeRate::from_sat_per_vb_unchecked(10_000),
                target_amount: Amount::from_sat(500_000),
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            },
        );

        assert_matches!(
            selection,
            Err(InsufficientFunds {
                available,
                ..
            }) if available.to_sat() == 300_010
        );
    }

    #[test]
    fn test_bnb_fallback_algorithm() {
        // utxo value
        // 120k + 80k + 300k
        let optional_utxos = get_oldest_first_test_utxos();
        let feerate = FeeRate::BROADCAST_MIN;
        let target_amount = Amount::from_sat(190_000);
        let drain_script = ScriptBuf::new();
        // bnb won't find exact match and should select oldest first
        let bnb_with_oldest_first =
            BranchAndBoundCoinSelection::new(8 + 1 + 22, OldestFirstCoinSelection);
        let res = bnb_with_oldest_first
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: optional_utxos,
                fee_rate: feerate,
                target_amount: target_amount,
                drain_script: &drain_script,
                rand: &mut thread_rng(),
                avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
            })
            .unwrap();
        assert_eq!(res.selected_amount(), Amount::from_sat(200_000));
    }

    #[test]
    fn test_filter_duplicates() {
        fn utxo(txid: &str, value: u64) -> WeightedUtxo {
            WeightedUtxo {
                satisfaction_weight: Weight::ZERO,
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::new(bitcoin::hashes::Hash::hash(txid.as_bytes()), 0),
                    txout: TxOut {
                        value: Amount::from_sat(value),
                        script_pubkey: ScriptBuf::new(),
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: 0,
                    chain_position: ChainPosition::Confirmed {
                        anchor: ConfirmationBlockTime {
                            block_id: BlockId {
                                height: 12345,
                                hash: BlockHash::all_zeros(),
                            },
                            confirmation_time: 12345,
                        },
                        transitively: None,
                    },
                }),
            }
        }

        fn to_utxo_vec(utxos: &[(&str, u64)]) -> Vec<WeightedUtxo> {
            let mut v = utxos
                .iter()
                .map(|&(txid, value)| utxo(txid, value))
                .collect::<Vec<_>>();
            v.sort_by_key(|u| u.utxo.outpoint());
            v
        }

        struct TestCase<'a> {
            name: &'a str,
            required: &'a [(&'a str, u64)],
            optional: &'a [(&'a str, u64)],
            exp_required: &'a [(&'a str, u64)],
            exp_optional: &'a [(&'a str, u64)],
        }

        let test_cases = [
            TestCase {
                name: "no_duplicates",
                required: &[("A", 1000), ("B", 2100)],
                optional: &[("C", 1000)],
                exp_required: &[("A", 1000), ("B", 2100)],
                exp_optional: &[("C", 1000)],
            },
            TestCase {
                name: "duplicate_required_utxos",
                required: &[("A", 3000), ("B", 1200), ("C", 1234), ("A", 3000)],
                optional: &[("D", 2100)],
                exp_required: &[("A", 3000), ("B", 1200), ("C", 1234)],
                exp_optional: &[("D", 2100)],
            },
            TestCase {
                name: "duplicate_optional_utxos",
                required: &[("A", 3000), ("B", 1200)],
                optional: &[("C", 5000), ("D", 1300), ("C", 5000)],
                exp_required: &[("A", 3000), ("B", 1200)],
                exp_optional: &[("C", 5000), ("D", 1300)],
            },
            TestCase {
                name: "duplicate_across_required_and_optional_utxos",
                required: &[("A", 3000), ("B", 1200), ("C", 2100)],
                optional: &[("A", 3000), ("D", 1200), ("E", 5000)],
                exp_required: &[("A", 3000), ("B", 1200), ("C", 2100)],
                exp_optional: &[("D", 1200), ("E", 5000)],
            },
        ];

        for (i, t) in test_cases.into_iter().enumerate() {
            let (required, optional) =
                filter_duplicates(to_utxo_vec(t.required), to_utxo_vec(t.optional));
            assert_eq!(
                required,
                to_utxo_vec(t.exp_required),
                "[{}:{}] unexpected `required` result",
                i,
                t.name
            );
            assert_eq!(
                optional,
                to_utxo_vec(t.exp_optional),
                "[{}:{}] unexpected `optional` result",
                i,
                t.name
            );
        }
    }

    #[test]
    fn test_deterministic_coin_selection_picks_same_utxos() {
        enum CoinSelectionAlgo {
            BranchAndBound,
            OldestFirst,
            LargestFirst,
        }

        struct TestCase<'a> {
            name: &'a str,
            coin_selection_algo: CoinSelectionAlgo,
            exp_vouts: &'a [u32],
        }

        let test_cases = [
            TestCase {
                name: "branch and bound",
                coin_selection_algo: CoinSelectionAlgo::BranchAndBound,
                // note: we expect these to be sorted largest first, which indicates
                // BnB succeeded with no fallback
                exp_vouts: &[29, 28, 27],
            },
            TestCase {
                name: "oldest first",
                coin_selection_algo: CoinSelectionAlgo::OldestFirst,
                exp_vouts: &[0, 1, 2],
            },
            TestCase {
                name: "largest first",
                coin_selection_algo: CoinSelectionAlgo::LargestFirst,
                exp_vouts: &[29, 28, 27],
            },
        ];

        let optional = generate_same_value_utxos(Amount::from_sat(100_000), 30);
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(1);
        let target_amount = calc_target_amount(&optional[0..3], fee_rate);
        assert_eq!(target_amount, Amount::from_sat(299_796));
        let drain_script = ScriptBuf::default();

        for tc in test_cases {
            let optional = optional.clone();

            let result = match tc.coin_selection_algo {
                CoinSelectionAlgo::BranchAndBound => {
                    BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
                        CoinSelectionParams {
                            required_utxos: vec![],
                            optional_utxos: optional,
                            fee_rate,
                            target_amount,
                            drain_script: &drain_script,
                            rand: &mut thread_rng(),
                            avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
                        },
                    )
                }
                CoinSelectionAlgo::OldestFirst => {
                    OldestFirstCoinSelection.coin_select(CoinSelectionParams {
                        required_utxos: vec![],
                        optional_utxos: optional,
                        fee_rate,
                        target_amount,
                        drain_script: &drain_script,
                        rand: &mut thread_rng(),
                        avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
                    })
                }
                CoinSelectionAlgo::LargestFirst => {
                    LargestFirstCoinSelection.coin_select(CoinSelectionParams {
                        required_utxos: vec![],
                        optional_utxos: optional,
                        fee_rate,
                        target_amount,
                        drain_script: &drain_script,
                        rand: &mut thread_rng(),
                        avoid_partial_spends: DO_NOT_AVOID_PARTIAL_SPENDS,
                    })
                }
            };

            assert!(result.is_ok(), "coin_select failed {}", tc.name);
            let result = result.unwrap();
            assert!(matches!(result.excess, Excess::NoChange { .. },));
            assert_eq!(
                result.selected.len(),
                3,
                "wrong selected len for {}",
                tc.name
            );
            assert_eq!(
                result.selected_amount(),
                Amount::from_sat(300_000),
                "wrong selected amount for {}",
                tc.name
            );
            assert_eq!(
                result.fee_amount,
                Amount::from_sat(204),
                "wrong fee amount for {}",
                tc.name
            );
            let vouts = result
                .selected
                .iter()
                .map(|utxo| utxo.outpoint().vout)
                .collect::<Vec<u32>>();
            assert_eq!(vouts, tc.exp_vouts, "wrong selected vouts for {}", tc.name);
        }
    }

    #[test]
    fn test_group_utxos_if_applies_grouping() {
        // generate 4 utxos:
        // - Two for script A
        // - Two for script B
        let utxos = generate_utxos_with_same_address();

        // Grouping should combine utxos with the same script when avoiding partial spends.
        let groups = group_utxos_if_applies(utxos, true);

        // Since we have two distinct script_pubkeys we expect 2 groups.
        assert_eq!(
            groups.len(),
            2,
            "Expected 2 groups for 2 distinct addresses"
        );

        // Each group must have exactly two UTXOs.
        for group in groups {
            assert_eq!(group.len(), 2, "Each group should contain exactly 2 UTXOs");
            // Check that all UTXOs in the group share the same script_pubkey.
            let script = group[0].utxo.txout().script_pubkey.clone();
            for utxo in group.iter() {
                assert_eq!(utxo.utxo.txout().script_pubkey, script);
            }
        }
    }

    #[test]
    fn test_group_utxos_if_applies_max_entries() {
        // Create 101 UTXOs with the same script (address A)
        let script_a = bitcoin::ScriptBuf::from(vec![b'A']);
        let mut utxos = Vec::new();
        for i in 0..101 {
            utxos.push(WeightedUtxo {
                satisfaction_weight: Weight::from_wu_usize(P2WPKH_SATISFACTION_SIZE),
                utxo: Utxo::Local(LocalOutput {
                    outpoint: OutPoint::from_str(&format!(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:{}",
                        i
                    ))
                    .unwrap(),
                    txout: TxOut {
                        value: Amount::from_sat(1_000_000_000),
                        script_pubkey: script_a.clone(),
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                    derivation_index: 42,
                    chain_position: ChainPosition::Unconfirmed { last_seen: Some(0) },
                }),
            });
        }

        // Group UTXOs with avoid_partial_spends enabled.
        let groups = group_utxos_if_applies(utxos, true);

        // Since all UTXOs share the same script_pubkey and OUTPUT_GROUP_MAX_ENTRIES is 100,
        // they must be split into 2 groups: one with 100 utxos and one with 1.
        assert_eq!(
            groups.len(),
            2,
            "Expected 2 groups after splitting 101 UTXOs"
        );
        let sizes: Vec<usize> = groups.iter().map(|g| g.len()).collect();
        assert!(
            sizes.contains(&100),
            "One group should contain exactly 100 UTXOs"
        );
        assert!(
            sizes.contains(&1),
            "One group should contain exactly 1 UTXO"
        );
    }

    #[test]
    fn test_coin_selection_grouping_address_behavior() {
        // Scenario: our node has four outputs:
        //    • 1.0 btc to A
        //    • 0.5 btc to A
        //    • 1.0 btc to B
        //    • 0.5 btc to B
        //
        // The node sends 0.2 btc to C.
        //
        // Without avoid_partial_spends:
        //   • The algorithm considers each UTXO separately.
        //   • In our LargestFirstCoinSelection (which orders optional groups descending by total value)
        //     the highest‐value individual coin is chosen.
        //   • Here that is the 1.0 btc output.
        //
        // With avoid_partial_spends:
        //   • UTXOs sharing the same address are grouped.
        //   • One group (either all A’s or all B’s) is used, so both UTXOs from that address are selected.
        //
        // To eliminate fee effects we use a zero fee rate.
        let fee_rate = FeeRate::ZERO;
        // Set target low enough so that a single UTXO would suffice.
        let target = Amount::from_sat(200_000_000);
        // A dummy drain script (change output script)
        let drain_script = ScriptBuf::new();

        // Generate the four test UTXOs.
        let utxos = generate_utxos_with_same_address();

        // --- Case 1: Without avoid_partial_spends (grouping disabled)
        let mut rng = thread_rng();
        let res_no_group = LargestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],        // no required UTXOs
                optional_utxos: utxos.clone(), // all UTXOs as optional
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: false, // grouping disabled
            })
            .expect("coin selection should succeed without grouping");
        // Without grouping, the algorithm picks one UTXO—the one with the highest value.
        // In our ordering, the 1.0 btc output is chosen.
        assert_eq!(
            res_no_group.selected.len(),
            1,
            "expected 1 UTXO selected when not grouping"
        );
        let selected_no_group = res_no_group.selected_amount();
        // We expect the selected UTXO to have a value of 1.0 btc (1_000_000_000 sat).
        assert_eq!(
            selected_no_group,
            Amount::from_sat(1_000_000_000),
            "expected non-grouped selection to pick the 1.0 btc output"
        );

        // --- Case 2: With avoid_partial_spends enabled (grouping enabled)
        let res_group = LargestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: vec![], // no required UTXOs
                optional_utxos: utxos,  // all UTXOs as optional
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: true, // grouping enabled
            })
            .expect("coin selection should succeed with grouping");
        // With grouping enabled, each address is treated as a group.
        // For either address A or B, the group consists of both outputs:
        //    1.0 btc + 0.5 btc = 1.5 btc in total.
        // Thus we expect exactly 2 UTXOs to be selected.
        assert_eq!(
            res_group.selected.len(),
            2,
            "expected 2 UTXOs selected when grouping is enabled"
        );
        let selected_group = res_group.selected_amount();
        // The grouped selection should have a higher total (1.5 btc) than the non-grouped one.
        assert!(
            selected_group > selected_no_group,
            "expected grouped selection amount to be larger"
        );
        // Also check that both UTXOs in the grouped selection share the same script.
        let common_script = res_group.selected[0].txout().script_pubkey.clone();
        for utxo in res_group.selected.iter() {
            assert_eq!(
                utxo.txout().script_pubkey,
                common_script,
                "all UTXOs in a grouped selection must belong to the same address"
            );
        }
    }

    #[test]
    fn test_coin_selection_grouping_address_behavior_oldestfirst() {
        // Using OldestFirstCoinSelection.
        let fee_rate = FeeRate::ZERO;
        let target = Amount::from_sat(200_000_000); // low target so a single coin would suffice
        let drain_script = ScriptBuf::new();
        let utxos = generate_utxos_with_same_address();

        // Case 1: Grouping disabled.
        let mut rng = thread_rng();
        let res_no_group = OldestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],        // no required UTXOs
                optional_utxos: utxos.clone(), // all UTXOs as optional
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: false, // grouping disabled
            })
            .expect("coin selection should succeed without grouping (OldestFirst)");
        // Expect the highest-value individual coin is chosen (here 1.0 btc).
        assert_eq!(
            res_no_group.selected.len(),
            1,
            "expected 1 UTXO selected when not grouping (OldestFirst)"
        );
        assert_eq!(
            res_no_group.selected_amount(),
            Amount::from_sat(1_000_000_000),
            "expected non-grouped selection to pick the 1.0 btc output (OldestFirst)"
        );

        // Case 2: Grouping enabled.
        let res_group = OldestFirstCoinSelection
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: true, // grouping enabled
            })
            .expect("coin selection should succeed with grouping (OldestFirst)");
        // With grouping enabled, one group (either A’s or B’s) is used: both outputs (1.0+0.5).
        assert_eq!(
            res_group.selected.len(),
            2,
            "expected 2 UTXOs selected when grouping is enabled (OldestFirst)"
        );
        assert_eq!(
            res_group.selected_amount(),
            Amount::from_sat(1_500_000_000),
            "expected grouped selection to pick outputs totaling 1.5 btc (OldestFirst)"
        );
        let common_script = res_group.selected[0].txout().script_pubkey.clone();
        for utxo in res_group.selected.iter() {
            assert_eq!(
                utxo.txout().script_pubkey,
                common_script,
                "all UTXOs in grouped selection must belong to the same address (OldestFirst)"
            );
        }
    }

    #[test]
    fn test_coin_selection_grouping_address_behavior_branch_and_bound() {
        // Using BranchAndBoundCoinSelection with SingleRandomDraw as fallback.
        let fee_rate = FeeRate::ZERO;
        let target = Amount::from_sat(200_000_000);
        let drain_script = ScriptBuf::new();
        let utxos = generate_utxos_with_same_address();

        let mut rng = thread_rng();
        let bnb_algo = BranchAndBoundCoinSelection::<SingleRandomDraw>::default();

        // --- Case 1: Without avoid_partial_spends (grouping disabled)
        let res_no_group = bnb_algo
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos.clone(),
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: false, // grouping disabled
            })
            .expect("coin selection should succeed without grouping (BnB)");
        // Expect exactly one UTXO selected. However, due to the fallback randomness
        // the chosen coin's value could be either 1.0 btc or 0.5 btc.
        assert_eq!(
            res_no_group.selected.len(),
            1,
            "expected 1 UTXO selected when not grouping (BnB)"
        );
        let non_group_val = res_no_group.selected_amount();
        assert!(
            non_group_val == Amount::from_sat(1_000_000_000) || non_group_val == Amount::from_sat(500_000_000),
            "expected non-grouped selection in BnB to be either 1.0 btc (1_000_000_000 sat) or 0.5 btc (500_000_000 sat), got {}",
            non_group_val
        );

        // --- Case 2: With avoid_partial_spends enabled (grouping enabled)
        let res_group = bnb_algo
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],
                optional_utxos: utxos,
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: true, // grouping enabled
            })
            .expect("coin selection should succeed with grouping (BnB)");
        // With grouping, each address is treated as a group.
        // For either address A or B, the group consists of both outputs:
        //    1.0 btc + 0.5 btc = 1.5 btc in total.
        // Thus we expect exactly 2 UTXOs to be selected.
        assert_eq!(
            res_group.selected.len(),
            2,
            "expected 2 UTXOs selected when grouping is enabled (BnB)"
        );
        assert_eq!(
            res_group.selected_amount(),
            Amount::from_sat(1_500_000_000),
            "expected grouped selection to pick outputs totaling 1.5 btc (BnB)"
        );
        let common_script = res_group.selected[0].txout().script_pubkey.clone();
        for utxo in res_group.selected.iter() {
            assert_eq!(
                utxo.txout().script_pubkey,
                common_script,
                "all UTXOs in grouped selection must belong to the same address (BnB)"
            );
        }
    }
    #[test]
    fn test_coin_selection_grouping_address_behavior_single_random_draw() {
        // Using SingleRandomDraw algorithm.
        let fee_rate = FeeRate::ZERO;
        let target = Amount::from_sat(200_000_000);
        let drain_script = ScriptBuf::new();
        let utxos = generate_utxos_with_same_address();
        let mut rng = thread_rng();

        // --- Case 1: Without avoid_partial_spends (grouping disabled)
        let res_no_group = SingleRandomDraw
            .coin_select(CoinSelectionParams {
                required_utxos: vec![],        // no required UTXOs
                optional_utxos: utxos.clone(), // all UTXOs as optional
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: false, // grouping disabled
            })
            .expect("coin selection should succeed without grouping (RandomDraw)");
        // Expect that exactly one UTXO is picked.
        assert_eq!(
            res_no_group.selected.len(),
            1,
            "expected 1 UTXO selected when not grouping (RandomDraw)"
        );
        let sel_amt = res_no_group.selected_amount();
        // Since SingleRandomDraw selects randomly, it may pick either the 1.0 btc or
        // the 0.5 btc output. We allow both.
        assert!(
            sel_amt == Amount::from_sat(1_000_000_000) || sel_amt == Amount::from_sat(500_000_000),
            "expected non-grouped selection to pick either the 1.0 btc or 0.5 btc output, got {}",
            sel_amt
        );

        // --- Case 2: With avoid_partial_spends enabled (grouping enabled)
        let res_group = SingleRandomDraw
            .coin_select(CoinSelectionParams {
                required_utxos: vec![], // no required UTXOs
                optional_utxos: utxos,  // all UTXOs as optional
                fee_rate,
                target_amount: target,
                drain_script: &drain_script,
                rand: &mut rng,
                avoid_partial_spends: true, // grouping enabled
            })
            .expect("coin selection should succeed with grouping (RandomDraw)");
        // With grouping enabled, the algorithm should select both UTXOs from one address.
        assert_eq!(
            res_group.selected.len(),
            2,
            "expected 2 UTXOs selected when grouping is enabled (RandomDraw)"
        );
        assert_eq!(
            res_group.selected_amount(),
            Amount::from_sat(1_500_000_000),
            "expected grouped selection to pick outputs totaling 1.5 btc (RandomDraw)"
        );
        let common_script = res_group.selected[0].txout().script_pubkey.clone();
        for utxo in res_group.selected.iter() {
            assert_eq!(
                utxo.txout().script_pubkey,
                common_script,
                "all UTXOs in grouped selection must belong to the same address (RandomDraw)"
            );
        }
    }
}
