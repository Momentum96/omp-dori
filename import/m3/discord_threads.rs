//! Active-thread discovery under explicitly allowed parents (at most 128, refreshed every 30 s) and
//! followed-thread routes to their parent.

use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tokio::time::Instant;

use super::discord_rest::DiscordApi;
use super::discord_types::Snowflake;

const ACTIVE_LIMIT: usize = 128;
const REFRESH: Duration = Duration::from_secs(30);
const DISCORD_EPOCH: u64 = 1_420_070_400_000;

#[derive(Deserialize)]
struct Channel {
    id: Snowflake,
    #[serde(rename = "type")]
    kind: u8,
    guild_id: Option<Snowflake>,
    parent_id: Option<Snowflake>,
    thread_metadata: Option<ThreadMetadata>,
}

#[derive(Deserialize)]
pub(super) struct ThreadMetadata {
    pub(super) archived: bool,
}

#[derive(Deserialize)]
struct Active {
    threads: Vec<Channel>,
}

// Lock guards are confined to synchronous route changes.
#[derive(Default)]
pub(super) struct Routes(Mutex<RouteState>);

#[derive(Default)]
struct RouteState {
    /// Active followed thread → its explicitly allowed parent.
    active: BTreeMap<Snowflake, Snowflake>,
    pending: BTreeSet<Snowflake>,
    dirty: bool,
}

impl Routes {
    fn lock(&self) -> MutexGuard<'_, RouteState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn contains(&self, id: Snowflake) -> bool {
        self.lock().active.contains_key(&id)
    }

    /// The parent of an active followed thread.
    pub(super) fn parent(&self, id: Snowflake) -> Option<Snowflake> {
        self.lock().active.get(&id).copied()
    }

    pub(super) fn wake(&self, id: Snowflake) -> bool {
        let mut state = self.lock();
        state.active.contains_key(&id) && state.pending.insert(id)
    }

    pub(super) fn invalidate(&self, id: Snowflake) {
        let mut state = self.lock();
        state.active.remove(&id);
        state.pending.remove(&id);
        state.dirty = true;
    }

    pub(super) fn refresh_soon(&self) {
        self.lock().dirty = true;
    }

    pub(super) fn take(&self, all: bool) -> BTreeSet<Snowflake> {
        let mut state = self.lock();
        if all {
            state.pending.clear();
            state.active.keys().copied().collect()
        } else {
            mem::take(&mut state.pending)
        }
    }

    fn replace(&self, active: BTreeMap<Snowflake, Snowflake>) {
        let mut state = self.lock();
        // A thread changed while REST was in flight. Keep the invalidation and retry.
        if !state.dirty {
            state.pending.retain(|id| active.contains_key(id));
            state.active = active;
        }
    }
}

pub(super) struct Discovery {
    parents: Vec<(Snowflake, Snowflake)>,
    guilds: BTreeSet<Snowflake>,
    refreshed: Option<Instant>,
    containers: BTreeSet<Snowflake>,
    /// Explicit channels that are not threads (thread parents and recall spaces).
    plain: BTreeSet<Snowflake>,
    /// Explicit threads → their parents.
    explicit_threads: BTreeMap<Snowflake, Snowflake>,
    pub(super) startup: Snowflake,
}

impl Discovery {
    pub(super) async fn open(api: &DiscordApi, channels: &[Snowflake]) -> Result<Self> {
        let elapsed = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        let offset = elapsed
            .checked_sub(DISCORD_EPOCH)
            .context("system time predates Discord")?;
        let barrier = offset
            .checked_mul(1 << 22)
            .context("Discord timestamp exceeds the snowflake range")?;
        let startup = Snowflake::try_from(barrier.to_string()).map_err(anyhow::Error::msg)?;
        let mut parents = Vec::new();
        let mut guilds = BTreeSet::new();
        let mut containers = BTreeSet::new();
        let mut plain = BTreeSet::new();
        let mut explicit_threads = BTreeMap::new();
        for &id in channels {
            let channel: Channel = api.get(&format!("channels/{id}")).await?;
            ensure!(
                channel.id == id,
                "Discord channel identity changed while discovering threads"
            );
            if matches!(channel.kind, 15 | 16) {
                containers.insert(id);
            }
            match (channel.kind, channel.parent_id) {
                (10..=12, Some(parent)) => {
                    explicit_threads.insert(id, parent);
                }
                _ => {
                    plain.insert(id);
                }
            }
            // DMs and explicitly tracked threads are already routes, never discovery parents.
            if matches!(channel.kind, 0 | 5 | 15 | 16)
                && let Some(guild) = channel.guild_id
            {
                parents.push((id, guild));
                guilds.insert(guild);
            }
        }
        Ok(Self {
            parents,
            guilds,
            refreshed: None,
            containers,
            plain,
            explicit_threads,
            startup,
        })
    }

    /// An explicit non-thread channel (a thread parent and recall space).
    pub(super) fn is_parent(&self, channel: Snowflake) -> bool {
        self.plain.contains(&channel)
    }

    pub(super) fn readable(&self, channel: Snowflake) -> bool {
        !self.containers.contains(&channel)
    }

    pub(super) async fn refresh(&mut self, api: &DiscordApi, routes: &Routes) -> bool {
        if !mem::take(&mut routes.lock().dirty)
            && self.refreshed.is_some_and(|at| at.elapsed() < REFRESH)
        {
            return false;
        }
        self.refreshed = Some(Instant::now());
        let mut active = BTreeMap::new();
        for &guild in &self.guilds {
            let reply = api
                .get::<Active>(&format!("guilds/{guild}/threads/active"))
                .await;
            let threads = match reply {
                Ok(reply) => reply.threads,
                Err(_) => continue,
            };
            for thread in threads {
                if thread.guild_id == Some(guild)
                    && matches!(thread.kind, 10..=12)
                    && thread
                        .thread_metadata
                        .is_some_and(|metadata| !metadata.archived)
                    && thread
                        .parent_id
                        .is_some_and(|parent| self.parents.contains(&(parent, guild)))
                {
                    if let Some(parent) = thread.parent_id {
                        active.insert(thread.id, parent);
                    }
                    if active.len() > ACTIVE_LIMIT {
                        routes.replace(BTreeMap::new());
                        return true;
                    }
                }
            }
        }
        routes.replace(active);
        true
    }
}
