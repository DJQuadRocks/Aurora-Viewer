//! Repeated copies into Marketplace listings, one inventory change at a time.
//!
//! Each step is the plan of one "Copy to Marketplace listings" or
//! "Move to Marketplace listings" (`actions::marketplace`, the
//! `move_*_to_marketplacelistings` helpers of llinventoryfunctions.cpp and
//! `LLFolderBridge::performAction` in llinventorybridge.cpp). A count asks
//! for that many separate listing folders. When the merchant also creates
//! listings, the new folders are returned once the inventory change has
//! landed, for the staggered `POST /listings` queue. Nothing here sleeps.

use aurora_net::inventory::operations::{Mutation, Operation};
use std::collections::HashMap;
use uuid::Uuid;

pub const COPY_MIN: u32 = 1;
pub const COPY_MAX: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyCountError {
    NotANumber,
    OutOfRange,
}

/// The number typed in the copy dialog: 1 to 100.
pub fn parse_copy_count(text: &str) -> Result<u32, CopyCountError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CopyCountError::NotANumber);
    }
    match text.parse::<u32>() {
        Ok(count) if (COPY_MIN..=COPY_MAX).contains(&count) => Ok(count),
        Ok(_) => Err(CopyCountError::OutOfRange),
        Err(_) => Err(CopyCountError::NotANumber),
    }
}

/// The listing folders a marketplace copy or move is about to create: the
/// new children of the Marketplace listings folder. Version and stock
/// folders sit under those, as one single copy would arrange them.
pub fn staged_listing_folders(change: &Mutation, listings_root: Option<Uuid>) -> Vec<Uuid> {
    let root = listings_root.or_else(|| {
        change.operations.iter().find_map(|op| match op {
            Operation::Folder { id, type_default: 53, .. } => Some(*id),
            _ => None,
        })
    });
    let Some(root) = root else {
        return Vec::new();
    };
    change
        .operations
        .iter()
        .filter_map(|op| match op {
            Operation::Folder {
                id, parent, type_default, ..
            } if *parent == root && *type_default != 53 => Some(*id),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Transfer {
    pub ids: Vec<Uuid>,
    pub copy: bool,
    pub count: u32,
    pub create_listings: bool,
}

#[derive(Debug, Clone)]
enum Phase {
    Ready,
    /// The listings window is holding the change behind a question.
    Guard {
        folders: Vec<Uuid>,
        copies: bool,
    },
    Inventory {
        request: Uuid,
        folders: Vec<Uuid>,
        copies: bool,
    },
}

#[derive(Debug, Default)]
pub struct CopyPump {
    ids: Vec<Uuid>,
    copy: bool,
    remaining: u32,
    total: u32,
    create_listings: bool,
    phase: Option<Phase>,
}

impl CopyPump {
    pub fn start(&mut self, transfer: Transfer) -> Result<(), CopyCountError> {
        if transfer.ids.is_empty() {
            return Err(CopyCountError::NotANumber);
        }
        if transfer.copy && !(COPY_MIN..=COPY_MAX).contains(&transfer.count) {
            return Err(CopyCountError::OutOfRange);
        }
        let total = if transfer.copy { transfer.count } else { 1 };
        self.ids = transfer.ids;
        self.copy = transfer.copy;
        self.remaining = total;
        self.total = total;
        self.create_listings = transfer.create_listings;
        self.phase = Some(Phase::Ready);
        Ok(())
    }

    pub fn active(&self) -> bool {
        self.phase.is_some()
    }

    /// `n` of `m` for the copy that is running or about to run.
    pub fn progress(&self) -> Option<(u32, u32)> {
        self.phase.as_ref().map(|_| {
            let done = self.total.saturating_sub(self.remaining);
            ((done + 1).min(self.total), self.total)
        })
    }

    pub fn next_ids(&self) -> Option<(Vec<Uuid>, bool)> {
        match &self.phase {
            Some(Phase::Ready) if self.remaining > 0 => Some((self.ids.clone(), self.copy)),
            _ => None,
        }
    }

    pub fn submitted(&mut self, request: Uuid, folders: Vec<Uuid>, copies: bool) {
        self.phase = Some(Phase::Inventory { request, folders, copies });
    }

    pub fn note_held(&mut self, folders: Vec<Uuid>, copies: bool) {
        self.phase = Some(Phase::Guard { folders, copies });
    }

    pub fn poll_guard(&mut self, pending: Option<Uuid>, still_asking: bool) {
        let Some(Phase::Guard { folders, copies }) = &self.phase else {
            return;
        };
        let folders = folders.clone();
        let copies = *copies;
        if let Some(request) = pending {
            self.phase = Some(Phase::Inventory { request, folders, copies });
        } else if !still_asking {
            self.clear();
        }
    }

    pub fn reject(&mut self) {
        self.clear();
    }

    /// The listing folders of a finished step, once the inventory has them.
    /// `None` while copies are still arriving, or when `request` is not this
    /// queue's. The ids are rewritten through `created` when the server
    /// assigned new ones.
    pub fn on_reply(&mut self, request: Uuid, copies_still_running: bool, created: &HashMap<Uuid, Uuid>) -> Option<Vec<Uuid>> {
        let Some(Phase::Inventory {
            request: waiting,
            folders,
            copies,
        }) = &self.phase
        else {
            return None;
        };
        if *waiting != request {
            return None;
        }
        let folders = remap(folders, created);
        let copies = *copies;
        if copies_still_running && copies {
            self.phase = Some(Phase::Inventory {
                request,
                folders,
                copies: true,
            });
            return None;
        }
        Some(self.finish(folders))
    }

    pub fn on_copies_finished(&mut self, request: Uuid) -> Option<Vec<Uuid>> {
        let Some(Phase::Inventory {
            request: waiting,
            folders,
            copies: true,
        }) = &self.phase
        else {
            return None;
        };
        if *waiting != request {
            return None;
        }
        let folders = folders.clone();
        Some(self.finish(folders))
    }

    pub fn on_failure(&mut self, request: Uuid) -> bool {
        match &self.phase {
            Some(Phase::Inventory { request: waiting, .. }) if *waiting == request => {
                self.clear();
                true
            }
            _ => false,
        }
    }

    /// Drop the copies that have not started. The one already sent is allowed
    /// to finish, so its listing folder can still be created.
    pub fn cancel(&mut self) {
        match &self.phase {
            Some(Phase::Ready) | None => self.clear(),
            Some(Phase::Guard { .. } | Phase::Inventory { .. }) => self.remaining = 1,
        }
    }

    fn finish(&mut self, folders: Vec<Uuid>) -> Vec<Uuid> {
        self.remaining = self.remaining.saturating_sub(1);
        let out = if self.create_listings { folders } else { Vec::new() };
        if self.remaining == 0 {
            self.clear();
        } else {
            self.phase = Some(Phase::Ready);
        }
        out
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

fn remap(folders: &[Uuid], created: &HashMap<Uuid, Uuid>) -> Vec<Uuid> {
    folders.iter().map(|id| created.get(id).copied().unwrap_or(*id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_net::inventory::operations::Operation;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn transfer(copy: bool, count: u32, create_listings: bool) -> Transfer {
        Transfer {
            ids: vec![id(7)],
            copy,
            count,
            create_listings,
        }
    }

    #[test]
    fn copy_counts_accept_one_to_one_hundred() {
        assert_eq!(parse_copy_count("1"), Ok(1));
        assert_eq!(parse_copy_count(" 100 "), Ok(100));
        assert_eq!(parse_copy_count("42"), Ok(42));
        assert_eq!(parse_copy_count("01"), Ok(1));
        assert_eq!(parse_copy_count(""), Err(CopyCountError::NotANumber));
        assert_eq!(parse_copy_count("  "), Err(CopyCountError::NotANumber));
        assert_eq!(parse_copy_count("one"), Err(CopyCountError::NotANumber));
        assert_eq!(parse_copy_count("1.5"), Err(CopyCountError::NotANumber));
        assert_eq!(parse_copy_count("0"), Err(CopyCountError::OutOfRange));
        assert_eq!(parse_copy_count("101"), Err(CopyCountError::OutOfRange));
    }

    #[test]
    fn copies_run_one_at_a_time_and_a_move_is_a_single_step() {
        let mut pump = CopyPump::default();
        pump.start(transfer(true, 3, false)).expect("three copies");
        assert_eq!(pump.progress(), Some((1, 3)));
        assert_eq!(pump.next_ids(), Some((vec![id(7)], true)));
        pump.submitted(id(1), vec![id(10)], true);
        assert_eq!(pump.next_ids(), None, "the next copy waits");
        assert!(pump.on_reply(id(1), true, &HashMap::new()).is_none());
        assert!(pump.on_copies_finished(id(1)).unwrap().is_empty(), "create was not asked");
        assert_eq!(pump.progress(), Some((2, 3)));
        assert_eq!(pump.next_ids(), Some((vec![id(7)], true)));
        pump.cancel();
        pump.submitted(id(2), vec![id(11)], false);
        assert!(pump.on_reply(id(2), false, &HashMap::new()).unwrap().is_empty());
        assert!(!pump.active(), "cancel drops the third copy");

        pump.start(transfer(false, 9, false)).expect("a move ignores the count");
        assert_eq!(pump.progress(), Some((1, 1)));
        pump.submitted(id(3), vec![id(12)], false);
        assert!(pump.on_reply(id(3), false, &HashMap::new()).unwrap().is_empty());
        assert!(!pump.active());
    }

    #[test]
    fn create_listings_is_handed_the_new_folders_only_after_the_copy_lands() {
        let mut pump = CopyPump::default();
        pump.start(transfer(true, 2, true)).expect("two copies");
        pump.submitted(id(1), vec![id(10), id(11)], true);
        let created = HashMap::from([(id(10), id(110))]);
        assert!(pump.on_reply(id(1), true, &created).is_none());
        assert_eq!(pump.on_copies_finished(id(1)).unwrap(), vec![id(110), id(11)]);
        pump.submitted(id(2), vec![id(12)], false);
        assert_eq!(pump.on_reply(id(2), false, &HashMap::new()).unwrap(), vec![id(12)]);
        assert!(!pump.active());
    }

    #[test]
    fn an_inventory_failure_stops_the_remaining_copies() {
        let mut pump = CopyPump::default();
        pump.start(transfer(true, 4, true)).expect("four");
        pump.submitted(id(1), vec![id(10)], false);
        assert!(pump.on_failure(id(1)));
        assert!(!pump.active());
        assert!(!pump.on_failure(id(1)));
    }

    #[test]
    fn staged_listing_folders_are_the_new_children_of_the_listings_root() {
        let root = id(53);
        let listing = id(8);
        let version = id(9);
        let change = Mutation {
            operations: vec![
                Operation::Folder {
                    id: root,
                    parent: id(1),
                    name: "Marketplace listings".into(),
                    type_default: 53,
                },
                Operation::Folder {
                    id: listing,
                    parent: root,
                    name: "Chair".into(),
                    type_default: -1,
                },
                Operation::Folder {
                    id: version,
                    parent: listing,
                    name: "Chair".into(),
                    type_default: -1,
                },
            ],
            ..Default::default()
        };
        assert_eq!(staged_listing_folders(&change, None), vec![listing]);
        assert_eq!(staged_listing_folders(&change, Some(root)), vec![listing]);
    }
}
