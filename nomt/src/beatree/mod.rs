use allocator::{PageNumber, Store, StoreReader, FREELIST_EMPTY};
use anyhow::{Context, Result};
use branch::BRANCH_NODE_SIZE;
use parking_lot::{Mutex, RwLock};
use std::{collections::BTreeMap, fs::File, mem, ops::DerefMut, path::Path, sync::Arc};
use threadpool::ThreadPool;

use crate::io::{fsyncer::Fsyncer, IoHandle, IoPool, PagePool};

mod allocator;
mod branch;
mod index;
mod leaf;
mod ops;

mod writeout;
use index::Index;

#[cfg(feature = "benchmarks")]
pub mod benches;

pub type Key = [u8; 32];

#[derive(Clone)]
pub struct Tree {
    shared: Arc<RwLock<Shared>>,
    sync: Arc<Mutex<Sync>>,
    bbn_fsync: Arc<Fsyncer>,
    ln_fsync: Arc<Fsyncer>,
    sync_tp: ThreadPool,
}

struct Shared {
    page_pool: PagePool,
    io_handle: IoHandle,
    bbn_index: index::Index,
    leaf_store: Store,
    bbn_store: Store,
    leaf_store_rd: StoreReader,
    /// Primary staging collects changes that are committed but not synced yet. Upon sync, changes
    /// from here are moved to secondary staging.
    primary_staging: BTreeMap<Key, Option<Vec<u8>>>,
    /// Secondary staging collects committed changes that are currently being synced. This is None
    /// if there is no sync in progress.
    secondary_staging: Option<Arc<BTreeMap<Key, Option<Vec<u8>>>>>,
}

struct Sync {
    tp: ThreadPool,
    commit_concurrency: usize,
}

impl Shared {
    fn take_staged_changeset(&mut self) -> Arc<BTreeMap<Key, Option<Vec<u8>>>> {
        assert!(self.secondary_staging.is_none());
        let staged = Arc::new(mem::take(&mut self.primary_staging));
        self.secondary_staging = Some(staged.clone());
        staged
    }
}

impl Tree {
    pub fn open(
        page_pool: PagePool,
        io_pool: &IoPool,
        ln_freelist_pn: u32,
        bbn_freelist_pn: u32,
        ln_bump: u32,
        bbn_bump: u32,
        bbn_file: &File,
        ln_file: &File,
        commit_concurrency: usize,
    ) -> Result<Tree> {
        let ln_freelist_pn = Some(ln_freelist_pn)
            .map(PageNumber)
            .filter(|&x| x != FREELIST_EMPTY);
        let bbn_freelist_pn = Some(bbn_freelist_pn)
            .map(PageNumber)
            .filter(|&x| x != FREELIST_EMPTY);

        let ln_bump = PageNumber(ln_bump);
        let bbn_bump = PageNumber(bbn_bump);

        let leaf_store = Store::open(
            &page_pool,
            ln_file.try_clone().unwrap(),
            ln_bump,
            ln_freelist_pn,
        )?;

        let bbn_store = Store::open(
            &page_pool,
            bbn_file.try_clone().unwrap(),
            bbn_bump,
            bbn_freelist_pn,
        )?;

        let bbn_freelist_tracked = bbn_store.all_tracked_freelist_pages();
        let index = ops::reconstruct(
            bbn_file.try_clone().unwrap(),
            &page_pool,
            &bbn_freelist_tracked,
            bbn_bump,
        )
        .with_context(|| format!("failed to reconstruct btree from bbn store file"))?;
        let shared = Shared {
            io_handle: io_pool.make_handle(),
            page_pool: io_pool.page_pool().clone(),
            bbn_index: index,
            leaf_store_rd: StoreReader::new(leaf_store.clone(), io_pool.page_pool().clone()),
            leaf_store,
            bbn_store,
            primary_staging: BTreeMap::new(),
            secondary_staging: None,
        };

        let sync = Sync {
            tp: ThreadPool::with_name("beatree-sync".into(), commit_concurrency),
            commit_concurrency,
        };

        Ok(Tree {
            sync_tp: ThreadPool::with_name("beatree-sync".into(), 1),
            shared: Arc::new(RwLock::new(shared)),
            sync: Arc::new(Mutex::new(sync)),
            bbn_fsync: Arc::new(Fsyncer::new("bbn", bbn_file)),
            ln_fsync: Arc::new(Fsyncer::new("ln", ln_file)),
        })
    }

    /// Lookup a key in the btree.
    pub fn lookup(&self, key: Key) -> Option<Vec<u8>> {
        let shared = self.shared.read();

        // First look up in the primary staging which contains the most recent changes.
        if let Some(val) = shared.primary_staging.get(&key) {
            return val.clone();
        }

        // Then check the secondary staging which is a bit older, but fresher still than the btree.
        if let Some(val) = shared.secondary_staging.as_ref().and_then(|x| x.get(&key)) {
            return val.clone();
        }

        // Finally, look up in the btree.
        ops::lookup(key, &shared.bbn_index, &shared.leaf_store_rd).unwrap()
    }

    /// Returns a controller for the sync process.
    pub fn sync(&self) -> SyncController {
        SyncController {
            tree: self.clone(),
            tp: self.sync_tp.clone(),
            sync_data: Arc::new(Mutex::new(None)),
            bbn_index: Arc::new(Mutex::new(None)),
        }
    }

    /// Commit a set of changes to the btree.
    ///
    /// The changeset is a list of key value pairs to be added or removed from the btree.
    /// The changeset is applied atomically. If the changeset is empty, the btree is not modified.
    // There might be some temptation to unify this with prepare_sync, but this should not be done
    // because in the future sync and commit will be called on different threads at different times.
    fn commit(&self, changeset: Vec<(Key, Option<Vec<u8>>)>) {
        if changeset.is_empty() {
            return;
        }
        let mut inner = self.shared.write();
        let staging = &mut inner.primary_staging;
        for (key, value) in changeset {
            staging.insert(key, value);
        }
    }

    /// Dump all changes performed by commits to the underlying storage medium.
    ///
    /// Either blocks or panics if another sync is inflight.
    fn prepare_sync(&self) -> (SyncData, Index) {
        // Take the sync lock.
        //
        // That will exclude any other syncs from happening. This is a long running operation.
        //
        // Note the ordering of taking locks is important.
        let mut sync = self.sync.lock();

        // Take the shared lock. Briefly.
        let staged_changeset;
        let bbn_index;
        let page_pool;
        let leaf_store;
        let bbn_store;
        let io_handle;
        {
            let mut shared = self.shared.write();
            staged_changeset = shared.take_staged_changeset();
            bbn_index = shared.bbn_index.clone();
            page_pool = shared.page_pool.clone();
            leaf_store = shared.leaf_store.clone();
            bbn_store = shared.bbn_store.clone();
            io_handle = shared.io_handle.clone();
        }

        {
            let sync = sync.deref_mut();

            // Update will modify the index in a CoW manner.
            //
            // Nodes that need to be modified are not modified in place but they are
            // removed from the copy of the index,
            // and new ones (implying the one created as modification of existing nodes) are
            // allocated.
            //
            // Thus during the update:
            // + The index will just be modified, being a copy of the one used in parallel during lookups
            // + Allocation and releases of the leaf_store_wr will be executed normally,
            //   as everything will be in a pending state until commit
            // + All branch page releases will be performed at the end of the sync, when the old
            //   revision of the index is dropped.
            //   This makes it possible to keep the previous state of the tree (before this sync)
            //   available and reachable from the old index
            // + All necessary page writes will be issued to the store and their completion waited
            //   upon. However, these changes are not reflected until `finish_sync`.
            ops::update(
                staged_changeset.clone(),
                bbn_index,
                leaf_store,
                bbn_store,
                page_pool,
                io_handle,
                sync.tp.clone(),
                sync.commit_concurrency,
            )
            .unwrap()
        }
    }

    fn finish_sync(&self, bbn_index: Index) {
        // Take the shared lock again to complete the update to the new shared state
        let mut inner = self.shared.write();
        inner.secondary_staging = None;
        inner.bbn_index = bbn_index;
    }
}

/// Data generated during update
pub struct SyncData {
    pub ln_freelist_pn: u32,
    pub ln_bump: u32,
    pub bbn_freelist_pn: u32,
    pub bbn_bump: u32,
}

/// Creates the required files for the beatree.
pub fn create(db_dir: impl AsRef<Path>) -> anyhow::Result<()> {
    // Create the files.
    //
    // Size them to have an empty page at the beginning, this is reserved for the nil page.
    let ln_fd = File::create(db_dir.as_ref().join("ln"))?;
    let bbn_fd = File::create(db_dir.as_ref().join("bbn"))?;
    ln_fd.set_len(BRANCH_NODE_SIZE as u64)?;
    bbn_fd.set_len(BRANCH_NODE_SIZE as u64)?;

    // Sync files and the directory. I am not sure if syncing files is necessar, but it
    // is necessary to make sure that the directory is synced.
    ln_fd.sync_all()?;
    bbn_fd.sync_all()?;
    Ok(())
}

/// A handle that controls the sync process.
///
/// The order of the calls should always be:
///
/// 1. [`Self::begin_sync`] - Initiates an asynchronous sync operation.
/// 2. [`Self::wait_pre_meta`] - Blocks until the sync process completes and returns metadata.
///    The manifest can be updated after this call returns successfully.
/// 3. [`Self::post_meta`] - Finalizes the sync process by updating internal state.
///
/// # Thread Safety
///
/// This controller is designed to be used from a single thread. While the underlying operations
/// are thread-safe, the controller itself maintains state that requires calls to be made in sequence.
///
/// # Error Handling
///
/// If [`Self::wait_pre_meta`] returns an error, the sync process has failed and the controller
/// should be discarded.
// TODO: error handling is coming in a follow up.
pub struct SyncController {
    tree: Tree,
    tp: ThreadPool,
    sync_data: Arc<Mutex<Option<SyncData>>>,
    bbn_index: Arc<Mutex<Option<Index>>>,
}

impl SyncController {
    /// Begins the sync process.
    ///
    /// Accepts a list of changes to be committed to the btree.
    ///
    /// Non-blocking.
    pub fn begin_sync(&mut self, changeset: Vec<(Key, Option<Vec<u8>>)>) {
        let beatree = self.tree.clone();
        let sync_data = self.sync_data.clone();
        let bbn_index = self.bbn_index.clone();
        self.tp.execute(move || {
            beatree.commit(changeset);
            let (out_meta, out_bbn_index) = beatree.prepare_sync();

            let mut sync_data = sync_data.lock();
            *sync_data = Some(out_meta);
            drop(sync_data);

            let mut bbn_index = bbn_index.lock();
            *bbn_index = Some(out_bbn_index);
            drop(bbn_index);

            beatree.bbn_fsync.fsync();
            beatree.ln_fsync.fsync();
        });
    }

    /// Waits for the writes to the tree to be synced to disk which allows the caller to proceed
    /// with updating the manifest.
    ///
    /// This must be called after [`Self::begin_sync`].
    pub fn wait_pre_meta(&mut self) -> anyhow::Result<SyncData> {
        self.tree.bbn_fsync.wait()?;
        self.tree.ln_fsync.wait()?;

        // UNWRAP: fsync of bbn and ln above ensures that sync_data is Some.
        let sync_data = self.sync_data.lock().take().unwrap();
        Ok(sync_data)
    }

    /// Finishes sync.
    ///
    /// Has to be called after the manifest is updated. Must be invoked by the sync
    /// thread. Blocking.
    pub fn post_meta(&mut self) {
        let bbn_index = self.bbn_index.lock().take().unwrap();
        self.tree.finish_sync(bbn_index);
    }
}
