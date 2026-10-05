use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use bitvec::{boxed::BitBox, order::Msb0, slice::BitSlice, vec::BitVec};
use tracing::debug_span;

use crate::{spawn_utils::BlockingSpawner, storage::filesystem::OurFileExt};

pub trait BitV: Send + Sync {
    fn as_slice(&self) -> &BitSlice<u8, Msb0>;
    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0>;
    fn into_dyn(self) -> Box<dyn BitV>;
    fn as_bytes(&self) -> &[u8];
    fn flush(&mut self, flush_async: bool) -> anyhow::Result<()>;
}

pub type BoxBitV = Box<dyn BitV>;

pub struct DiskBackedBitV {
    bv: BitBox<u8, Msb0>,
    // Progress snapshots supersede each other. Retain only the latest pending
    // snapshot if disk writes or the blocking-I/O semaphore stall.
    flush_tx: tokio::sync::watch::Sender<Option<Arc<BitBox<u8, Msb0>>>>,
}

impl Drop for DiskBackedBitV {
    fn drop(&mut self) {
        if self.queue_flush().is_err() {
            tracing::warn!("error flushing bitv on drop: flusher task is dead")
        }
    }
}

// NOTE on mmap. rqbit used it for a while, but it has issues on slow disks.
// We want writes to bitv to be instant in RAM. However when disk is slow, occasionally
// the writes stall which blocks the executor.
// Thus this separate "thread" of flushing was implemented.
impl DiskBackedBitV {
    pub async fn new(filename: PathBuf, spawner: BlockingSpawner) -> anyhow::Result<Self> {
        let buf = tokio::fs::read(&filename)
            .await
            .with_context(|| format!("error reading {filename:?}"))?;
        let bv = BitVec::from_vec(buf).into_boxed_bitslice();

        // blocking file to avoid double-buffering and double-memcpy
        let file = spawner
            .block_in_place_with_semaphore(|| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(false)
                    .open(&filename)
            })
            .await
            .with_context(|| format!("error opening {filename:?}"))?;

        let (tx, mut rx) = tokio::sync::watch::channel::<Option<Arc<BitBox<u8, Msb0>>>>(None);
        librqbit_core::spawn_utils::spawn(
            debug_span!("diskbitv-flusher", ?filename),
            format!("DiskBackedBitV::flusher {filename:?}"),
            async move {
                while rx.changed().await.is_ok() {
                    // Release the watch borrow before awaiting disk I/O, so
                    // publishers can replace a pending snapshot immediately.
                    let snapshot = rx.borrow_and_update().clone();
                    let Some(snapshot) = snapshot else {
                        continue;
                    };

                    if let Err(e) = spawner
                        .block_in_place_with_semaphore(|| {
                            file.pwrite_all(0, snapshot.as_raw_slice())
                        })
                        .await
                    {
                        tracing::error!(?filename, "error writing to bitv: {e:#}");
                        if let Err(e) = tokio::fs::remove_file(&filename).await {
                            tracing::error!(?filename, "error removing bitv: {e:#}");
                        }
                        break;
                    }

                    if let Err(e) = spawner
                        .block_in_place_with_semaphore(|| file.sync_all())
                        .await
                    {
                        tracing::error!(?filename, "error fsyncing bitv: {e:#}");
                    }
                }

                Ok::<_, anyhow::Error>(())
            },
        );
        Ok(Self { bv, flush_tx: tx })
    }

    fn queue_flush(&self) -> anyhow::Result<()> {
        self.flush_tx
            .send(Some(Arc::new(self.bv.clone())))
            .context("flusher task is dead")
    }
}

#[async_trait::async_trait]
impl BitV for BitBox<u8, Msb0> {
    fn as_slice(&self) -> &BitSlice<u8, Msb0> {
        self.as_bitslice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0> {
        self.as_mut_bitslice()
    }

    fn as_bytes(&self) -> &[u8] {
        self.as_raw_slice()
    }

    fn flush(&mut self, _flush_async: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }
}

impl BitV for DiskBackedBitV {
    fn as_slice(&self) -> &BitSlice<u8, Msb0> {
        self.bv.as_bitslice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0> {
        self.bv.as_mut_bitslice()
    }

    fn as_bytes(&self) -> &[u8] {
        self.bv.as_raw_slice()
    }

    fn flush(&mut self, _flush_async: bool) -> anyhow::Result<()> {
        self.queue_flush()
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }
}

impl BitV for Box<dyn BitV> {
    fn as_slice(&self) -> &BitSlice<u8, Msb0> {
        (**self).as_slice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0> {
        (**self).as_slice_mut()
    }

    fn as_bytes(&self) -> &[u8] {
        (**self).as_bytes()
    }

    fn flush(&mut self, flush_async: bool) -> anyhow::Result<()> {
        (**self).flush(flush_async)
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc, time::Duration};

    use super::{BitV, DiskBackedBitV};
    use crate::spawn_utils::BlockingSpawner;

    async fn wait_for_saved_bytes(filename: &Path, expected: &[u8]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tokio::fs::read(filename).await.unwrap() == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("flusher did not persist the final snapshot");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_stalled_disk_io_retains_only_inflight_and_latest_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let filename = directory.path().join("progress.bitv");
        let bitmap_bytes = 256 * 1024;
        std::fs::write(&filename, vec![0; bitmap_bytes]).unwrap();
        let spawner = BlockingSpawner::new(1);
        let mut bitv = DiskBackedBitV::new(filename.clone(), spawner.clone())
            .await
            .unwrap();

        // Hold all blocking-I/O permits after opening the file. This reproduces
        // a flusher stalled behind download writes, without timing a slow disk.
        let permit = spawner.semaphore().acquire_owned().await.unwrap();
        bitv.bv.as_raw_mut_slice().fill(1);
        bitv.flush(true).unwrap();
        let inflight = Arc::downgrade(bitv.flush_tx.borrow().as_ref().unwrap());
        tokio::time::timeout(Duration::from_secs(5), async {
            while inflight.strong_count() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("flusher did not select the first snapshot");

        for version in 0..1024 {
            let previous = Arc::downgrade(bitv.flush_tx.borrow().as_ref().unwrap());
            bitv.bv.as_raw_mut_slice().fill(version as u8);
            // Both call modes feed the same ordered, coalescing persistence path.
            bitv.flush(version % 2 == 0).unwrap();
            if version > 0 {
                assert!(
                    previous.upgrade().is_none(),
                    "obsolete snapshot was retained"
                );
            }
        }
        assert_eq!(inflight.strong_count(), 1);
        assert_eq!(
            Arc::strong_count(bitv.flush_tx.borrow().as_ref().unwrap()),
            1
        );

        // Drop must replace pending progress with this last mutation even while
        // the first write is stalled. Closing the sender must still deliver it.
        bitv.bv.as_raw_mut_slice().fill(0xa5);
        drop(bitv);
        drop(permit);
        wait_for_saved_bytes(&filename, &vec![0xa5; bitmap_bytes]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_dropping_bitv_persists_changes_without_an_explicit_flush() {
        let directory = tempfile::tempdir().unwrap();
        let filename = directory.path().join("progress.bitv");
        let mut expected = vec![0; 4096];
        std::fs::write(&filename, &expected).unwrap();
        let mut bitv = DiskBackedBitV::new(filename.clone(), BlockingSpawner::new(1))
            .await
            .unwrap();
        expected[17] = 0x81;
        bitv.bv.as_raw_mut_slice()[17] = expected[17];
        drop(bitv);

        wait_for_saved_bytes(&filename, &expected).await;
    }

    #[tokio::test]
    async fn memory_snapshots_coalesce_before_flusher_runs_on_current_thread() {
        let directory = tempfile::tempdir().unwrap();
        let filename = directory.path().join("progress.bitv");
        let mut expected = vec![0; 4096];
        std::fs::write(&filename, &expected).unwrap();
        let mut bitv = DiskBackedBitV::new(filename.clone(), BlockingSpawner::new(1))
            .await
            .unwrap();

        // No await in this loop: the single-threaded runtime cannot run the
        // flusher until all snapshots have been published and the sender closes.
        for version in 0..4096 {
            let previous = bitv.flush_tx.borrow().as_ref().map(Arc::downgrade);
            expected.fill(version as u8);
            bitv.bv.as_raw_mut_slice().copy_from_slice(&expected);
            bitv.flush(true).unwrap();
            if let Some(previous) = previous {
                assert!(previous.upgrade().is_none());
            }
        }
        expected[0] = 0;
        bitv.bv.as_raw_mut_slice()[0] = 0;
        drop(bitv);

        wait_for_saved_bytes(&filename, &expected).await;
    }
}
