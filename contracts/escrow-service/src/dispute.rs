//! Dispute rules.
//!
//! Who may raise which dispute, and what happens if the arbiter does not rule
//! before the arbitration deadline. The default rulings exist so an absent
//! arbiter can never strand funds in escrow indefinitely: once the deadline
//! passes, anyone can apply the default for the dispute's reason.

use soroban_sdk::contracttype;

use crate::PartyRole;

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisputeReason {
    /// Buyer: the goods or service never arrived.
    NonDelivery,
    /// Buyer: what arrived does not match the listing.
    NotAsDescribed,
    /// Buyer: the item is broken or faulty.
    Defective,
    /// Buyer: a different item was delivered.
    WrongItem,
    /// Buyer: delivered, but well past the agreed date.
    LateDelivery,
    /// Seller: delivered, but the buyer will not confirm or respond.
    BuyerUnresponsive,
    /// Either party: anything else.
    Other,
}

impl DisputeReason {
    /// Whether a party in `role` may raise a dispute for this reason.
    pub fn can_be_raised_by(self, role: PartyRole) -> bool {
        match self {
            DisputeReason::BuyerUnresponsive => role == PartyRole::Seller,
            DisputeReason::Other => true,
            _ => role == PartyRole::Buyer,
        }
    }

    /// Share of the remaining escrow returned to the buyer, in basis points,
    /// when the arbiter misses the deadline. The rest goes to the seller.
    pub fn default_buyer_bps(self) -> u32 {
        match self {
            // The buyer's claim stands if no one disproves it: full refund.
            DisputeReason::NonDelivery
            | DisputeReason::NotAsDescribed
            | DisputeReason::Defective
            | DisputeReason::WrongItem => 10_000,
            // Goods arrived, just late: split the difference.
            DisputeReason::LateDelivery | DisputeReason::Other => 5_000,
            // Seller delivered and the buyer went silent: pay the seller.
            DisputeReason::BuyerUnresponsive => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sellers_claim_buyer_unresponsive() {
        assert!(DisputeReason::BuyerUnresponsive.can_be_raised_by(PartyRole::Seller));
        assert!(!DisputeReason::BuyerUnresponsive.can_be_raised_by(PartyRole::Buyer));
        assert!(!DisputeReason::NonDelivery.can_be_raised_by(PartyRole::Seller));
        assert!(DisputeReason::Other.can_be_raised_by(PartyRole::Seller));
        assert!(DisputeReason::Other.can_be_raised_by(PartyRole::Buyer));
    }

    #[test]
    fn defaults_never_exceed_one_hundred_percent() {
        for r in [
            DisputeReason::NonDelivery,
            DisputeReason::NotAsDescribed,
            DisputeReason::Defective,
            DisputeReason::WrongItem,
            DisputeReason::LateDelivery,
            DisputeReason::BuyerUnresponsive,
            DisputeReason::Other,
        ] {
            assert!(r.default_buyer_bps() <= 10_000);
        }
    }
}
