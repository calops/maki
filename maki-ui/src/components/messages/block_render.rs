//! Per-block render cache for the Lua transcript renderer.
//!
//! Rust projects a block, asks the renderer for it keyed by
//! `(id, revision, width, theme generation, mode)`, and keeps at most one
//! request in flight per block. Replies are drained outside `view`; a
//! completion applies only while its id and revision still match, so a
//! reordered or stale render is dropped and the caller keeps drawing the
//! previous frame.

use std::collections::HashMap;
use std::sync::Arc;

use maki_lua::{BlockRender, RenderCtx, RenderObject};

/// Stable, panel-local identity for one projected block. Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BlockId(u64);

impl BlockId {
    pub(crate) fn new(id: u64) -> Self {
        Self(id)
    }
}

/// Bumped whenever a block's projected content changes.
pub(crate) type Revision = u64;

/// Everything a rendered block depends on besides its content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RenderKey {
    pub(crate) width: u16,
    pub(crate) theme_gen: u64,
    pub(crate) mode: Arc<str>,
    /// Renderer registry generation. Advances when a plugin registers,
    /// unloads, or replaces its renderer, so a block skipped before the
    /// change is retried and a reply from the former chain is refused.
    pub(crate) generation: u64,
}

enum State {
    Pending(flume::Receiver<BlockRender>),
    Ready(Vec<RenderObject>),
    /// No renderer claimed the block, or one raised. Sticky for this key.
    Skipped,
}

struct Entry {
    revision: Revision,
    key: RenderKey,
    state: State,
}

/// Render results for transcript blocks, applied outside `view`.
#[derive(Default)]
pub(crate) struct BlockRenderCache {
    entries: HashMap<BlockId, Entry>,
}

impl BlockRenderCache {
    /// Asks the renderer for `id`. Supersedes any request still in flight
    /// for the same block and skips a block already covered by this key.
    pub(crate) fn request(
        &mut self,
        id: BlockId,
        revision: Revision,
        block: serde_json::Value,
        key: RenderKey,
        send: impl FnOnce(serde_json::Value, RenderCtx) -> flume::Receiver<BlockRender>,
    ) {
        if let Some(entry) = self.entries.get(&id)
            && entry.revision == revision
            && entry.key == key
        {
            return;
        }
        let ctx = RenderCtx {
            width: key.width,
            mode: Arc::clone(&key.mode),
            theme_gen: key.theme_gen,
        };
        let rx = send(block, ctx);
        self.entries.insert(
            id,
            Entry {
                revision,
                key,
                state: State::Pending(rx),
            },
        );
    }

    /// True when `id` still needs a render for this revision and key, so
    /// the caller can skip building the block projection.
    pub(crate) fn needs(&self, id: BlockId, revision: Revision, key: &RenderKey) -> bool {
        !matches!(
            self.entries.get(&id),
            Some(entry) if entry.revision == revision && entry.key == *key
        )
    }

    /// Applies every finished answer, calling `on_failure` with the plugin and
    /// message of each accepted failure so the caller can surface it. Reports
    /// whether anything changed, so the caller can mark the frame dirty.
    pub(crate) fn poll(&mut self, mut on_failure: impl FnMut(&Arc<str>, u64, &str)) -> bool {
        let mut changed = false;
        for entry in self.entries.values_mut() {
            let State::Pending(rx) = &entry.state else {
                continue;
            };
            let Ok(result) = rx.try_recv() else {
                continue;
            };
            let generation = entry.key.generation;
            entry.state = match result {
                BlockRender::Objects(objects) => State::Ready(objects),
                BlockRender::Unhandled => State::Skipped,
                BlockRender::Failed { plugin, message } => {
                    on_failure(&plugin, generation, &message);
                    State::Skipped
                }
            };
            changed = true;
        }
        changed
    }

    /// The rendered objects for `id`, when they match `revision` and `key`.
    pub(crate) fn objects(
        &self,
        id: BlockId,
        revision: Revision,
        key: &RenderKey,
    ) -> Option<&[RenderObject]> {
        let entry = self.entries.get(&id)?;
        if entry.revision != revision || entry.key != *key {
            return None;
        }
        match &entry.state {
            State::Ready(objects) => Some(objects),
            State::Pending(_) | State::Skipped => None,
        }
    }

    /// True while any block is still waiting on the renderer, so the
    /// caller can schedule a frame instead of sleeping the full idle poll.
    pub(crate) fn inflight(&self) -> bool {
        self.entries
            .values()
            .any(|entry| matches!(entry.state, State::Pending(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::SnapshotLine;

    const FAILURE_MESSAGE: &str = "renderer boom";

    fn key(width: u16) -> RenderKey {
        key_gen(width, 7)
    }

    fn key_gen(width: u16, generation: u64) -> RenderKey {
        RenderKey {
            width,
            theme_gen: 1,
            mode: Arc::from("build"),
            generation,
        }
    }

    fn key_theme(width: u16, theme_gen: u64) -> RenderKey {
        RenderKey {
            theme_gen,
            ..key_gen(width, 7)
        }
    }

    fn send_object(block: serde_json::Value, _ctx: RenderCtx) -> flume::Receiver<BlockRender> {
        let (tx, rx) = flume::bounded(1);
        tx.send(BlockRender::Objects(vec![RenderObject::Lines {
            lines: vec![SnapshotLine::plain(
                block["kind"].as_str().unwrap_or_default().to_owned(),
            )],
            decorations: Vec::new(),
        }]))
        .expect("reply");
        rx
    }

    fn text(objects: &[RenderObject]) -> String {
        objects
            .iter()
            .map(|object| match object {
                RenderObject::Lines { lines, .. } => lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.text.as_str())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                RenderObject::Raw { seq, .. } => seq.clone(),
                RenderObject::ToolBody => "[tool]".to_owned(),
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn object(id: BlockId, cache: &mut BlockRenderCache, revision: Revision, kind: &str) {
        object_with(id, cache, revision, kind, key(40));
    }

    fn object_with(
        id: BlockId,
        cache: &mut BlockRenderCache,
        revision: Revision,
        kind: &str,
        key: RenderKey,
    ) {
        cache.request(
            id,
            revision,
            serde_json::json!({ "kind": kind }),
            key,
            send_object,
        );
    }

    #[test]
    fn ready_objects_are_exposed_after_poll() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        object(id, &mut cache, 1, "user");
        assert!(cache.objects(id, 1, &key(40)).is_none());
        assert!(cache.poll(|_, _, _| {}));
        assert_eq!(text(cache.objects(id, 1, &key(40)).unwrap()), "user");
    }

    #[test]
    fn same_key_is_not_requested_twice() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        object(id, &mut cache, 1, "first");
        cache.poll(|_, _, _| {});
        object(id, &mut cache, 1, "second");
        assert_eq!(text(cache.objects(id, 1, &key(40)).unwrap()), "first");
    }

    #[test]
    fn changed_revision_supersedes_and_drops_stale_reply() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        let (tx, rx) = flume::bounded(1);
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "old"}),
            key(40),
            move |_, _| rx.clone(),
        );
        object(id, &mut cache, 2, "new");
        // The superseded revision's reply has nowhere to land.
        assert!(tx.send(BlockRender::Objects(Vec::new())).is_err());
        assert!(cache.poll(|_, _, _| {}));
        assert_eq!(text(cache.objects(id, 2, &key(40)).unwrap()), "new");
        assert!(cache.objects(id, 1, &key(40)).is_none());
    }

    #[test]
    fn width_change_re_renders() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        object(id, &mut cache, 1, "narrow");
        cache.poll(|_, _, _| {});
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "wide"}),
            key(80),
            send_object,
        );
        assert!(cache.objects(id, 1, &key(80)).is_none());
        assert!(cache.poll(|_, _, _| {}));
        assert_eq!(text(cache.objects(id, 1, &key(80)).unwrap()), "wide");
    }

    #[test]
    fn theme_generation_change_re_renders() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        object_with(id, &mut cache, 1, "old-theme", key_theme(40, 1));
        cache.poll(|_, _, _| {});
        object_with(id, &mut cache, 1, "new-theme", key_theme(40, 2));
        assert!(cache.objects(id, 1, &key_theme(40, 2)).is_none());
        assert!(cache.poll(|_, _, _| {}));
        assert_eq!(
            text(cache.objects(id, 1, &key_theme(40, 2)).unwrap()),
            "new-theme"
        );
    }

    #[test]
    fn unhandled_is_sticky_for_the_key() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        let retried = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "user"}),
            key(40),
            move |_, _| {
                let (tx, rx) = flume::bounded(1);
                tx.send(BlockRender::Unhandled).expect("reply");
                rx
            },
        );
        cache.poll(|_, _, _| {});
        let counter2 = Arc::clone(&retried);
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "user"}),
            key(40),
            move |block, ctx| {
                counter2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                send_object(block, ctx)
            },
        );
        assert_eq!(retried.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(cache.objects(id, 1, &key(40)).is_none());
    }

    #[test]
    fn failure_reaches_the_callback_with_its_generation() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        let plugin: Arc<str> = Arc::from("renderer");
        let sent_plugin = Arc::clone(&plugin);
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "user"}),
            key_gen(40, 9),
            move |_, _| {
                let (tx, rx) = flume::bounded(1);
                tx.send(BlockRender::Failed {
                    plugin: sent_plugin,
                    message: FAILURE_MESSAGE.to_owned(),
                })
                .expect("reply");
                rx
            },
        );
        let mut seen = Vec::new();
        assert!(cache.poll(|plugin, generation, message| {
            seen.push((Arc::clone(plugin), generation, message.to_owned()));
        }));
        assert_eq!(seen, vec![(plugin, 9, FAILURE_MESSAGE.to_owned())]);
        assert!(cache.objects(id, 1, &key_gen(40, 9)).is_none());
    }

    #[test]
    fn unhandled_is_retried_after_the_generation_changes() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "user"}),
            key_gen(40, 7),
            |_, _| {
                let (tx, rx) = flume::bounded(1);
                tx.send(BlockRender::Unhandled).expect("reply");
                rx
            },
        );
        cache.poll(|_, _, _| {});
        assert!(cache.objects(id, 1, &key_gen(40, 7)).is_none());
        object_with(id, &mut cache, 1, "user", key_gen(40, 8));
        assert!(cache.poll(|_, _, _| {}));
        assert_eq!(text(cache.objects(id, 1, &key_gen(40, 8)).unwrap()), "user");
    }

    #[test]
    fn stale_reply_from_an_older_generation_is_dropped() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        let (old_tx, old_rx) = flume::bounded(1);
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "old"}),
            key_gen(40, 7),
            move |_, _| old_rx.clone(),
        );
        object_with(id, &mut cache, 1, "new", key_gen(40, 8));
        assert!(old_tx.send(BlockRender::Objects(Vec::new())).is_err());
        assert!(cache.poll(|_, _, _| {}));
        assert!(cache.objects(id, 1, &key_gen(40, 7)).is_none());
        assert_eq!(text(cache.objects(id, 1, &key_gen(40, 8)).unwrap()), "new");
    }

    #[test]
    fn inflight_tracks_pending_requests() {
        let mut cache = BlockRenderCache::default();
        let id = BlockId::new(1);
        let (_tx, rx) = flume::bounded(1);
        cache.request(
            id,
            1,
            serde_json::json!({"kind": "user"}),
            key(40),
            move |_, _| rx.clone(),
        );
        assert!(cache.inflight());
        cache.poll(|_, _, _| {});
        assert!(cache.inflight());
    }
}
