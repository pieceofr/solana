//! Utility functions and types for Epoch Accounts Hash

use {
    crate::bank::Bank,
    solana_sdk::clock::{Epoch, Slot},
};

/// Calculation of the EAH occurs once per epoch.  All nodes in the cluster must agree on which
/// slot the EAH is based on.  This slot will be at an offset into the epoch, and referred to as
/// the "start" slot for the EAH calculation.
#[must_use]
#[inline]
pub fn calculation_offset_start(bank: &Bank) -> Slot {
    calculation_info(bank).calculation_offset_start
}

/// Calculation of the EAH occurs once per epoch.  All nodes in the cluster must agree on which
/// bank will hash the EAH into its `Bank::hash`.  This slot will be at an offset into the epoch,
/// and referred to as the "stop" slot for the EAH calculation.  All nodes must complete the EAH
/// calculation before this slot!
#[must_use]
#[inline]
pub fn calculation_offset_stop(bank: &Bank) -> Slot {
    calculation_info(bank).calculation_offset_stop
}

/// For the epoch that `bank` is in, get the slot that the EAH calculation starts
#[must_use]
#[inline]
pub fn calculation_start(bank: &Bank) -> Slot {
    calculation_info(bank).calculation_start
}

/// For the epoch that `bank` is in, get the slot that the EAH calculation stops
#[must_use]
#[inline]
pub fn calculation_stop(bank: &Bank) -> Slot {
    calculation_info(bank).calculation_stop
}

/// Is this bank in the calculation window?
#[must_use]
#[inline]
pub fn is_in_calculation_window(bank: &Bank) -> bool {
    let bank_slot = bank.slot();
    let info = calculation_info(bank);
    bank_slot >= info.calculation_start && bank_slot < info.calculation_stop
}

/// For the epoch that `bank` is in, get all the EAH calculation information
pub fn calculation_info(bank: &Bank) -> CalculationInfo {
    let epoch = bank.epoch();
    let epoch_schedule = bank.epoch_schedule();

    let slots_per_epoch = epoch_schedule.get_slots_in_epoch(epoch);
    let calculation_offset_start = slots_per_epoch / 4;
    let calculation_offset_stop = slots_per_epoch / 4 * 3;

    let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(epoch);
    let last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);
    let calculation_start = first_slot_in_epoch.saturating_add(calculation_offset_start);
    let calculation_stop = first_slot_in_epoch.saturating_add(calculation_offset_stop);

    CalculationInfo {
        epoch,
        slots_per_epoch,
        first_slot_in_epoch,
        last_slot_in_epoch,
        calculation_offset_start,
        calculation_offset_stop,
        calculation_start,
        calculation_stop,
    }
}

/// All the EAH calculation information for a specific epoch
///
/// Computing the EAH calculation information looks up a bunch of values.  Instead of throwing
/// those values away, they are kept in here as well.  This may aid in future debugging, and the
/// additional fields are trivial in size.
#[derive(Debug, Default, Copy, Clone)]
pub struct CalculationInfo {
    /*
     * The values that were looked up, which were needed to get the calculation info
     */
    /// The epoch this information applies to
    pub epoch: Epoch,
    /// Number of slots in this epoch
    pub slots_per_epoch: u64,
    /// First slot in this epoch
    pub first_slot_in_epoch: Slot,
    /// Last slot in this epoch
    pub last_slot_in_epoch: Slot,

    /*
     * The computed values for the calculation info
     */
    /// Offset into the epoch when the EAH calculation starts
    pub calculation_offset_start: Slot,
    /// Offset into the epoch when the EAH calculation stops
    pub calculation_offset_stop: Slot,
    /// Absolute slot where the EAH calculation starts
    pub calculation_start: Slot,
    /// Absolute slot where the EAH calculation stops
    pub calculation_stop: Slot,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{epoch_schedule::EpochSchedule, genesis_config::GenesisConfig},
    };

    #[test]
    fn test_calculation_offset_bounds() {
        let bank = Bank::default_for_tests();
        let offset_start = calculation_offset_start(&bank);
        let offset_stop = calculation_offset_stop(&bank);
        assert!(offset_start < offset_stop);
    }

    #[test]
    fn test_calculation_bounds() {
        let bank = Bank::default_for_tests();
        let start = calculation_start(&bank);
        let stop = calculation_stop(&bank);
        assert!(start < stop);
    }

    #[test]
    fn test_calculation_info() {
        for slots_per_epoch in [32, 100, 65_536, 432_000, 123_456_789] {
            for warmup in [false, true] {
                let genesis_config = GenesisConfig {
                    epoch_schedule: EpochSchedule::custom(slots_per_epoch, slots_per_epoch, warmup),
                    ..GenesisConfig::default()
                };
                let info = calculation_info(&Bank::new_for_tests(&genesis_config));
                assert!(info.calculation_offset_start < info.calculation_offset_stop);
                assert!(info.calculation_offset_start < info.slots_per_epoch);
                assert!(info.calculation_offset_stop < info.slots_per_epoch);
                assert!(info.calculation_start < info.calculation_stop,);
                assert!(info.calculation_start > info.first_slot_in_epoch,);
                assert!(info.calculation_stop < info.last_slot_in_epoch,);
            }
        }
    }
}
