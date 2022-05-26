//! Admin's representation of rewards and history of the system.

use crate::prelude::*;
use std::cell;

/// To create a user incentive for token possession, we distribute time
/// dependent rewards. A farmer stakes tokens of a mint `S`, ie. they lock them
/// with the program, and they become eligible for harvest.
#[derive(Default)]
#[account(zero_copy)]
pub struct Farm {
    /// Can change settings on this farm.
    pub admin: Pubkey,
    /// The mint of tokens which are staked by farmers. Also referred to as
    /// `S`.
    ///
    /// Created e.g. in the core part of the AMM logic and here
    /// serves as a natural boundary between the two features: _(1)_ depositing
    /// liquidity and swapping; _(2)_ farming with which this document is
    /// concerned
    pub stake_mint: Pubkey,
    /// Staked tokens are stored in this program's vault (token account.)
    ///
    /// This is derivable from the farm's pubkey as a seed.
    pub stake_vault: Pubkey,
    /// List of different harvest mints with configuration of how many tokens
    /// are released per slot.
    ///
    /// # Important
    /// Defaults to an array with all harvest mints as default pubkeys. Only
    /// when a pubkey is not the default one is the harvest initialized.
    ///
    /// # Note
    /// Len must match [`consts::MAX_HARVEST_MINTS`].
    pub harvests: [Harvest; 10],
    /// Stores snapshots of the amount of total staked tokens and changes to
    /// `ρ`.
    pub snapshots: Snapshots,
    /// Enforces a minimum amount of timespan between snapshots, thus ensures
    /// that the ring_buffer in total has a minimum amount of time ellapsed.
    /// When a Farm is initiated, min_snapshot_window_slots is defaulted to
    /// zero. When zero, the endpoint take_snapshots will set this contraint
    /// to the default value [`consts::MIN_SNAPSHOT_WINDOW_SLOTS`].
    /// This field is configurable via the endpoint set_min_snapshot_window
    /// which can be called by the admin.
    pub min_snapshot_window_slots: u64,
}

/// # Important
/// If the `harvest_mint` is equal to [`Pubkey::default`], then the harvest
/// is uninitialized. We don't use an enum to represent uninitialized mints as
/// the anchor FE client has troubles parsing enums in zero copy accounts. And
/// this way we also safe some account space.
#[derive(Debug, Eq, PartialEq, Default)]
#[zero_copy]
pub struct Harvest {
    /// The mint of tokens which are distributed to farmers. This can be the
    /// same mint as `S`.
    pub mint: Pubkey,
    /// Admin deposits the reward tokens which are harvested by farmer into
    /// this vault.
    ///
    /// This is derivable from the farm's pubkey and harvest mint's pubkey.
    pub vault: Pubkey,
    /// The harvest is distributed using a configurable _tokens per slot_
    /// (`ρ`.) This value represents how many tokens should be divided
    /// between all farmers per slot (~0.4s.)
    ///
    /// This stores an ordered list of changes to this setting. We need to keep
    /// the history because changes to this value should never apply
    /// retroactively. The history is limited by the snapshots ring buffer full
    /// rotation period. See the design docs for more info.
    ///
    /// # Important
    /// This array is ordered by the `TokensPerSlotHistory.since.slot` integer
    /// DESC.
    ///
    /// # Note
    /// This len must match [`consts::TOKENS_PER_SLOT_HISTORY_LEN`].
    pub tokens_per_slot: [TokensPerSlotHistory; 10],
}

#[derive(Debug, Default, Eq, PartialEq)]
#[zero_copy]
pub struct TokensPerSlotHistory {
    pub value: TokenAmount,
    /// The new value was updated at this slot. However, it will not be valid
    /// _since_ this slot, only since the first snapshot start slot that's
    /// greater than this slot. That is, the configuration cannot be applied
    /// to currently open snapshot window.
    pub at: Slot,
}

#[derive(Eq, PartialEq)]
#[zero_copy]
pub struct Snapshots {
    /// What's the last snapshot index to consider valid. When the buffer tip
    /// reaches [`consts::SNAPSHOTS_LEN`], it is set to 0 again and now the
    /// queue of snapshots starts at index 1. With next call, the tip is set to
    /// 1 and queue starts at index 2.
    ///
    /// There's a special scenario to consider which is the first population of
    /// the ring buffer. We check the slot at the last index of the buffer and
    /// if the slot is equal to zero, that means that we haven't done the first
    /// rotation around the buffer yet. And therefore if the tip is at N, in
    /// this special case the beginning is on index 0 and not N + 1.
    ///
    /// # Note
    /// It's [`u64`] and not smaller because otherwise there are issues with
    /// packing of this struct and deserialization.
    pub ring_buffer_tip: u64,
    /// How many tokens were in the staking vault.
    ///
    /// # Note
    /// Len must match [`consts::SNAPSHOTS_LEN`].
    pub ring_buffer: [Snapshot; 1000],
}

/// Defines a snapshot window.
#[derive(Debug, Default, Eq, PartialEq)]
#[zero_copy]
pub struct Snapshot {
    pub staked: TokenAmount,
    pub started_at: Slot,
}

impl Default for Snapshots {
    fn default() -> Self {
        Self {
            ring_buffer_tip: 0,
            ring_buffer: [Snapshot::default(); consts::SNAPSHOTS_LEN],
        }
    }
}

impl Farm {
    pub const SIGNER_PDA_PREFIX: &'static [u8; 6] = b"signer";
    pub const STAKE_VAULT_PREFIX: &'static [u8; 11] = b"stake_vault";
}

impl Harvest {
    pub const VAULT_PREFIX: &'static [u8; 13] = b"harvest_vault";

    /// Returns the last change to ρ before or at a given slot.
    ///
    /// # Important
    /// If the admin changes ρ during an open snapshot window, it should only be
    /// considered from the next snapshot. This method _does not account_ for
    /// that invariant.
    ///
    /// # Returns
    /// First tuple member is the ρ itself, second tuple member returns the slot
    /// of the _next_ ρ change if any ([`None`] if latest.)
    pub fn tokens_per_slot(&self, at: Slot) -> (TokenAmount, Option<Slot>) {
        match self
            .tokens_per_slot
            .iter()
            .position(|tps| tps.at.slot <= at.slot)
        {
            Some(0) => (self.tokens_per_slot[0].value, None),
            Some(i) => (
                self.tokens_per_slot[i].value,
                Some(self.tokens_per_slot[i - 1].at),
            ),
            None => {
                msg!("There is no ρ history for the farm at {}", at.slot);
                (
                    // no history = harvest lost
                    TokenAmount { amount: 0 },
                    // find the oldest (hence rev) change to the setting
                    self.tokens_per_slot
                        .iter()
                        .rev()
                        .find(|tps| tps.value.amount != 0)
                        .map(|tps| tps.at)
                        .or(Some(self.tokens_per_slot[0].at)),
                )
            }
        }
    }
}

impl Farm {
    /// Use use [`cell::Ref`] because farm is too large to fit on stack.
    pub fn latest_snapshot(farm: &cell::Ref<Farm>) -> Snapshot {
        farm.snapshots.ring_buffer[farm.snapshots.ring_buffer_tip as usize]
    }

    /// This method contains the core logic of the take_snapshot endpoint.
    /// The method is called in the handle function of the endpoint.
    /// It writes current stake_vault amount along with the current slot
    /// to the snapshot positioned in the next ring_buffer_tip.
    pub fn take_snapshot(
        &mut self,
        current_slot: Slot,
        stake_vault: TokenAmount,
    ) -> Result<()> {
        // When the farm is initialised, farm.min_snapshot_window_slots is set
        // to zero If the admin does not change this value the program
        // defaults the minimum snapshot window slots to the default
        // value
        let min_snapshot_window_slots = if self.min_snapshot_window_slots == 0 {
            consts::MIN_SNAPSHOT_WINDOW_SLOTS
        } else {
            self.min_snapshot_window_slots
        };

        let mut snapshots = &mut self.snapshots;

        // The slot in which the last snapshot was taken
        let last_snapshot_slot = snapshots.ring_buffer
            [(snapshots.ring_buffer_tip as usize)]
            .started_at
            .slot;

        // Assert that sufficient time as passed
        if current_slot.slot < last_snapshot_slot + min_snapshot_window_slots {
            return Err(error!(
                AmmError::InsufficientSlotTimeSinceLastSnapshot
            ));
        }

        // Set snapshot ring buffer tip to next
        // When the farm is initialised, the ring_buffer_tip is defaulted to
        // zero. This means that the first in the first iteration of the
        // ring_buffer the new snapshot elements are recorded
        // from the index 1 onwards. Only when the tip reaches the max value and
        // it resets to 0 that the snapshot elements start being
        // recorded from  index 0 onwards.
        let is_tip_last_index = snapshots.ring_buffer_tip as usize
            == snapshots.ring_buffer.len() - 1;

        snapshots.ring_buffer_tip = if is_tip_last_index {
            0
        } else {
            snapshots.ring_buffer_tip + 1
        };

        // Write data to the to the buffer
        let tip = snapshots.ring_buffer_tip as usize;

        snapshots.ring_buffer[tip] = Snapshot {
            staked: TokenAmount {
                amount: stake_vault.amount,
            },
            started_at: Slot {
                slot: current_slot.slot,
            },
        };

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_matches_harvest_tokens_per_slot_with_const() {
        let harvest = Harvest::default();

        assert_eq!(
            harvest.tokens_per_slot.len(),
            consts::TOKENS_PER_SLOT_HISTORY_LEN
        );
    }

    #[test]
    fn it_matches_snapshots_with_const() {
        let snapshots = Snapshots::default();

        assert_eq!(snapshots.ring_buffer.len(), consts::SNAPSHOTS_LEN);
    }

    #[test]
    fn it_matches_harvests_with_const() {
        let farm = Farm::default();

        assert_eq!(farm.harvests.len(), consts::MAX_HARVEST_MINTS);
    }

    #[test]
    fn it_has_stable_size() {
        let farm = Farm::default();

        assert_eq!(8 + std::mem::size_of_val(&farm), 18_360);
    }

    #[test]
    fn it_calculates_tps_with_empty_setting() {
        let harvest = Harvest::default();
        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 0 });
        assert_eq!(tps.amount, 0);
        assert!(next_change.is_none());
    }

    #[test]
    fn it_calculates_tps_with_one_setting() {
        let mut harvest = Harvest::default();
        harvest.tokens_per_slot[0] = TokensPerSlotHistory {
            value: TokenAmount { amount: 10 },
            at: Slot { slot: 100 },
        };

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 0 });
        assert_eq!(tps.amount, 0);
        assert_eq!(next_change, Some(Slot { slot: 100 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 50 });
        assert_eq!(tps.amount, 0);
        assert_eq!(next_change, Some(Slot { slot: 100 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 100 });
        assert_eq!(tps.amount, 10);
        assert!(next_change.is_none());

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 101 });
        assert_eq!(tps.amount, 10);
        assert!(next_change.is_none());

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 200 });
        assert_eq!(tps.amount, 10);
        assert!(next_change.is_none());
    }

    #[test]
    fn it_calculates_tps_with_five_settings() {
        let mut harvest = Harvest::default();
        harvest.tokens_per_slot[0] = TokensPerSlotHistory {
            value: TokenAmount { amount: 10 },
            at: Slot { slot: 100 },
        };
        harvest.tokens_per_slot[1] = TokensPerSlotHistory {
            value: TokenAmount { amount: 5 },
            at: Slot { slot: 90 },
        };
        harvest.tokens_per_slot[2] = TokensPerSlotHistory {
            value: TokenAmount { amount: 8 },
            at: Slot { slot: 80 },
        };
        harvest.tokens_per_slot[3] = TokensPerSlotHistory {
            value: TokenAmount { amount: 0 },
            at: Slot { slot: 70 },
        };
        harvest.tokens_per_slot[4] = TokensPerSlotHistory {
            value: TokenAmount { amount: 20 },
            at: Slot { slot: 60 },
        };

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 101 });
        assert_eq!(tps.amount, 10);
        assert!(next_change.is_none());

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 100 });
        assert_eq!(tps.amount, 10);
        assert!(next_change.is_none());

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 91 });
        assert_eq!(tps.amount, 5);
        assert_eq!(next_change, Some(Slot { slot: 100 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 90 });
        assert_eq!(tps.amount, 5);
        assert_eq!(next_change, Some(Slot { slot: 100 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 89 });
        assert_eq!(tps.amount, 8);
        assert_eq!(next_change, Some(Slot { slot: 90 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 81 });
        assert_eq!(tps.amount, 8);
        assert_eq!(next_change, Some(Slot { slot: 90 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 80 });
        assert_eq!(tps.amount, 8);
        assert_eq!(next_change, Some(Slot { slot: 90 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 71 });
        assert_eq!(tps.amount, 0);
        assert_eq!(next_change, Some(Slot { slot: 80 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 61 });
        assert_eq!(tps.amount, 20);
        assert_eq!(next_change, Some(Slot { slot: 70 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 60 });
        assert_eq!(tps.amount, 20);
        assert_eq!(next_change, Some(Slot { slot: 70 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 59 });
        assert_eq!(tps.amount, 0);
        assert_eq!(next_change, Some(Slot { slot: 60 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 0 });
        assert_eq!(tps.amount, 0);
        assert_eq!(next_change, Some(Slot { slot: 60 }));
    }

    #[test]
    fn it_calculates_tps_with_max_settings() {
        let mut harvest = Harvest::default();
        harvest.tokens_per_slot[0] = TokensPerSlotHistory {
            value: TokenAmount { amount: 10 },
            at: Slot { slot: 100 },
        };
        for i in 1..(consts::MAX_HARVEST_MINTS - 2) {
            harvest.tokens_per_slot[i] = harvest.tokens_per_slot[0];
        }
        harvest.tokens_per_slot[8] = TokensPerSlotHistory {
            value: TokenAmount { amount: 1 },
            at: Slot { slot: 10 },
        };
        harvest.tokens_per_slot[9] = TokensPerSlotHistory {
            value: TokenAmount { amount: 5 },
            at: Slot { slot: 5 },
        };

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 9 });
        assert_eq!(tps.amount, 5);
        assert_eq!(next_change, Some(Slot { slot: 10 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 5 });
        assert_eq!(tps.amount, 5);
        assert_eq!(next_change, Some(Slot { slot: 10 }));

        let (tps, next_change) = harvest.tokens_per_slot(Slot { slot: 0 });
        assert_eq!(tps.amount, 0);
        assert_eq!(next_change, Some(Slot { slot: 5 }));
    }

    #[test]
    fn it_returns_farm_latest_snapshot() {
        let farm = Farm::default();
        assert_eq!(
            Farm::latest_snapshot(&cell::RefCell::new(farm).borrow()),
            Snapshot::default()
        );

        let mut farm = Farm::default();
        farm.snapshots.ring_buffer_tip = 10;
        farm.snapshots.ring_buffer[10] = Snapshot {
            staked: TokenAmount::new(10),
            started_at: Slot::new(20),
        };
        assert_eq!(
            Farm::latest_snapshot(&cell::RefCell::new(farm).borrow()),
            Snapshot {
                staked: TokenAmount::new(10),
                started_at: Slot::new(20),
            }
        );
    }

    #[test]
    fn it_takes_snapshot() {
        let mut farm = Farm::default();
        farm.min_snapshot_window_slots = 1;

        let stake_vault_amount = 10;
        let current_slot = 5;

        assert_eq!(farm.snapshots.ring_buffer_tip, 0);

        let _result = farm.take_snapshot(
            Slot::new(current_slot),
            TokenAmount::new(stake_vault_amount),
        );

        // After take_snapshot is called the tip should
        // move from 0 to 1
        assert_eq!(farm.snapshots.ring_buffer_tip, 1);

        assert_eq!(
            farm.snapshots.ring_buffer[1].staked,
            TokenAmount { amount: 10 }
        );

        assert_eq!(farm.snapshots.ring_buffer[1].started_at, Slot { slot: 5 });
    }
}
