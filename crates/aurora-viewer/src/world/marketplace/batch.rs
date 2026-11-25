//! Staggered creation of Marketplace listings.
//!
//! One `POST /listings` at a time, the same request a single "Create listing"
//! sends (`LLMarketplaceData::createListing`, createSLMListingCoro in
//! llmarketplacefunctions.cpp). The Legacy Viewer creates one listing when
//! the merchant asks. A batch is this viewer's convenience: several
//! unassociated folders, or the folders a copy just made, go through that
//! same request with a pause between them so the Marketplace is not flooded.
//! A new listing is never activated. Nothing here sleeps: the caller passes
//! the clock and the gap.

use std::collections::VecDeque;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Inclusive wait, in whole seconds, between two `POST /listings`.
pub const GAP_MIN_SECS: u64 = 5;
pub const GAP_MAX_SECS: u64 = 8;
/// Wait after HTTP 429 or 503 before the same folder is tried again.
pub const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(30);
/// Attempts for one folder, including the first, before a rate limit is
/// reported and the queue moves on.
pub const RATE_LIMIT_TRIES: u32 = 3;

/// `roll` is any value from the caller's generator. The result stays inside
/// [`GAP_MIN_SECS`]..=[`GAP_MAX_SECS`].
pub fn gap_seconds(roll: u64) -> u64 {
    let span = GAP_MAX_SECS - GAP_MIN_SECS + 1;
    GAP_MIN_SECS + roll % span
}

#[derive(Debug, Clone)]
struct Job {
    folder: Uuid,
    tries: u32,
}

#[derive(Debug, Default)]
pub struct CreateQueue {
    pending: VecDeque<Job>,
    inflight: Option<Job>,
    not_before: Option<Instant>,
    done: usize,
    total: usize,
    failures: Vec<(Uuid, String)>,
}

impl CreateQueue {
    pub fn push(&mut self, folders: impl IntoIterator<Item = Uuid>) {
        if self.inflight.is_none() && self.pending.is_empty() {
            self.failures.clear();
            self.done = 0;
            self.total = 0;
        }
        for folder in folders {
            if folder.is_nil() {
                continue;
            }
            if self.pending.iter().any(|job| job.folder == folder) || self.inflight.as_ref().is_some_and(|job| job.folder == folder) {
                continue;
            }
            self.pending.push_back(Job { folder, tries: 0 });
            self.total += 1;
        }
    }

    pub fn is_idle(&self) -> bool {
        self.inflight.is_none() && self.pending.is_empty()
    }

    /// `n` of `m` while a batch is running: the folder in flight, or the next
    /// one during the pause. `None` when nothing is queued.
    pub fn progress(&self) -> Option<(usize, usize)> {
        if self.total == 0 || self.is_idle() {
            return None;
        }
        Some(((self.done + 1).min(self.total), self.total))
    }

    pub fn waiting(&self, now: Instant) -> bool {
        self.inflight.is_none() && !self.pending.is_empty() && self.not_before.is_some_and(|at| now < at)
    }

    /// A new `POST /listings` may leave now. A pause left by the previous one,
    /// even after the queue looked idle, still holds.
    pub fn accepts_immediate(&self, now: Instant) -> bool {
        self.is_idle() && self.not_before.is_none_or(|at| now >= at)
    }

    pub fn failures(&self) -> &[(Uuid, String)] {
        &self.failures
    }

    /// The folder whose `POST /listings` should leave now, if the pause is over.
    pub fn drive(&mut self, now: Instant) -> Option<Uuid> {
        if self.inflight.is_some() || self.not_before.is_some_and(|at| now < at) {
            return None;
        }
        let job = self.pending.pop_front()?;
        let folder = job.folder;
        self.inflight = Some(job);
        Some(folder)
    }

    pub fn owns(&self, folder: Uuid) -> bool {
        self.inflight.as_ref().is_some_and(|job| job.folder == folder)
    }

    pub fn inflight(&self) -> Option<Uuid> {
        self.inflight.as_ref().map(|job| job.folder)
    }

    pub fn succeeded(&mut self, now: Instant, gap: Duration) {
        if self.inflight.take().is_some() {
            self.done += 1;
        }
        self.arm_next(now, gap);
    }

    pub fn failed(&mut self, now: Instant, reason: String, gap: Duration) {
        if let Some(job) = self.inflight.take() {
            self.failures.push((job.folder, reason));
            self.done += 1;
        }
        self.arm_next(now, gap);
    }

    /// `true` when the same folder will be sent again after `backoff`.
    pub fn rate_limited(&mut self, now: Instant, backoff: Duration, gap: Duration) -> bool {
        let Some(mut job) = self.inflight.take() else {
            return false;
        };
        job.tries += 1;
        if job.tries >= RATE_LIMIT_TRIES {
            self.failures.push((job.folder, "rate limit".into()));
            self.done += 1;
            self.arm_next(now, gap);
            return false;
        }
        self.pending.push_front(job);
        self.not_before = Some(now + backoff);
        true
    }

    pub fn cancel(&mut self) {
        let dropped = self.pending.len();
        self.pending.clear();
        self.total = self.total.saturating_sub(dropped);
        if self.inflight.is_none() {
            self.done = 0;
            self.total = 0;
            self.not_before = None;
        }
    }

    fn arm_next(&mut self, now: Instant, gap: Duration) {
        // The pause stays even when nothing is queued: the next folder, from
        // a later copy or another Create, must not leave before it.
        self.not_before = Some(now + gap);
        if self.pending.is_empty() {
            self.done = 0;
            self.total = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn the_gap_stays_between_five_and_eight_seconds() {
        for roll in [0, 1, 2, 3, 4, u64::MAX, 1_000] {
            let seconds = gap_seconds(roll);
            assert!((GAP_MIN_SECS..=GAP_MAX_SECS).contains(&seconds), "{roll} -> {seconds}");
        }
        assert_eq!(gap_seconds(0), 5);
        assert_eq!(gap_seconds(3), 8);
        assert_eq!(gap_seconds(4), 5);
    }

    #[test]
    fn the_first_listing_is_sent_at_once_and_the_next_waits_for_the_gap() {
        let mut queue = CreateQueue::default();
        let now = Instant::now();
        queue.push([id(1), id(2), id(1)]);
        assert_eq!(queue.progress(), Some((1, 2)), "a duplicate folder is not queued twice");
        assert_eq!(queue.drive(now), Some(id(1)));
        assert_eq!(queue.drive(now), None, "one request in flight");
        let gap = Duration::from_secs(5);
        queue.succeeded(now, gap);
        assert!(queue.waiting(now));
        assert_eq!(queue.progress(), Some((2, 2)));
        assert_eq!(queue.drive(now + Duration::from_secs(4)), None);
        assert_eq!(queue.drive(now + gap), Some(id(2)));
        queue.succeeded(now + gap, gap);
        assert!(queue.is_idle());
        assert_eq!(queue.progress(), None);
        queue.push([id(3)]);
        assert!(!queue.accepts_immediate(now + gap), "the pause of the previous listing still holds");
        assert_eq!(queue.drive(now + gap), None);
        assert_eq!(queue.drive(now + gap + gap), Some(id(3)));
    }

    #[test]
    fn a_failure_is_kept_and_the_queue_continues() {
        let mut queue = CreateQueue::default();
        let now = Instant::now();
        let gap = Duration::from_secs(6);
        queue.push([id(1), id(2), id(3)]);
        assert_eq!(queue.drive(now), Some(id(1)));
        queue.failed(now, "missing version".into(), gap);
        assert_eq!(queue.failures(), &[(id(1), "missing version".into())]);
        assert_eq!(queue.drive(now), None);
        assert_eq!(queue.drive(now + gap), Some(id(2)));
        queue.succeeded(now + gap, gap);
        assert_eq!(queue.drive(now + gap + gap), Some(id(3)));
    }

    #[test]
    fn a_rate_limit_retries_the_same_folder_after_the_backoff() {
        let mut queue = CreateQueue::default();
        let now = Instant::now();
        let gap = Duration::from_secs(5);
        queue.push([id(1), id(2)]);
        assert_eq!(queue.drive(now), Some(id(1)));
        assert!(queue.rate_limited(now, RATE_LIMIT_BACKOFF, gap));
        assert_eq!(queue.drive(now + Duration::from_secs(29)), None);
        assert_eq!(queue.drive(now + RATE_LIMIT_BACKOFF), Some(id(1)));
        queue.succeeded(now + RATE_LIMIT_BACKOFF, gap);
        assert_eq!(queue.failures(), &[]);
        assert_eq!(queue.drive(now + RATE_LIMIT_BACKOFF + gap), Some(id(2)));
    }

    #[test]
    fn too_many_rate_limits_skip_the_folder_and_continue() {
        let mut queue = CreateQueue::default();
        let now = Instant::now();
        let gap = Duration::from_secs(5);
        queue.push([id(1), id(2)]);
        let mut at = now;
        for _ in 0..RATE_LIMIT_TRIES - 1 {
            assert_eq!(queue.drive(at), Some(id(1)));
            assert!(queue.rate_limited(at, RATE_LIMIT_BACKOFF, gap));
            at += RATE_LIMIT_BACKOFF;
        }
        assert_eq!(queue.drive(at), Some(id(1)));
        assert!(!queue.rate_limited(at, RATE_LIMIT_BACKOFF, gap));
        assert_eq!(queue.failures().len(), 1);
        assert_eq!(queue.failures()[0].0, id(1));
        assert_eq!(queue.drive(at), None);
        assert_eq!(queue.drive(at + gap), Some(id(2)));
    }

    #[test]
    fn cancel_drops_what_has_not_been_sent() {
        let mut queue = CreateQueue::default();
        let now = Instant::now();
        queue.push([id(1), id(2), id(3)]);
        assert_eq!(queue.drive(now), Some(id(1)));
        queue.cancel();
        assert_eq!(queue.progress(), Some((1, 1)), "the request already sent still counts");
        assert_eq!(queue.drive(now), None);
        queue.succeeded(now, Duration::from_secs(5));
        assert!(queue.is_idle());
        assert_eq!(queue.drive(now + Duration::from_secs(5)), None);
    }
}
