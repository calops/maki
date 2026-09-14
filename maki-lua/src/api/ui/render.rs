//! The transcript block renderer chain.
//!
//! Rust projects the session into structured blocks and calls the
//! outermost registered renderer with `(prev, block, ctx)`. A renderer
//! returns in-memory render objects; `prev` runs the next renderer down
//! to the host default, which reports [`BlockRender::Unhandled`] so the
//! caller keeps its own rendering. Registration order is load order, so
//! the last plugin to register is outermost.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use maki_agent::SnapshotLine;
use mlua::{Function, Lua, MultiValue, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::autocmd::dispatch;
use crate::docs::{FnDoc, ParamDoc};

use super::buf::parse_line;

pub(crate) const RENDER_STORE_MISSING: &str = "renderer store not installed";

/// Autocmd fired when a block renderer raises, carrying `{ plugin, message }`.
/// The message stays out of the transcript and every model-facing surface.
pub(crate) const RENDERER_ERROR_EVENT: &str = "RendererError";

/// Attribution for a renderer failure no layer could be tied to.
pub(crate) const UNKNOWN_PLUGIN: &str = "<unknown>";

/// Bumped on every change to the renderer chain.
static RENDERER_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Monotonic count of renderer registry changes. Render keys carry it so a
/// block rendered before a plugin registered, unloaded, or replaced its
/// renderer is retried, and a reply from the former chain is refused.
pub fn renderer_generation() -> u64 {
    RENDERER_GENERATION.load(Ordering::Relaxed)
}

fn bump_generation() {
    RENDERER_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Per-render context. `width` is the content width in cells. Blocks are
/// height-unbounded, so scrolling never re-renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCtx {
    pub width: u16,
    pub mode: Arc<str>,
    pub theme_gen: u64,
}

/// One in-memory render object. `Lines` reuses the span IR buffers and
/// floats already speak.
///
/// `Raw` is a narrowly validated final-writer payload, not a general escape
/// hatch: Lua only describes the sequence and the rectangle it wants. Rust
/// alone validates it, decides absolute placement and clipping, emits it after
/// the frame draw, and restores terminal state around it. Lua never writes to
/// the terminal.
///
/// `ToolBody` is temporary, through the tool migration only: it asks the host
/// to place its native tool body at this point. Lua positions it but cannot
/// carry or mutate the highlight, spinner, snapshot, truncation or interaction
/// metadata; the host owns and validates all of it.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderObject {
    Lines(Vec<SnapshotLine>),
    Raw {
        seq: String,
        width: u16,
        height: u16,
    },
    ToolBody,
}

/// What a block renderer chain produced.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockRender {
    /// No layer claimed the block, so the caller's own renderer applies.
    Unhandled,
    Objects(Vec<RenderObject>),
    /// A layer raised, so the caller keeps its previous frame. `plugin` names
    /// the innermost layer that raised, or [`UNKNOWN_PLUGIN`] when none could
    /// be attributed.
    Failed {
        plugin: Arc<str>,
        message: String,
    },
}

struct Layer {
    plugin: Arc<str>,
    key: RegistryKey,
}

/// Registered block renderers in load order. The last entry is the
/// outermost layer and sees every block first.
#[derive(Default)]
pub(crate) struct RendererStore {
    layers: Vec<Layer>,
}

impl RendererStore {
    /// Registers `key` for `plugin`, replacing any previous one. Answers
    /// the replaced key so the caller can drop its registry value.
    fn set(&mut self, plugin: Arc<str>, key: RegistryKey) -> Option<RegistryKey> {
        let replaced = self.clear_plugin(&plugin);
        self.layers.push(Layer { plugin, key });
        replaced
    }

    /// Removes `plugin`'s renderer, answering its key for cleanup.
    fn clear_plugin(&mut self, plugin: &str) -> Option<RegistryKey> {
        let idx = self
            .layers
            .iter()
            .position(|layer| layer.plugin.as_ref() == plugin)?;
        Some(self.layers.remove(idx).key)
    }
}

/// Clears `plugin`'s renderer and drops its registry value. Shared by
/// unload and failed-load rollback.
pub(crate) fn clear_plugin(lua: &Lua, plugin: &str) {
    let removed = lua
        .app_data_mut::<RendererStore>()
        .and_then(|mut store| store.clear_plugin(plugin));
    if let Some(key) = removed {
        let _ = lua.remove_registry_value(key);
        bump_generation();
    }
}

/// Registers, or clears with `nil`, the calling plugin's block renderer.
pub(crate) fn set_block_renderer(lua: &Lua, plugin: &Arc<str>, value: Value) -> LuaResult<()> {
    match value {
        Value::Nil => {
            clear_plugin(lua, plugin);
            Ok(())
        }
        Value::Function(func) => {
            let mut store = lua
                .app_data_mut::<RendererStore>()
                .ok_or_else(|| mlua::Error::runtime(RENDER_STORE_MISSING))?;
            let key = lua.create_registry_value(func)?;
            let replaced = store.set(Arc::clone(plugin), key);
            drop(store);
            if let Some(replaced) = replaced {
                let _ = lua.remove_registry_value(replaced);
            }
            bump_generation();
            Ok(())
        }
        other => Err(mlua::Error::runtime(format!(
            "set_block_renderer expects a function or nil, got {}",
            other.type_name()
        ))),
    }
}

pub(crate) fn ctx_to_lua(lua: &Lua, ctx: &RenderCtx) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set("width", ctx.width)?;
    table.set("mode", ctx.mode.as_ref())?;
    table.set("theme_gen", ctx.theme_gen)?;
    Ok(table)
}

/// Walks the registered chain for one block. Runs on the Lua thread, so
/// the caller must not hold a borrow of [`RendererStore`]. A raise is
/// attributed to its plugin, reported through the [`RENDERER_ERROR_EVENT`]
/// autocmd, and returned to the caller, which keeps its previous frame.
pub(crate) async fn render_block(lua: &Lua, block: Value, ctx: Table) -> BlockRender {
    let funcs = match lua.app_data_ref::<RendererStore>() {
        Some(store) if !store.layers.is_empty() => store
            .layers
            .iter()
            .map(|layer| {
                lua.registry_value::<Function>(&layer.key)
                    .map(|func| (Arc::clone(&layer.plugin), func))
            })
            .collect::<LuaResult<Vec<_>>>(),
        _ => return BlockRender::Unhandled,
    };
    let raised: Arc<Mutex<Option<Arc<str>>>> = Arc::default();
    let render = match funcs {
        Err(error) => failed(error.to_string()),
        Ok(funcs) => {
            let funcs = Arc::new(funcs);
            let outermost = funcs.len() as isize - 1;
            match chain(lua, funcs, outermost, Arc::clone(&raised))
                .and_then(|renderer| renderer.call::<Value>((block, Value::Table(ctx))))
            {
                Ok(value) => parse_result(value),
                Err(error) => BlockRender::Failed {
                    plugin: raised
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone()
                        .unwrap_or_else(|| Arc::from(UNKNOWN_PLUGIN)),
                    message: error.to_string(),
                },
            }
        }
    };
    if let BlockRender::Failed { plugin, message } = &render {
        report_error(lua, plugin, message).await;
    }
    render
}

/// Best-effort [`RENDERER_ERROR_EVENT`] notification. A failed dispatch must
/// never turn into a second failure, so a missing table is simply skipped.
async fn report_error(lua: &Lua, plugin: &Arc<str>, message: &str) {
    if let Ok(data) = error_data(lua, plugin, message) {
        dispatch(lua.clone(), RENDERER_ERROR_EVENT.to_owned(), None, data).await;
    }
}

fn error_data(lua: &Lua, plugin: &Arc<str>, message: &str) -> LuaResult<Value> {
    let table = lua.create_table()?;
    table.set("plugin", plugin.as_ref())?;
    table.set("message", message)?;
    Ok(Value::Table(table))
}

/// Builds a Lua function that calls layer `idx` with a `prev` bound to
/// the layer below it. `idx < 0` is the host default, which reports
/// unhandled so the caller renders the block itself. `raised` records the
/// first layer whose call returned an error, so the innermost layer that
/// actually raised wins over the wrappers the error propagates through.
fn chain(
    lua: &Lua,
    funcs: Arc<Vec<(Arc<str>, Function)>>,
    idx: isize,
    raised: Arc<Mutex<Option<Arc<str>>>>,
) -> LuaResult<Function> {
    if idx < 0 {
        return lua.create_function(|_, _: MultiValue| Ok(Value::Nil));
    }
    lua.create_function(move |lua, (block, ctx): (Value, Value)| {
        let prev = chain(lua, Arc::clone(&funcs), idx - 1, Arc::clone(&raised))?;
        let (plugin, func) = &funcs[idx as usize];
        func.call::<Value>((prev, block, ctx)).inspect_err(|_| {
            let mut raised = raised.lock().unwrap_or_else(PoisonError::into_inner);
            if raised.is_none() {
                *raised = Some(Arc::clone(plugin));
            }
        })
    })
}

/// A failure with no layer to attribute it to.
pub(crate) fn failed(message: String) -> BlockRender {
    BlockRender::Failed {
        plugin: Arc::from(UNKNOWN_PLUGIN),
        message,
    }
}

fn parse_result(value: Value) -> BlockRender {
    match value {
        Value::Nil | Value::Boolean(false) => BlockRender::Unhandled,
        Value::String(text) => match text.to_str() {
            Ok(line) => BlockRender::Objects(vec![RenderObject::Lines(vec![SnapshotLine::plain(
                line.to_owned(),
            )])]),
            Err(error) => failed(error.to_string()),
        },
        Value::Table(table) => parse_objects(table),
        other => failed(format!(
            "block renderer returned {}, expected a list of lines or {{raw = ...}}",
            other.type_name()
        )),
    }
}

fn parse_objects(table: Table) -> BlockRender {
    if let Some(raw) = as_raw(&table) {
        return match raw {
            Ok(object) => BlockRender::Objects(vec![object]),
            Err(error) => failed(error.to_string()),
        };
    }
    if as_tool(&table) {
        return BlockRender::Objects(vec![RenderObject::ToolBody]);
    }
    let mut out = Vec::with_capacity(table.raw_len());
    for idx in 1..=table.raw_len() {
        let item: Value = match table.raw_get(idx) {
            Ok(item) => item,
            Err(error) => return failed(error.to_string()),
        };
        let object = match &item {
            Value::String(_) => parse_line(&item).map(|line| RenderObject::Lines(vec![line])),
            Value::Table(inner) => match as_raw(inner) {
                Some(raw) => raw,
                None if as_tool(inner) => Ok(RenderObject::ToolBody),
                None => parse_line(&item).map(|line| RenderObject::Lines(vec![line])),
            },
            other => Err(mlua::Error::runtime(format!(
                "render object must be a string, a line, or {{raw = ...}}, got {}",
                other.type_name()
            ))),
        };
        match object {
            Ok(object) => out.push(object),
            Err(error) => return failed(error.to_string()),
        }
    }
    BlockRender::Objects(out)
}

/// A tool-body request is a marker only: the host supplies the content and
/// every piece of metadata, so Lua has nothing to forge.
fn as_tool(table: &Table) -> bool {
    matches!(table.raw_get::<Value>("tool"), Ok(Value::Boolean(true)))
}

fn as_raw(table: &Table) -> Option<LuaResult<RenderObject>> {
    let raw: Value = table.raw_get("raw").ok()?;
    let Value::String(seq) = raw else {
        return None;
    };
    Some((|| {
        Ok(RenderObject::Raw {
            seq: seq.to_str().map_err(mlua::Error::external)?.to_owned(),
            width: table.raw_get::<Option<u16>>("width")?.unwrap_or(0),
            height: table.raw_get::<Option<u16>>("height")?.unwrap_or(0),
        })
    })())
}

#[allow(non_upper_case_globals)]
pub(crate) const set_block_renderer__doc: FnDoc = FnDoc {
    name: "set_block_renderer",
    args: "{fn}",
    desc: "Registers the calling plugin's transcript block renderer. The callback receives `(prev, block, ctx)` and returns a list of render objects: strings, lines (tables of spans), `{{raw = \"...\", width = n, height = n}}` to request a raw sequence in a reserved rectangle, or `maki.ui.transcript_tool()` to place the host's native tool body. Raw is a narrow payload, not an escape hatch: Rust validates the sequence, decides its absolute placement and clipping, emits it after the frame draw, and restores terminal state around it, so the callback never writes to the terminal. `prev(block, ctx)` runs the next renderer down the chain, ending at maki's default. Registration follows plugin load order, so the last plugin to register is outermost. Pass nil to remove your renderer.",
    params: &[ParamDoc {
        name: "{fn}",
        ty: "function|nil",
        desc: "`function(prev, block, ctx) -> table`, or nil to unregister.",
    }],
    returns: "",
    guard: None,
    example: "maki.ui.set_block_renderer(function(prev, block, ctx)\n  if block.kind ~= \"user\" then\n    return prev(block, ctx)\n  end\n  return {\n    { \"▌ \", \"accent\" },\n    { { block.text, { bold = true } } },\n  }\nend)",
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::convert::json_to_lua;
    use serde_json::json;
    use test_case::test_case;

    const PLUGIN: &str = "test";

    fn lua_with_store() -> Lua {
        let lua = Lua::new();
        lua.set_app_data(RendererStore::default());
        lua
    }

    fn register(lua: &Lua, plugin: &str, source: &str) {
        let func: Function = lua.load(source).eval().expect("valid renderer");
        let key = lua.create_registry_value(func).expect("registry key");
        lua.app_data_mut::<RendererStore>()
            .expect("store")
            .set(Arc::from(plugin), key);
    }

    fn render(lua: &Lua, block: serde_json::Value) -> BlockRender {
        let block = json_to_lua(lua, &block).expect("block");
        let ctx = RenderCtx {
            width: 40,
            mode: Arc::from("build"),
            theme_gen: 1,
        };
        let ctx = ctx_to_lua(lua, &ctx).expect("ctx");
        smol::block_on(render_block(lua, block, ctx))
    }

    fn failure(result: BlockRender) -> (Arc<str>, String) {
        match result {
            BlockRender::Failed { plugin, message } => (plugin, message),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    fn lines(objects: &[RenderObject]) -> Vec<String> {
        objects
            .iter()
            .map(|object| match object {
                RenderObject::Lines(lines) => lines
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
            .collect()
    }

    #[test]
    fn tool_body_marker_parses_as_an_object() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            "function(prev, block, ctx) return { { tool = true } } end",
        );
        assert_eq!(
            render(&lua, json!({"kind": "tool"})),
            BlockRender::Objects(vec![RenderObject::ToolBody])
        );
    }

    #[test]
    fn a_bare_tool_body_marker_parses() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            "function(prev, block, ctx) return { tool = true } end",
        );
        assert_eq!(
            render(&lua, json!({"kind": "tool"})),
            BlockRender::Objects(vec![RenderObject::ToolBody])
        );
    }

    #[test]
    fn tool_body_composes_with_surrounding_lines() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            "function(prev, block, ctx) return { 'above', { tool = true }, 'below' } end",
        );
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "tool"})) else {
            panic!("expected objects");
        };
        assert_eq!(objects.len(), 3);
        assert_eq!(objects[1], RenderObject::ToolBody);
    }

    #[test]
    fn unhandled_when_no_layers() {
        let lua = lua_with_store();
        assert_eq!(
            render(&lua, json!({"kind": "user"})),
            BlockRender::Unhandled
        );
    }

    #[test]
    fn single_layer_returns_lines() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            "function(prev, block, ctx) return { 'hello' } end",
        );
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "user"})) else {
            panic!("expected objects");
        };
        assert_eq!(lines(&objects), vec!["hello"]);
    }

    #[test]
    fn outer_wrapper_delegates_to_prev() {
        let lua = lua_with_store();
        register(
            &lua,
            "inner",
            "function(prev, block, ctx) return { 'inner' } end",
        );
        register(
            &lua,
            "outer",
            r#"function(prev, block, ctx)
                 local out = prev(block, ctx)
                 out[#out + 1] = 'outer'
                 return out
               end"#,
        );
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "user"})) else {
            panic!("expected objects");
        };
        assert_eq!(lines(&objects), vec!["inner", "outer"]);
    }

    #[test]
    fn last_registered_wins_without_prev() {
        let lua = lua_with_store();
        register(
            &lua,
            "inner",
            "function(prev, block, ctx) return { 'inner' } end",
        );
        register(
            &lua,
            "outer",
            "function(prev, block, ctx) return { 'outer' } end",
        );
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "user"})) else {
            panic!("expected objects");
        };
        assert_eq!(lines(&objects), vec!["outer"]);
    }

    #[test]
    fn callback_returning_nil_is_unhandled() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            "function(prev, block, ctx) return prev(block, ctx) end",
        );
        assert_eq!(
            render(&lua, json!({"kind": "user"})),
            BlockRender::Unhandled
        );
    }

    #[test]
    fn raw_object_parses() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            r#"function(prev, block, ctx)
                 return { { raw = "\27_G", width = 2, height = 1 } }
               end"#,
        );
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "user"})) else {
            panic!("expected objects");
        };
        assert_eq!(
            objects,
            vec![RenderObject::Raw {
                seq: "\u{1b}_G".to_owned(),
                width: 2,
                height: 1,
            }]
        );
    }

    #[test]
    fn multi_span_line_parses() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            r#"function(prev, block, ctx)
                 return { { { 'a', 'bold' }, { 'b' } } }
               end"#,
        );
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "user"})) else {
            panic!("expected objects");
        };
        let RenderObject::Lines(lines) = &objects[0] else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].text, "a");
        assert_eq!(lines[0].spans[1].text, "b");
    }

    #[test]
    fn ctx_and_block_reach_the_callback() {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            r#"function(prev, block, ctx)
                 return { ctx.width .. ' ' .. block.kind .. ' ' .. ctx.mode }
               end"#,
        );
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "tool"})) else {
            panic!("expected objects");
        };
        assert_eq!(lines(&objects), vec!["40 tool build"]);
    }

    #[test]
    fn raising_layer_becomes_failed_and_is_attributed() {
        let lua = lua_with_store();
        register(&lua, PLUGIN, "function(prev, block, ctx) error('boom') end");
        let (plugin, message) = failure(render(&lua, json!({"kind": "user"})));
        assert_eq!(&*plugin, PLUGIN);
        assert!(message.contains("boom"));
    }

    #[test]
    fn inner_raise_is_attributed_over_the_wrapper() {
        let lua = lua_with_store();
        register(
            &lua,
            "inner",
            "function(prev, block, ctx) error('inner boom') end",
        );
        register(
            &lua,
            "outer",
            "function(prev, block, ctx) return prev(block, ctx) end",
        );
        let (plugin, message) = failure(render(&lua, json!({"kind": "user"})));
        assert_eq!(&*plugin, "inner");
        assert!(message.contains("inner boom"));
    }

    #[test]
    fn outer_raise_is_attributed_to_the_outer_layer() {
        let lua = lua_with_store();
        register(
            &lua,
            "inner",
            "function(prev, block, ctx) return { 'inner' } end",
        );
        register(
            &lua,
            "outer",
            "function(prev, block, ctx) prev(block, ctx); error('outer boom') end",
        );
        let (plugin, message) = failure(render(&lua, json!({"kind": "user"})));
        assert_eq!(&*plugin, "outer");
        assert!(message.contains("outer boom"));
    }

    #[test]
    fn error_data_carries_plugin_and_message() {
        let lua = lua_with_store();
        let data = error_data(&lua, &Arc::from(PLUGIN), "boom").expect("data");
        let Value::Table(table) = data else {
            panic!("expected a table");
        };
        assert_eq!(table.get::<String>("plugin").expect("plugin"), PLUGIN);
        assert_eq!(table.get::<String>("message").expect("message"), "boom");
    }

    #[test_case("return { { { 'a', 'bold' }, { 'b' } }, 1 }" ; "integer_item")]
    #[test_case("return 42" ; "non_line_value")]
    fn malformed_output_fails(source: &str) {
        let lua = lua_with_store();
        register(
            &lua,
            PLUGIN,
            &format!("function(prev, block, ctx) {source} end"),
        );
        assert!(matches!(
            render(&lua, json!({"kind": "user"})),
            BlockRender::Failed { .. }
        ));
    }

    #[test]
    fn set_block_renderer_replaces_previous() {
        let lua = lua_with_store();
        let plugin: Arc<str> = Arc::from(PLUGIN);
        for text in ["first", "second"] {
            let func: Function = lua
                .load(format!(
                    "function(prev, block, ctx) return {{ '{text}' }} end"
                ))
                .eval()
                .expect("valid renderer");
            set_block_renderer(&lua, &plugin, Value::Function(func)).expect("register");
        }
        let BlockRender::Objects(objects) = render(&lua, json!({"kind": "user"})) else {
            panic!("expected objects");
        };
        assert_eq!(lines(&objects), vec!["second"]);
    }

    #[test]
    fn clear_plugin_unregisters() {
        let lua = lua_with_store();
        let plugin: Arc<str> = Arc::from(PLUGIN);
        let func: Function = lua
            .load("function(prev, block, ctx) return { 'x' } end")
            .eval()
            .expect("valid renderer");
        set_block_renderer(&lua, &plugin, Value::Function(func)).expect("register");
        set_block_renderer(&lua, &plugin, Value::Nil).expect("clear");
        assert_eq!(
            render(&lua, json!({"kind": "user"})),
            BlockRender::Unhandled
        );
    }

    #[test]
    fn generation_advances_on_set_and_clear() {
        let lua = lua_with_store();
        let plugin: Arc<str> = Arc::from(PLUGIN);
        let before = renderer_generation();
        let func: Function = lua
            .load("function(prev, block, ctx) return { 'x' } end")
            .eval()
            .expect("valid renderer");
        set_block_renderer(&lua, &plugin, Value::Function(func)).expect("register");
        let after = renderer_generation();
        assert!(after > before);
        set_block_renderer(&lua, &plugin, Value::Nil).expect("clear");
        assert!(renderer_generation() > after);
    }

    #[test]
    fn set_block_renderer_rejects_non_function() {
        let lua = lua_with_store();
        let plugin: Arc<str> = Arc::from(PLUGIN);
        assert!(set_block_renderer(&lua, &plugin, Value::Integer(1)).is_err());
    }
}
