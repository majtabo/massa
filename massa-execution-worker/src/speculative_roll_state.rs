// Copyright (c) 2022 MASSA LABS <info@massa.net>

use massa_execution_exports::ExecutionError;
use massa_final_state::FinalState;
use massa_models::{
    constants::{default::POS_SELL_CYCLES, PERIODS_PER_CYCLE, ROLL_PRICE},
    prehash::Map,
    Address, Amount, Slot,
};
use massa_pos_exports::{PoSChanges, SelectorController};
use parking_lot::RwLock;
use std::sync::Arc;

use crate::active_history::ActiveHistory;

/// Speculative state of the rolls
#[allow(dead_code)]
pub(crate) struct SpeculativeRollState {
    /// Thread-safe shared access to the final state. For reading only.
    final_state: Arc<RwLock<FinalState>>,

    /// History of the outputs of recently executed slots.
    /// Slots should be consecutive, newest at the back.
    active_history: Arc<RwLock<ActiveHistory>>,

    /// Selector used to feed_cycle and get_selection
    selector: Box<dyn SelectorController>,

    /// List of changes to the state after settling roll sell/buy
    added_changes: PoSChanges,
}

impl SpeculativeRollState {
    /// Creates a new `SpeculativeRollState`
    ///
    /// # Arguments
    /// * `selector`: PoS draws selector controller
    /// * `active_history`: thread-safe shared access the speculative execution history
    pub fn new(
        final_state: Arc<RwLock<FinalState>>,
        active_history: Arc<RwLock<ActiveHistory>>,
        selector: Box<dyn SelectorController>,
    ) -> Self {
        let active_lock = active_history.read();
        let final_lock = final_state.read();
        let production_stats = active_lock
            .fetch_production_stats()
            .or_else(|| final_lock.pos_state.get_production_stats())
            .unwrap_or_default();
        let deferred_credits = active_lock
            .fetch_all_deferred_credits_at(&slot)
            .into_iter()
            .chain(final_lock.pos_state.get_deferred_credits_at(&slot))
            .collect();
        let added_changes = PoSChanges {
            production_stats,
            deferred_credits,
            ..Default::default()
        };
        SpeculativeRollState {
            final_state,
            active_history,
            selector,
            added_changes,
        }
    }

    /// Returns the changes caused to the `SpeculativeRollState` since its creation,
    /// and resets their local value to nothing.
    pub fn take(&mut self) -> PoSChanges {
        std::mem::take(&mut self.added_changes)
    }

    /// Takes a snapshot (clone) of the changes caused to the `SpeculativeRollState` since its creation
    pub fn get_snapshot(&self) -> PoSChanges {
        self.added_changes.clone()
    }

    /// Resets the `SpeculativeRollState` to a snapshot (see `get_snapshot` method)
    pub fn reset_to_snapshot(&mut self, snapshot: PoSChanges) {
        self.added_changes = snapshot;
    }

    /// Add `roll_count` rolls to the buyer address.
    /// Validity checks must be performed _outside_ of this function.
    ///
    /// # Arguments
    /// * `buyer_addr`: address that will receive the rolls
    /// * `roll_count`: number of rolls it will receive
    pub fn add_rolls(&mut self, buyer_addr: &Address, roll_count: u64) {
        let count = self
            .added_changes
            .roll_changes
            .entry(*buyer_addr)
            .or_insert_with(|| {
                self.active_history
                    .read()
                    .fetch_roll_count(buyer_addr)
                    .unwrap_or_else(|| self.final_state.read().pos_state.get_rolls_for(buyer_addr))
            });
        *count = count.saturating_add(roll_count);
    }

    /// Try to sell `roll_count` rolls from the seller address.
    ///
    /// # Arguments
    /// * `seller_addr`: address to sell the rolls from
    /// * `roll_count`: number of rolls to sell
    pub fn try_sell_rolls(
        &mut self,
        seller_addr: &Address,
        slot: Slot,
        roll_count: u64,
    ) -> Result<(), ExecutionError> {
        // take a read lock on the final state
        let final_lock = self.final_state.read();

        // fetch the roll count from: current changes > active history > final state
        let count = self
            .added_changes
            .roll_changes
            .entry(*seller_addr)
            .or_insert_with(|| {
                self.active_history
                    .read()
                    .fetch_roll_count(seller_addr)
                    .unwrap_or_else(|| final_lock.pos_state.get_rolls_for(seller_addr))
            });

        // verify that the seller has enough rolls to sell
        if *count < roll_count {
            return Err(ExecutionError::RollSellError(format!(
                "{} tried to sell {} rolls but only has {}",
                seller_addr, roll_count, count
            )));
        }

        // remove the rolls
        *count = count.saturating_sub(roll_count);

        // add deferred reimbursement corresponding to the sold rolls value
        let credit = self
            .added_changes
            .deferred_credits
            .entry(Slot::last(
                slot.get_cycle(PERIODS_PER_CYCLE) + POS_SELL_CYCLES,
            ))
            .or_insert_with(Map::default);
        credit.insert(*seller_addr, ROLL_PRICE.saturating_mul_u64(roll_count));

        Ok(())
    }

    /// Update production statistics of an address.
    ///
    /// # Arguments
    /// * `creator`: the supposed creator
    /// * `slot`: current slot
    /// * `contains_block`: indicates whether or not `creator` produced the block
    pub fn update_production_stats(&mut self, creator: &Address, slot: Slot, contains_block: bool) {
        if let Some(production_stats) = self.added_changes.production_stats.get_mut(creator) {
            if contains_block {
                production_stats.block_success_count =
                    production_stats.block_success_count.saturating_add(1);
                self.added_changes.seed_bits.push(slot.get_first_bit());
            } else {
                production_stats.block_failure_count =
                    production_stats.block_failure_count.saturating_add(1);
            }
        }
    }

    /// Settle the production statistics at `slot`.
    ///
    /// This function should only be used at the end of a cycle.
    ///
    /// # Arguments:
    /// `slot`: the final slot of the cycle to compute
    pub fn settle_production_stats(&mut self, slot: Slot) {
        let credits = self
            .added_changes
            .deferred_credits
            .entry(Slot::last(
                slot.get_cycle(PERIODS_PER_CYCLE) + POS_SELL_CYCLES,
            ))
            .or_insert_with(Map::default);
        for (addr, stats) in self.added_changes.production_stats.iter() {
            if !stats.satisfying() {
                let rolls = self
                    .added_changes
                    .roll_changes
                    .entry(*addr)
                    .or_insert_with(u64::default);
                // checking overflow for the sake of it
                if let Some(amount) = ROLL_PRICE.checked_mul_u64(*rolls) {
                    credits.insert(*addr, amount);
                }
                *rolls = 0;
            }
        }
    }

    /// Get the deferred credits of `slot`.
    ///
    /// # Arguments
    /// * `slot`: associated slot of the deferred credits to be executed
    pub fn get_deferred_credits(&mut self, slot: Slot) -> Map<Address, Amount> {
        self.added_changes.deferred_credits()
    }
}