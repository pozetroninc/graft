use std::{collections::HashMap, fmt::Debug, sync::Arc};

use crate::core::{VolumeId, lsn::LSN};
use futures::stream::FuturesUnordered;
use tokio::time::Interval;
use tokio_stream::StreamExt;
use tryiter::TryIteratorExt;

use crate::{
    GraftErr,
    local::fjall_storage::FjallStorage,
    remote::Remote,
    rt::{
        action::{Action, FetchLog, RemoteCommit},
        task::{Result, Task},
    },
};

pub struct AutosyncTask {
    ticker: Interval,
}

impl AutosyncTask {
    pub fn new(ticker: Interval) -> Self {
        Self { ticker }
    }
}

impl Debug for AutosyncTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutosyncTask")
            .field("interval", &self.ticker.period())
            .finish()
    }
}

impl Task for AutosyncTask {
    const NAME: &'static str = "autosync";

    async fn run(&mut self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<()> {
        loop {
            // wait for the next tick
            self.ticker.tick().await;

            enum Subtask {
                Push { vid: VolumeId },
                Pull { vid: VolumeId },
            }

            // Map of LogId -> leaf_hash_min_lsn for trust boundary enforcement.
            // When multiple volumes share a log, use the earliest (most restrictive) boundary.
            let mut fetches: HashMap<_, Option<LSN>> = HashMap::new();
            // a set of actions to execute
            let mut actions = vec![];

            // collect actions
            {
                let reader = storage.read();
                let mut volumes = reader.iter_volumes().map_err(GraftErr::from);
                while let Some(volume) = volumes.try_next()? {
                    let latest_local = reader.latest_lsn(&volume.local)?;
                    let latest_remote = reader.latest_lsn(&volume.remote)?;
                    let local_changes = volume.local_changes(latest_local).is_some();
                    let remote_changes = volume.remote_changes(latest_remote).is_some();

                    if remote_changes && local_changes {
                        // volume has diverged and requires user/app intervention
                    } else if remote_changes {
                        actions.push(Subtask::Pull { vid: volume.vid })
                    } else if local_changes {
                        let entry = fetches.entry(volume.remote).or_insert(None);
                        *entry = merge_min_lsn(*entry, volume.leaf_hash_min_lsn);
                        actions.push(Subtask::Push { vid: volume.vid })
                    } else {
                        let entry = fetches.entry(volume.remote).or_insert(None);
                        *entry = merge_min_lsn(*entry, volume.leaf_hash_min_lsn);
                        actions.push(Subtask::Pull { vid: volume.vid });
                    }
                }
            }

            // execute all scheduled fetches
            let mut futures: FuturesUnordered<_> = fetches
                .into_iter()
                .map(|(log, leaf_hash_min_lsn)| {
                    FetchLog {
                        log,
                        max_lsn: None,
                        leaf_hash_min_lsn,
                    }
                    .run(storage.clone(), remote.clone())
                })
                .collect();
            while let Some(result) = futures.next().await {
                if let Err(err) = result {
                    tracing::error!("Autosync fetch failed: {:?}", err);
                }
            }

            // execute all scheduled actions
            let mut futures: FuturesUnordered<_> = actions
                .into_iter()
                .map(|action| async {
                    match action {
                        Subtask::Push { vid } => {
                            RemoteCommit { vid }
                                .run(storage.clone(), remote.clone())
                                .await
                        }
                        Subtask::Pull { vid } => {
                            Ok(storage.read_write().sync_remote_to_local(vid)?)
                        }
                    }
                })
                .collect();
            while let Some(result) = futures.next().await {
                if let Err(err) = result {
                    tracing::error!("Autosync action failed: {:?}", err);
                }
            }
        }
    }
}

/// Merge two optional LSN boundaries, keeping the most restrictive (earliest).
fn merge_min_lsn(a: Option<LSN>, b: Option<LSN>) -> Option<LSN> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}
