// Copyright 2016 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Small filesystem helpers and the test-only large-allocation tracker.

use std::fs::{File, Metadata};
use std::path::Path;
use std::time::SystemTime;

use fs2::FileExt;
use log::warn;

#[cfg(target_family = "unix")]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[cfg(test)]
use std::alloc::{GlobalAlloc, Layout, System};
#[cfg(test)]
use std::cell::Cell;

#[cfg(target_family = "unix")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileChangeCookie {
    device_id: u64,
    inode: u64,
    change_seconds: i64,
    change_nanoseconds: i64,
}

pub(crate) fn get_mod_time<P: AsRef<Path>>(path: P) -> Option<SystemTime> {
    File::open(path).and_then(|f| f.metadata()).ok().and_then(|meta| mod_time_from_metadata(&meta))
}

pub(crate) fn mod_time_from_metadata(meta: &Metadata) -> Option<SystemTime> {
    meta.modified().ok()
}

pub(crate) fn get_file_len<P: AsRef<Path>>(path: P) -> Option<u64> {
    File::open(path).and_then(|f| f.metadata()).map(|meta| meta.len()).ok()
}

#[cfg(target_family = "unix")]
pub(crate) fn get_change_cookie<P: AsRef<Path>>(path: P) -> Option<FileChangeCookie> {
    File::open(path).and_then(|f| f.metadata()).ok().map(|meta| change_cookie_from_metadata(&meta))
}

#[cfg(target_family = "unix")]
pub(crate) fn change_cookie_from_metadata(meta: &Metadata) -> FileChangeCookie {
    FileChangeCookie {
        device_id: meta.dev(),
        inode: meta.ino(),
        change_seconds: meta.ctime(),
        change_nanoseconds: meta.ctime_nsec(),
    }
}

pub(crate) fn open_advisory_lock(path: &Path) -> Option<File> {
    File::open(path).ok().and_then(|lf| match lf.try_lock_exclusive() {
        Ok(()) => Some(lf),
        Err(e) => {
            warn!("Could not lock newly saved file {:?}: {}", path, e);
            None
        }
    })
}

/// Returns the file permissions for the file at a given path on UNIXy systems,
/// if present.
#[cfg(target_family = "unix")]
pub(crate) fn get_permissions<P: AsRef<Path>>(path: P) -> Option<u32> {
    File::open(path).and_then(|f| f.metadata()).map(|meta| permissions_from_metadata(&meta)).ok()
}

#[cfg(target_family = "unix")]
pub(crate) fn permissions_from_metadata(meta: &Metadata) -> u32 {
    meta.permissions().mode()
}

#[cfg(test)]
struct TrackingAlloc;

#[cfg(test)]
thread_local! {
    static TRACK_ALLOC_THRESHOLD: Cell<usize> = const { Cell::new(0) };
    static TRACK_LARGE_ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
    static TRACK_LARGEST_ALLOC: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
#[global_allocator]
static GLOBAL_ALLOCATOR: TrackingAlloc = TrackingAlloc;

#[cfg(test)]
#[inline]
fn record_large_alloc(layout: Layout) {
    TRACK_ALLOC_THRESHOLD.with(|threshold| {
        let threshold = threshold.get();
        if threshold == 0 || layout.size() < threshold {
            return;
        }
        TRACK_LARGE_ALLOC_COUNT.with(|count| count.set(count.get() + 1));
        TRACK_LARGEST_ALLOC.with(|largest| largest.set(largest.get().max(layout.size())));
    });
}

#[cfg(test)]
unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_large_alloc(layout);
        // SAFETY: delegated to the system allocator with the same layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: delegated to the system allocator with the same pair.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
pub(crate) fn with_large_alloc_tracking<T>(
    threshold: usize,
    f: impl FnOnce() -> T,
) -> (T, usize, usize) {
    TRACK_ALLOC_THRESHOLD.with(|value| value.set(threshold));
    TRACK_LARGE_ALLOC_COUNT.with(|value| value.set(0));
    TRACK_LARGEST_ALLOC.with(|value| value.set(0));
    let result = f();
    let alloc_count = TRACK_LARGE_ALLOC_COUNT.with(|value| value.get());
    let largest_alloc = TRACK_LARGEST_ALLOC.with(|value| value.get());
    TRACK_ALLOC_THRESHOLD.with(|value| value.set(0));
    (result, alloc_count, largest_alloc)
}
