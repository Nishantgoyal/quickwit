// Copyright (C) 2024 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use quickwit_common::uri::Uri;
use quickwit_config::SplitCacheLimits;
use tokio::sync::Semaphore;
use ulid::Ulid;

use super::SplitFilepath;

type LastAccessDate = u64;

/// Maximum number of splits to track.
const MAX_NUM_CANDIDATES: usize = 1_000;

/// Splits that are freshly reported get a last access time of `now - NEWLY_REPORT_SPLIT_LAST_TIME`.
const NEWLY_REPORTED_SPLIT_LAST_TIME: Duration = Duration::from_secs(60 * 10); // 10mn

#[derive(Clone, Copy)]
pub(crate) struct SplitKey {
    pub last_accessed: LastAccessDate,
    pub split_ulid: Ulid,
}

impl PartialOrd for SplitKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SplitKey {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.last_accessed, &self.split_ulid).cmp(&(other.last_accessed, &other.split_ulid))
    }
}

impl PartialEq for SplitKey {
    fn eq(&self, other: &Self) -> bool {
        (self.last_accessed, &self.split_ulid) == (other.last_accessed, &other.split_ulid)
    }
}

impl Eq for SplitKey {}

#[derive(Clone, Debug)]
enum Status {
    Candidate(CandidateSplit),
    Downloading { alive_token: Weak<()> },
    OnDisk { num_bytes: u64 },
}

impl PartialEq for Status {
    fn eq(&self, other: &Status) -> bool {
        match (self, other) {
            (Status::Candidate(candidate_split), Status::Candidate(other_candidate_split)) => {
                candidate_split == other_candidate_split
            }
            (Status::Downloading { .. }, Status::Downloading { .. }) => true,
            (
                Status::OnDisk { num_bytes },
                Status::OnDisk {
                    num_bytes: other_num_bytes,
                },
            ) => num_bytes == other_num_bytes,
            _ => false,
        }
    }
}

pub struct SplitInfo {
    pub(crate) split_key: SplitKey,
    status: Status,
}

/// The split table keeps track of splits we know about (regardless of whether they have already
/// been downloaded or not).
///
/// Invariant:
/// Each split appearing into split_to_status, should be listed 1 and exactly once in the
/// either
/// - on_disk_splits
/// - downloading_splits
/// - candidate_splits.
///
/// It is possible for the split table to exceed its limits, by at most one split.
pub struct SplitTable {
    on_disk_splits: BTreeSet<SplitKey>,
    downloading_splits: BTreeSet<SplitKey>,
    candidate_splits: BTreeSet<SplitKey>,
    split_to_status: HashMap<Ulid, SplitInfo>,
    origin_time: Instant,
    limits: SplitCacheLimits,
    on_disk_bytes: u64,

    // Directory containing the cached split files.
    // Split ids are universally unique, so we all put them in the same directory.
    root_path: PathBuf,

    fd_semaphore: Arc<Semaphore>,
    fd_cache: lru::LruCache<Ulid, Arc<SplitFilepath>>,
}

impl SplitTable {
    pub(crate) fn with_limits_and_existing_splits(
        limits: SplitCacheLimits,
        existing_filepaths: BTreeMap<Ulid, u64>,
        root_path: PathBuf,
    ) -> SplitTable {
        let origin_time = Instant::now() - NEWLY_REPORTED_SPLIT_LAST_TIME;
        let mut split_table = SplitTable {
            on_disk_splits: BTreeSet::default(),
            candidate_splits: BTreeSet::default(),
            downloading_splits: BTreeSet::default(),
            split_to_status: HashMap::default(),
            origin_time,
            limits,
            on_disk_bytes: 0u64,

            fd_semaphore: Arc::new(Semaphore::new(200)),
            fd_cache: lru::LruCache::new(NonZeroUsize::new(100).unwrap()),
            root_path,
        };
        split_table.acknowledge_on_disk_splits(existing_filepaths);
        split_table
    }

    /// Returns a File object for the given split if it is within the local
    /// searcher split cache (None if absent).
    ///
    /// This method ensure that the number of file descriptors opened at ounce is
    /// bounded using a semaphore.
    ///
    /// In order to avoid opening and closing files too many times, the split table also
    /// includes a cache of file descriptors.
    pub async fn try_get_or_open_fd(
        &mut self,
        split_id: Ulid,
        storage_uri: &Uri,
    ) -> Option<Arc<SplitFilepath>> {
        // TODO when do we have cache eviction?
        // do we have it for both caches?
        if let Some(split_filepath) = self.fd_cache.get(&split_id) {
            return Some(split_filepath.clone());
        }
        let num_bytes: u64 = self.is_on_disk(split_id, storage_uri)?;
        let fd_guard = Semaphore::acquire_owned(self.fd_semaphore.clone())
            .await
            .unwrap();
        let split_filename = quickwit_common::split_file(split_id);
        let cache_split_file_path = self.root_path.join(split_filename);
        let file = std::fs::File::open(cache_split_file_path).unwrap();
        let split_filepath = Arc::new(SplitFilepath {
            file,
            num_bytes,
            _fd_guard: fd_guard,
        });
        self.fd_cache.put(split_id, split_filepath.clone());
        Some(split_filepath)
    }

    fn acknowledge_on_disk_splits(&mut self, existing_filepaths: BTreeMap<Ulid, u64>) {
        for (split_ulid, num_bytes) in existing_filepaths {
            let split_info = SplitInfo {
                split_key: SplitKey {
                    last_accessed: 0,
                    split_ulid,
                },
                status: Status::OnDisk { num_bytes },
            };
            self.insert(split_info);
        }
    }
}

fn compute_timestamp(start: Instant) -> LastAccessDate {
    start.elapsed().as_micros() as u64
}

impl SplitTable {
    fn is_on_disk(&mut self, split_ulid: Ulid, storage_uri: &Uri) -> Option<u64> {
        if let Status::OnDisk { num_bytes } = self.touch(split_ulid, storage_uri) {
            Some(num_bytes)
        } else {
            None
        }
    }

    fn remove(&mut self, split_ulid: Ulid) -> Option<SplitInfo> {
        self.fd_cache.pop(&split_ulid);
        let split_info = self.split_to_status.remove(&split_ulid)?;
        let split_queue: &mut BTreeSet<SplitKey> = match split_info.status {
            Status::Candidate { .. } => &mut self.candidate_splits,
            Status::Downloading { .. } => &mut self.downloading_splits,
            Status::OnDisk { num_bytes } => {
                self.on_disk_bytes -= num_bytes;
                crate::metrics::STORAGE_METRICS
                    .searcher_split_cache
                    .in_cache_count
                    .dec();
                crate::metrics::STORAGE_METRICS
                    .searcher_split_cache
                    .in_cache_num_bytes
                    .sub(num_bytes as i64);
                &mut self.on_disk_splits
            }
        };
        let is_in_queue = split_queue.remove(&split_info.split_key);
        assert!(is_in_queue);
        if let Status::Downloading { alive_token } = &split_info.status {
            if alive_token.strong_count() == 0 {
                return None;
            }
        }
        Some(split_info)
    }

    fn gc_downloading_splits_if_necessary(&mut self) {
        if self.downloading_splits.len()
            < (self.limits.num_concurrent_downloads.get() as usize + 10)
        {
            return;
        }
        let mut splits_to_remove = Vec::new();
        for split in &self.downloading_splits {
            if let Some(split_info) = self.split_to_status.get(&split.split_ulid) {
                if let Status::Downloading { alive_token } = &split_info.status {
                    if alive_token.strong_count() == 0 {
                        splits_to_remove.push(split.split_ulid);
                    }
                }
            }
        }
        for split in splits_to_remove {
            self.remove(split);
        }
    }

    /// Insert a `split_info`. This methods assumes the split was not present in the split table
    /// to begin with. It will panic if the split was already present.
    ///
    /// Keep this method private.
    fn insert(&mut self, split_info: SplitInfo) {
        let was_not_in_queue = match split_info.status {
            Status::Candidate { .. } => {
                let was_not_in_queue = self.candidate_splits.insert(split_info.split_key);
                self.truncate_candidate_list();
                was_not_in_queue
            }
            Status::Downloading { .. } => self.downloading_splits.insert(split_info.split_key),
            Status::OnDisk { num_bytes } => {
                self.on_disk_bytes += num_bytes;
                crate::metrics::STORAGE_METRICS
                    .searcher_split_cache
                    .in_cache_count
                    .inc();
                crate::metrics::STORAGE_METRICS
                    .searcher_split_cache
                    .in_cache_num_bytes
                    .add(num_bytes as i64);
                self.on_disk_splits.insert(split_info.split_key)
            }
        };
        self.gc_downloading_splits_if_necessary();
        assert!(was_not_in_queue);
        let split_ulid_was_absent = self
            .split_to_status
            .insert(split_info.split_key.split_ulid, split_info)
            .is_none();
        assert!(split_ulid_was_absent);
    }

    fn touch(&mut self, split_ulid: Ulid, storage_uri: &Uri) -> Status {
        let timestamp = compute_timestamp(self.origin_time);
        self.mutate_split(split_ulid, |old_split_info| {
            if let Some(mut split_info) = old_split_info {
                split_info.split_key.last_accessed = timestamp;
                split_info
            } else {
                // We did not know anything about this split.
                // Let's add it to the table.
                SplitInfo {
                    split_key: SplitKey {
                        split_ulid,
                        last_accessed: timestamp,
                    },
                    status: Status::Candidate(CandidateSplit {
                        storage_uri: storage_uri.clone(),
                        split_ulid,
                        living_token: Arc::new(()),
                    }),
                }
            }
        })
    }

    /// Mutates a split ulid.
    ///
    /// By design this function maintains the invariant.
    /// It removes the split with the given ulid, modifies, and re
    fn mutate_split(
        &mut self,
        split_ulid: Ulid,
        mutate_fn: impl FnOnce(Option<SplitInfo>) -> SplitInfo,
    ) -> Status {
        let split_info_opt = self.remove(split_ulid);
        let new_split: SplitInfo = mutate_fn(split_info_opt);
        let new_status = new_split.status.clone();
        self.insert(new_split);
        new_status
    }

    fn change_split_status(&mut self, split_ulid: Ulid, status: Status) {
        let start_time = self.origin_time;
        self.mutate_split(split_ulid, move |split_info_opt| {
            if let Some(mut split_info) = split_info_opt {
                split_info.status = status;
                split_info
            } else {
                SplitInfo {
                    split_key: SplitKey {
                        last_accessed: Instant::now(),
                        split_ulid,
                    },
                    status,
                }
            }
        });
    }

    pub(crate) fn report(&mut self, split_ulid: Ulid, storage_uri: Uri) {
        let origin_time = self.origin_time;
        self.mutate_split(split_ulid, move |split_info_opt| {
            if let Some(split_info) = split_info_opt {
                return split_info;
            }
            SplitInfo {
                split_key: SplitKey {
                    last_accessed: Instant::now()
                        .checked_sub(NEWLY_REPORTED_SPLIT_LAST_TIME)
                        .unwrap_or(origin_time),
                    split_ulid,
                },
                status: Status::Candidate(CandidateSplit {
                    storage_uri,
                    split_ulid,
                    living_token: Arc::new(()),
                }),
            }
        });
    }

    /// Make sure we have at most `MAX_CANDIDATES` candidate splits.
    fn truncate_candidate_list(&mut self) {
        while self.candidate_splits.len() > MAX_NUM_CANDIDATES {
            let worst_candidate = self.candidate_splits.first().unwrap().split_ulid;
            self.remove(worst_candidate);
        }
    }

    pub(crate) fn register_as_downloaded(&mut self, split_ulid: Ulid, num_bytes: u64) {
        self.change_split_status(split_ulid, Status::OnDisk { num_bytes });
    }

    /// Change the state of the given split from candidate to downloading state,
    /// and returns its URI.
    ///
    /// This function does NOT trigger the download itself. It is up to
    /// the caller to actually initiate the download.
    pub(crate) fn start_download(&mut self, split_ulid: Ulid) -> Option<CandidateSplit> {
        let split_info = self.remove(split_ulid)?;
        let Status::Candidate(candidate_split) = split_info.status else {
            self.insert(split_info);
            return None;
        };
        let alive_token = Arc::downgrade(&candidate_split.living_token);
        self.insert(SplitInfo {
            split_key: split_info.split_key,
            status: Status::Downloading { alive_token },
        });
        Some(candidate_split)
    }

    fn best_candidate(&self) -> Option<SplitKey> {
        self.candidate_splits.last().copied()
    }

    fn is_out_of_limits(&self) -> bool {
        if self.on_disk_splits.is_empty() {
            return false;
        }
        if self.on_disk_splits.len() + self.downloading_splits.len()
            > self.limits.max_num_splits.get() as usize
        {
            return true;
        }
        if self.on_disk_bytes > self.limits.max_num_bytes.as_u64() {
            return true;
        }
        false
    }

    /// Evicts splits to reach the target limits.
    ///
    /// Returns false if the first candidate for eviction is
    /// fresher that the candidate split. (Note this is suboptimal.
    ///
    /// Returns `None` if this would mean evicting splits that
    /// have been accessed more recently than the candidate split.
    pub(crate) fn make_room_for_split_if_necessary(
        &mut self,
        last_access_date_bound_opt: Option<LastAccessDate>,
    ) -> Option<Vec<Ulid>> {
        let mut split_infos = Vec::new();
        while self.is_out_of_limits() {
            if let Some(first_split) = self.on_disk_splits.first() {
                if let Some(last_access_date) = last_access_date_bound_opt {
                    if first_split.last_accessed > last_access_date {
                        // This is not worth doing the eviction.
                        break;
                    }
                }
                split_infos.extend(self.remove(first_split.split_ulid));
            } else {
                break;
            }
        }
        if self.is_out_of_limits() {
            // We are still out of limits.
            // Let's not go through with the eviction, and reinsert the splits.
            for split_info in split_infos {
                self.insert(split_info);
            }
            None
        } else {
            Some(
                split_infos
                    .into_iter()
                    .map(|split_info| split_info.split_key.split_ulid)
                    .collect(),
            )
        }
    }

    pub(crate) fn find_download_opportunity(&mut self) -> Option<DownloadOpportunity> {
        let best_candidate_split_key = self.best_candidate()?;
        let splits_to_delete: Vec<Ulid> =
            self.make_room_for_split_if_necessary(Some(best_candidate_split_key.last_accessed))?;
        let split_to_download: CandidateSplit =
            self.start_download(best_candidate_split_key.split_ulid)?;
        Some(DownloadOpportunity {
            splits_to_delete,
            split_to_download,
        })
    }

    #[cfg(test)]
    pub fn num_bytes(&self) -> u64 {
        self.on_disk_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CandidateSplit {
    pub storage_uri: Uri,
    pub split_ulid: Ulid,
    pub living_token: Arc<()>,
}

pub(crate) struct DownloadOpportunity {
    // At this point, the split have already been removed from the split table.
    // The file however need to be deleted.
    pub splits_to_delete: Vec<Ulid>,
    pub split_to_download: CandidateSplit,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use bytesize::ByteSize;
    use quickwit_common::uri::Uri;
    use quickwit_config::SplitCacheLimits;
    use ulid::Ulid;

    use crate::split_cache::split_table::{DownloadOpportunity, SplitTable};

    const TEST_STORAGE_URI: &str = "s3://test";

    fn sorted_split_ulids(num_splits: usize) -> Vec<Ulid> {
        let mut split_ulids: Vec<Ulid> =
            std::iter::repeat_with(Ulid::new).take(num_splits).collect();
        split_ulids.sort();
        split_ulids
    }

    #[test]
    fn test_split_table() {
        let mut split_table = SplitTable::with_limits_and_existing_splits(
            SplitCacheLimits {
                max_num_bytes: ByteSize::kb(1),
                max_num_splits: NonZeroU32::new(1).unwrap(),
                num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            },
            Default::default(),
        );
        let ulids = sorted_split_ulids(2);
        let ulid1 = ulids[0];
        let ulid2 = ulids[1];
        split_table.report(ulid1, Uri::for_test(TEST_STORAGE_URI));
        split_table.report(ulid2, Uri::for_test(TEST_STORAGE_URI));
        let candidate = split_table.best_candidate().unwrap();
        assert_eq!(candidate.split_ulid, ulid2);
    }

    #[test]
    fn test_split_table_prefer_last_touched() {
        let mut split_table = SplitTable::with_limits_and_existing_splits(
            SplitCacheLimits {
                max_num_bytes: ByteSize::kb(1),
                max_num_splits: NonZeroU32::new(1).unwrap(),
                num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            },
            Default::default(),
        );
        let ulids = sorted_split_ulids(2);
        let ulid1 = ulids[0];
        let ulid2 = ulids[1];
        split_table.report(ulid1, Uri::for_test(TEST_STORAGE_URI));
        split_table.report(ulid2, Uri::for_test(TEST_STORAGE_URI));
        let split_guard_opt = split_table.get_split_guard(ulid1, &Uri::for_test("s3://test1/"));
        assert!(split_guard_opt.is_none());
        let candidate = split_table.best_candidate().unwrap();
        assert_eq!(candidate.split_ulid, ulid1);
    }

    #[test]
    fn test_split_table_prefer_start_download_prevent_new_report() {
        let mut split_table = SplitTable::with_limits_and_existing_splits(
            SplitCacheLimits {
                max_num_bytes: ByteSize::kb(1),
                max_num_splits: NonZeroU32::new(1).unwrap(),
                num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            },
            Default::default(),
        );
        let ulid1 = Ulid::new();
        split_table.report(ulid1, Uri::for_test(TEST_STORAGE_URI));
        assert_eq!(split_table.num_bytes(), 0);
        let download = split_table.start_download(ulid1);
        assert!(download.is_some());
        assert!(split_table.start_download(ulid1).is_none());
        split_table.register_as_downloaded(ulid1, 10_000_000);
        assert_eq!(split_table.num_bytes(), 10_000_000);
        split_table.get_split_guard(ulid1, &Uri::for_test(TEST_STORAGE_URI));
        let ulid2 = Ulid::new();
        split_table.report(ulid2, Uri::for_test("s3://test`/"));
        let download = split_table.start_download(ulid2);
        assert!(download.is_some());
        assert!(split_table.start_download(ulid2).is_none());
        assert_eq!(split_table.num_bytes(), 10_000_000);
        split_table.register_as_downloaded(ulid2, 3_000_000);
        assert_eq!(split_table.num_bytes(), 13_000_000);
    }

    #[test]
    fn test_eviction_due_to_size() {
        let mut split_table = SplitTable::with_limits_and_existing_splits(
            SplitCacheLimits {
                max_num_bytes: ByteSize::mb(1),
                max_num_splits: NonZeroU32::new(30).unwrap(),
                num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            },
            Default::default(),
        );
        let mut split_ulids: Vec<Ulid> = std::iter::repeat_with(Ulid::new).take(6).collect();
        split_ulids.sort();
        let splits = [
            (split_ulids[0], 10_000),
            (split_ulids[1], 20_000),
            (split_ulids[2], 300_000),
            (split_ulids[3], 400_000),
            (split_ulids[4], 100_000),
            (split_ulids[5], 300_000),
        ];
        for (split_ulid, num_bytes) in splits {
            split_table.report(split_ulid, Uri::for_test(TEST_STORAGE_URI));
            split_table.register_as_downloaded(split_ulid, num_bytes);
        }
        let new_ulid = Ulid::new();
        split_table.report(new_ulid, Uri::for_test(TEST_STORAGE_URI));
        let DownloadOpportunity {
            splits_to_delete,
            split_to_download,
        } = split_table.find_download_opportunity().unwrap();
        assert_eq!(
            &splits_to_delete[..],
            &[splits[0].0, splits[1].0, splits[2].0][..]
        );
        assert_eq!(split_to_download.split_ulid, new_ulid);
    }

    #[test]
    fn test_eviction_due_to_num_splits() {
        let mut split_table = SplitTable::with_limits_and_existing_splits(
            SplitCacheLimits {
                max_num_bytes: ByteSize::mb(10),
                max_num_splits: NonZeroU32::new(5).unwrap(),
                num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            },
            Default::default(),
        );
        let mut split_ulids: Vec<Ulid> = std::iter::repeat_with(Ulid::new).take(6).collect();
        split_ulids.sort();
        let splits = [
            (split_ulids[0], 10_000),
            (split_ulids[1], 20_000),
            (split_ulids[2], 300_000),
            (split_ulids[3], 400_000),
            (split_ulids[4], 100_000),
            (split_ulids[5], 300_000),
        ];
        for (split_ulid, num_bytes) in splits {
            split_table.report(split_ulid, Uri::for_test(TEST_STORAGE_URI));
            split_table.register_as_downloaded(split_ulid, num_bytes);
        }
        let new_ulid = Ulid::new();
        split_table.report(new_ulid, Uri::for_test(TEST_STORAGE_URI));
        let DownloadOpportunity {
            splits_to_delete,
            split_to_download,
        } = split_table.find_download_opportunity().unwrap();
        assert_eq!(&splits_to_delete[..], &[splits[0].0][..]);
        assert_eq!(split_to_download.split_ulid, new_ulid);
    }

    #[test]
    fn test_failed_download_can_be_re_reported() {
        let mut split_table = SplitTable::with_limits_and_existing_splits(
            SplitCacheLimits {
                max_num_bytes: ByteSize::mb(10),
                max_num_splits: NonZeroU32::new(5).unwrap(),
                num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            },
            Default::default(),
        );
        let split_ulid = Ulid::new();
        split_table.report(split_ulid, Uri::for_test(TEST_STORAGE_URI));
        let candidate = split_table.start_download(split_ulid).unwrap();
        // This report should be cancelled as we have a download currently running.
        split_table.report(split_ulid, Uri::for_test(TEST_STORAGE_URI));

        assert!(split_table.start_download(split_ulid).is_none());
        std::mem::drop(candidate);

        // Still not possible to start a download.
        assert!(split_table.start_download(split_ulid).is_none());

        // This report should be considered as our candidate (and its alive token has been dropped)
        split_table.report(split_ulid, Uri::for_test(TEST_STORAGE_URI));

        let candidate2 = split_table.start_download(split_ulid).unwrap();
        assert_eq!(candidate2.split_ulid, split_ulid);
    }

    #[test]
    fn test_split_table_truncate_candidates() {
        let mut split_table = SplitTable::with_limits_and_existing_splits(
            SplitCacheLimits {
                max_num_bytes: ByteSize::mb(10),
                max_num_splits: NonZeroU32::new(5).unwrap(),
                num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            },
            Default::default(),
        );
        for i in 1..2_000 {
            let split_ulid = Ulid::new();
            split_table.report(split_ulid, Uri::for_test(TEST_STORAGE_URI));
            assert_eq!(
                split_table.candidate_splits.len(),
                i.min(super::MAX_NUM_CANDIDATES)
            );
        }
    }
}
