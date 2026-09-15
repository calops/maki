use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use humantime::format_duration;
use maki_agent::{SnapshotLine, SnapshotSpan, SpanStyle};
use maki_highlight::{DEFAULT_COLOR_NAME, SegmentColor};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Table, Value};
use strum::VariantNames;
use unicode_width::UnicodeWidthStr;

use crate::api::util::command::{
    Anchor, Border, BuiltinAction, Dimension, FloatConfig, HintEntries, HintWriter, Split,
    TitlePos, UiAction, WinCommand, WinEvent, ui_send,
};
use crate::api::util::convert::opt_bool;
use crate::api::util::pair::{Pair, try_pair};
use crate::docs::{FnDoc, ParamDoc};
pub(crate) mod blit;
pub(crate) mod buf;
pub(crate) mod render;
pub(crate) mod win;
pub(crate) mod wrap;

pub use buf::{SPINNER_STYLE_NAME, SPINNER_STYLE_PREFIX, spinner_token};

use crate::runtime::with_task_bufs;
use render::{DecorationGroups, set_block_renderer__doc};
use win::WinHandle;
use wrap::wrap_lines;

const WIDTH_ARG: &str = "width";
const HEIGHT_ARG: &str = "height";
const POSITIVE_INT_ERR: &str = "must be a positive integer";

pub(crate) struct HintStore {
    hints: BTreeMap<Arc<str>, Vec<(String, String)>>,
}

impl HintStore {
    pub fn new() -> Self {
        Self {
            hints: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, plugin: Arc<str>, spans: Vec<(String, String)>) {
        if spans.is_empty() {
            self.hints.remove(&plugin);
        } else {
            self.hints.insert(plugin, spans);
        }
    }

    pub fn clear_plugin(&mut self, plugin: &str) {
        self.hints.retain(|k, _| k.as_ref() != plugin);
    }

    pub fn snapshot_entries(&self) -> HintEntries {
        self.hints
            .iter()
            .map(|(k, v)| (Arc::clone(k), v.clone()))
            .collect()
    }
}

fn publish_hint_snapshot(lua: &Lua) {
    if let Some(store) = lua.app_data_ref::<HintStore>() {
        let entries = store.snapshot_entries();
        if let Some(writer) = lua.app_data_ref::<HintWriter>() {
            writer.publish(entries);
        }
    }
}

pub(crate) fn parse_footer(tbl: &Table) -> LuaResult<Vec<(String, String)>> {
    let footer_tbl: Table = match tbl.get("footer") {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    footer_tbl
        .sequence_values::<Table>()
        .map(|entry| {
            let entry = entry?;
            Ok((entry.get(1)?, entry.get(2)?))
        })
        .collect()
}

/// Creates a new buffer for building UI content. The first buffer
/// created in a task becomes the "live" buffer, streamed to the UI while
/// the tool runs, which is what the tool's own output pane wants. A
/// float that opens during a tool call would take that spot away, so
/// create its buffer with `{ scratch = true }`. It matches nvim's
/// `nvim_create_buf(false, true)`.
///
/// @param opts table? Optional. `scratch` (boolean) keeps the buffer out of the live slot, default false.
/// @return (Buf) Buffer handle.
/// @example
/// -- The tool's output pane:
/// local out = maki.ui.buf()
/// out:line("hello world")
///
/// -- A float raised during a tool call needs its own buffer:
/// local toast = maki.ui.buf({ scratch = true })
/// toast:line("copied!")
#[lua_fn]
fn buf(lua: &Lua, opts: Option<Table>) -> LuaResult<buf::BufHandle> {
    let scratch = opts.and_then(|t| opt_bool(&t, "scratch")).unwrap_or(false);
    Ok(with_task_bufs(lua, |store| {
        if scratch {
            store.create()
        } else {
            store.create_live()
        }
    }))
}

/// Looks up a semantic color from the current theme. Use this to keep
/// your plugin's colors consistent with the rest of the UI.
///
/// @param name string Semantic color name, e.g. "accent" or "background".
/// @return (string|nil) "#rrggbb" for a truecolor theme, a palette index as a
///   string like "4" when the theme names an ANSI color, or "default" for the
///   terminal's own color. Nil only when the name is unknown. Every form can be
///   passed straight to a span's `fg`/`bg`.
/// @example
/// local accent = maki.ui.theme_color("accent")
/// if accent then
///   buf:line({ { "note", { fg = accent, bold = true } } })
/// end
#[lua_fn]
fn theme_color(lua: &Lua, name: String) -> LuaResult<mlua::Value> {
    let Some(color) = maki_highlight::theme_color(&name) else {
        return Ok(mlua::Value::Nil);
    };
    Ok(mlua::Value::String(
        lua.create_string(segment_color_to_lua(color))?,
    ))
}

/// Returns a named theme style as a style table you can pass to a span and
/// then adjust, e.g. to add a modifier the theme does not set.
///
/// The name is the same one a span style string takes, such as `"tool_success"`
/// or `"accent"`. The table holds the resolved `fg`/`bg` and `underline_color`,
/// and only the modifiers that are on (`bold`, `italic`, `underline`, `dim`,
/// `strikethrough`, `reversed`, `hidden`, `slow_blink`, `rapid_blink`), so you
/// can set or clear any before returning it. Unlike `theme_color`, it keeps
/// modifiers and knows the host's UI name set.
///
/// @param name string A named theme style, e.g. "tool_success".
/// @return (table|nil) A span style table, or nil when the name is unknown.
/// @example
/// local style = maki.ui.theme_style("tool_success")
/// if style then
///   style.bold = true
/// end
/// buf:line({ { "all done", style } })
#[lua_fn]
fn theme_style(lua: &Lua, name: String) -> LuaResult<mlua::Value> {
    let Some(style) = maki_highlight::ui_style(&name) else {
        return Ok(mlua::Value::Nil);
    };
    Ok(mlua::Value::Table(buf::ui_style_to_lua(lua, &style)?))
}

/// The host's native transcript Markdown renderer, installed at UI startup.
pub type TranscriptMarkdownFn = fn(&str, u16, &str, &SpanStyle, &SpanStyle) -> Vec<SnapshotLine>;
pub type TranscriptCodeFn = fn(serde_json::Value, u16) -> Vec<SnapshotLine>;

static TRANSCRIPT_MARKDOWN: OnceLock<TranscriptMarkdownFn> = OnceLock::new();
static TRANSCRIPT_CODE: OnceLock<TranscriptCodeFn> = OnceLock::new();

/// Installs the renderer behind `maki.ui.transcript_markdown`. Headless hosts
/// leave it unset, and the primitive then answers nil.
pub fn set_transcript_markdown(renderer: TranscriptMarkdownFn) {
    let _ = TRANSCRIPT_MARKDOWN.set(renderer);
}

/// Renders Markdown exactly as the transcript does and returns its styled
/// lines, so a block renderer can reproduce what the transcript would draw
/// without a second implementation.
///
/// `opts.prefix` shares the first line for inline blocks (paragraph, list,
/// heading) and gets a leader line of its own for standalone blocks (code,
/// table, horizontal rule), matching the transcript. `opts.text_style` and
/// `opts.prefix_style` take the theme styles for the role line. Returns nil
/// when the host has no native renderer (headless).
///
/// @param text string Markdown source.
/// @param width integer Wrap width in display cells, > 0.
/// @param opts table|nil `{prefix?, text_style?, prefix_style?}`.
/// @return (table|nil) Lines, each a span list, or nil when unavailable.
/// @example
/// return maki.ui.transcript_markdown(block.text, ctx.width, {
///   prefix = "maki> ",
///   text_style = maki.ui.theme_style("assistant"),
///   prefix_style = maki.ui.theme_style("assistant_prefix"),
/// })
#[lua_fn]
fn transcript_markdown(
    lua: &Lua,
    text: String,
    width: Value,
    opts: Option<Table>,
) -> LuaResult<Value> {
    let width = positive_dimension(&width, WIDTH_ARG)?;
    let mut prefix = String::new();
    let mut text_style = SpanStyle::Default;
    let mut prefix_style = SpanStyle::Default;
    if let Some(opts) = opts {
        prefix = opts.get::<Option<String>>("prefix")?.unwrap_or_default();
        if let Some(value) = opts.get::<Option<Value>>("text_style")? {
            text_style = buf::parse_style(&value)?;
        }
        if let Some(value) = opts.get::<Option<Value>>("prefix_style")? {
            prefix_style = buf::parse_style(&value)?;
        }
    }
    let Some(render) = TRANSCRIPT_MARKDOWN.get() else {
        return Ok(Value::Nil);
    };
    let lines = render(&text, width, &prefix, &text_style, &prefix_style);
    let out = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, line) in lines.iter().enumerate() {
        out.raw_set(i + 1, buf::line_to_lua(lua, line)?)?;
    }
    Ok(Value::Table(out))
}

/// Installs the temporary exact-parity code renderer. It is removed once Lua
/// code-view primitives cover gutters and syntax, and does not replace the
/// semantic-decoration target.
pub fn set_transcript_code(renderer: TranscriptCodeFn) {
    let _ = TRANSCRIPT_CODE.set(renderer);
}

/// Renders structured code input and read output exactly as the transcript,
/// as a temporary parity bridge. Temporary: removed after Lua code-view
/// primitives reach parity, and separate from the semantic-decoration target.
///
/// @param code table Structured `block.tool.code` source data.
/// @param width integer Wrap width in display cells, > 0.
/// @return (table|nil) Lines, or nil when unavailable.
#[lua_fn]
fn transcript_code(lua: &Lua, code: Value, width: Value) -> LuaResult<Value> {
    let width = positive_dimension(&width, WIDTH_ARG)?;
    let code = lua.from_value(code)?;
    let Some(render) = TRANSCRIPT_CODE.get() else {
        return Ok(Value::Nil);
    };
    let lines = render(code, width);
    let out = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, line) in lines.iter().enumerate() {
        out.raw_set(i + 1, buf::line_to_lua(lua, line)?)?;
    }
    Ok(Value::Table(out))
}

/// Returns the render object that places the host's native tool body at this
/// point in a block renderer's output.
///
/// Temporary, for the tool migration only. Lua decides where the body goes and
/// what surrounds it, while the host owns the body's content and every piece of
/// its metadata: async syntax highlighting, spinner animation, live snapshot,
/// truncation and click offsets. A render may include it at most once, and the
/// host refuses the block otherwise. It carries no data, so none of that
/// metadata can be forged.
///
/// @return (table) `{tool = true}`.
/// @example
/// maki.ui.set_block_renderer(function(prev, block, ctx)
///   if block.kind ~= "tool" then
///     return prev(block, ctx)
///   end
///   return maki.ui.lines({ "tool above", "tool below" }, maki.ui.transcript_tool())
/// end)
#[lua_fn]
fn transcript_tool(lua: &Lua) -> LuaResult<Table> {
    let object = lua.create_table_with_capacity(1, 0)?;
    object.raw_set("tool", true)?;
    Ok(object)
}

/// Returns a span that the host paints with the live spinner glyph, for use
/// inside any line of spans, e.g. an in-progress tool's leading indicator.
///
/// The span carries empty text and the semantic `{spinner = ...}` style, so it
/// can sit anywhere a `{text, style}` span can and the host substitutes the
/// glyph on every tick without a re-render. `style` names the theme style the
/// glyph is drawn in; omit it for the theme's spinner style. The style must
/// name an existing semantic theme style; frame, cadence, and motion settings
/// remain host-owned.
///
/// @param style string|nil Existing semantic theme style name, e.g. "tool_dim".
/// @return (table) A span `{ "", { spinner = style } }`.
/// @example
/// maki.ui.set_block_renderer(function(prev, block, ctx)
///   return { { maki.ui.spinner(), "working" } }
/// end)
#[lua_fn]
fn spinner(lua: &Lua, style: Option<String>) -> LuaResult<Table> {
    let style = style.unwrap_or_else(|| buf::SPINNER_STYLE_NAME.to_owned());
    if !semantic_spinner_style(lua, &style) {
        return Err(mlua::Error::runtime(format!(
            "spinner style must name a registered semantic style, got {style}"
        )));
    }
    let span = lua.create_table_with_capacity(2, 0)?;
    span.raw_set(1, "")?;
    let style_table = lua.create_table_with_capacity(0, 1)?;
    style_table.raw_set("spinner", style)?;
    span.raw_set(2, style_table)?;
    Ok(span)
}

fn semantic_spinner_style(lua: &Lua, style: &str) -> bool {
    matches!(
        style,
        "spinner"
            | "dim"
            | "tool_dim"
            | "tool"
            | "tool_success"
            | "tool_error"
            | "error"
            | "success"
            | "warning"
            | "accent"
    ) || lua
        .app_data_ref::<DecorationGroups>()
        .is_some_and(|groups| groups.owns_style(style))
}

/// Returns the host-owned marker span for a todo status. The token controls
/// glyph and semantic style, so Lua never depends on the marker characters.
///
/// @param status string `pending`, `in_progress`, `completed`, or `cancelled`.
/// @return (table) A styled marker span.
/// @example
/// local marker = maki.ui.todo_marker("in_progress")
#[lua_fn]
fn todo_marker(lua: &Lua, status: String) -> LuaResult<Table> {
    let (text, group) = match status.as_str() {
        "pending" => ("[ ]", "todo.pending"),
        "in_progress" => ("[•]", "todo.in_progress"),
        "completed" => ("[✓]", "todo.completed"),
        "cancelled" => ("[x]", "todo.cancelled"),
        _ => {
            return Err(mlua::Error::runtime(
                "todo marker status must be pending, in_progress, completed, or cancelled",
            ));
        }
    };
    let span = lua.create_table_with_capacity(2, 0)?;
    span.raw_set(1, text)?;
    span.raw_set(2, group)?;
    Ok(span)
}

/// The host's native right-info builder, installed at UI startup.
pub type RightInfoFn = fn(usize, Option<&str>, Option<&str>, u16) -> Vec<SnapshotSpan>;

static RIGHT_INFO: OnceLock<RightInfoFn> = OnceLock::new();

/// Installs the builder behind `maki.ui.right_info`. Headless hosts leave it
/// unset, and the primitive then appends nothing.
pub fn set_right_info(builder: RightInfoFn) {
    let _ = RIGHT_INFO.set(builder);
}

/// Appends the host's right-aligned `usage`/`timestamp` tail to a line of
/// spans, using exactly the transcript's placement rule, and returns the same
/// table. The padding accounts for `spans`' display width, so call it after
/// the header's spans are assembled and before the line is returned.
///
/// Unavailable when the host installs no builder, in which case it appends
/// nothing. `usage` is measured in display cells and `timestamp` in bytes,
/// matching the transcript.
///
/// @param spans table The line's spans, mutated in place.
/// @param usage string|nil Usage text, e.g. "1.2k tokens".
/// @param timestamp string|nil Timestamp text, e.g. "12:00".
/// @param width integer Header width in cells, > 0.
/// @return (table) The same `spans` table.
/// @example
/// local spans = { { "bash> ls", "tool" } }
/// maki.ui.right_info(spans, "10 tokens", "12:00", ctx.width)
#[lua_fn]
fn right_info(
    lua: &Lua,
    spans: Table,
    usage: Option<String>,
    timestamp: Option<String>,
    width: Value,
) -> LuaResult<Table> {
    let Some(build) = RIGHT_INFO.get() else {
        return Ok(spans);
    };
    let width = positive_dimension(&width, WIDTH_ARG)?;
    let mut header_width = 0;
    for i in 1..=spans.raw_len() {
        header_width += span_display_width(&spans.raw_get::<Value>(i)?)?;
    }
    for span in build(header_width, usage.as_deref(), timestamp.as_deref(), width) {
        let idx = spans.raw_len() + 1;
        spans.raw_set(idx, buf::span_to_lua(lua, &span)?)?;
    }
    Ok(spans)
}

/// A span's contribution to a header's display width. A semantic spinner
/// carries no text but the host paints a fixed-width glyph, so it counts as
/// its baked width rather than zero.
fn span_display_width(span: &Value) -> LuaResult<usize> {
    let Value::Table(t) = span else {
        return Ok(0);
    };
    if is_spinner_span(t) {
        return Ok(buf::SPINNER_CELL_WIDTH);
    }
    let Value::String(s) = t.raw_get::<Value>(1)? else {
        return Ok(0);
    };
    Ok(UnicodeWidthStr::width(
        s.to_str().map_err(mlua::Error::external)?.as_ref(),
    ))
}

fn is_spinner_span(t: &Table) -> bool {
    let Ok(Value::Table(style)) = t.raw_get::<Value>(2) else {
        return false;
    };
    style.raw_get::<Value>("spinner").is_ok_and(|v| !v.is_nil())
}

/// Syntax-highlights a chunk of source code. Returns a table of styled
/// lines that you can feed into a buffer. Each line is a list of
/// `{text, style}` spans where style is a `{fg, bold?, italic?, underline?}` table.
///
/// `fg` is "#rrggbb" for a truecolor theme, a palette index as a string like
/// "4" when the theme names an ANSI color, or "default" for the terminal's own
/// color. Pass the span straight to `buf:line` and it resolves correctly in
/// every case.
///
/// @param code string Source text to highlight.
/// @param lang string Language identifier, e.g. "rust", "python".
/// @param opts table? Options. Fields:
///   - independent (boolean): highlight each line without cross-line context. Default false.
///   - prefix (string): prepend to the source before highlighting (affects token context). Default "".
/// @return (table) Lines: `{ { {text, style}, ... }, ... }`. Each style is `{fg, bold?, italic?, underline?}`.
/// @example
/// local lines = maki.ui.highlight("fn main() {}", "rust")
/// for _, spans in ipairs(lines) do
///   buf:line(spans)
/// end
#[lua_fn]
async fn highlight(lua: Lua, code: String, lang: String, opts: Option<Table>) -> LuaResult<Table> {
    let independent = opts
        .as_ref()
        .and_then(|t| opt_bool(t, "independent"))
        .unwrap_or(false);
    let prefix = opts
        .and_then(|t| t.get::<String>("prefix").ok())
        .unwrap_or_default();
    let segments = smol::unblock(move || {
        maki_highlight::pool::run(move || {
            if independent {
                maki_highlight::highlight_lines_independent(&lang, &code)
            } else {
                maki_highlight::highlight_code(&lang, &code, &prefix)
            }
        })
    })
    .await;
    segments_to_lua_lines(&lua, &segments)
}

/// Renders Markdown into styled lines ready to display in a buffer.
/// Each span's style is either a named string ("bold", "heading",
/// "inline_code", etc.) or a `{fg, bold?, italic?, underline?}` table
/// for syntax-highlighted code blocks.
///
/// @param text string Markdown source.
/// @param width integer Wrap width in columns.
/// @return (table) Lines: `{ { {text, style}, ... }, ... }`.
/// @example
/// local size = maki.ui.terminal_size()
/// local lines = maki.ui.markdown("# Hello\n\nSome **bold** text.", size.cols)
/// for _, spans in ipairs(lines) do
///   buf:line(spans)
/// end
#[lua_fn]
async fn markdown(lua: Lua, text: String, width: u16) -> LuaResult<Table> {
    let lines = smol::unblock(move || {
        maki_highlight::pool::run(move || maki_markdown::render::render(&text, width))
    })
    .await;
    markdown_lines_to_lua(&lua, &lines)
}

/// Formats a number of seconds into a short, human-friendly string.
/// Useful for displaying elapsed time in status messages.
///
/// @param secs integer Duration in seconds.
/// @return (string) Human-readable duration, e.g. "1m30s".
/// @example
/// maki.ui.humantime(90)   -- "1m30s"
/// maki.ui.humantime(3661) -- "1h1m1s"
#[lua_fn]
fn humantime(_lua: &Lua, secs: u64) -> LuaResult<String> {
    Ok(format_duration(Duration::from_secs(secs))
        .to_string()
        .replace(' ', ""))
}

/// Returns the current terminal size. Handy for sizing floating windows
/// or wrapping text to fit the screen.
///
/// @return (table) `{cols, rows}`, terminal width and height in characters.
/// @example
/// local size = maki.ui.terminal_size()
/// local half_width = math.floor(size.cols / 2)
#[lua_fn]
fn terminal_size(lua: &Lua) -> LuaResult<Table> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let tbl = lua.create_table()?;
    tbl.set("cols", cols)?;
    tbl.set("rows", rows)?;
    Ok(tbl)
}

/// Returns the display width of a string in terminal cells, matching
/// how `ratatui` measures text.
///
/// @param text string The text to measure.
/// @return (integer) Number of display cells the text occupies.
/// @example
/// local w = maki.ui.display_width("hello")
#[lua_fn]
fn display_width(_lua: &Lua, text: String) -> LuaResult<usize> {
    use unicode_width::UnicodeWidthStr;
    Ok(text.width())
}

/// Splits a string at a display-cell boundary.
///
/// @param text string The text to split.
/// @param max_width integer Maximum display cells for the head.
/// @return (table) `{head = string, tail = string}`.
/// @example
/// local t = maki.ui.truncate_text("hello world", 5)
/// -- t.head == "hello", t.tail == " world"
#[lua_fn]
fn truncate_text(lua: &Lua, text: String, max_width: usize) -> LuaResult<Table> {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    let mut idx = 0;
    for (i, c) in text.char_indices() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max_width {
            break;
        }
        width += w;
        idx = i + c.len_utf8();
    }
    let tbl = lua.create_table()?;
    tbl.set("head", &text[..idx])?;
    tbl.set("tail", &text[idx..])?;
    Ok(tbl)
}

/// Reads a dimension argument. Programmer errors throw, so a bad width or
/// height fails loudly instead of wrapping or placing nothing.
fn positive_dimension(value: &Value, arg: &str) -> LuaResult<u16> {
    let n = match value {
        Value::Integer(i) => *i,
        Value::Number(f) if f.fract() == 0.0 => *f as i64,
        _ => return Err(mlua::Error::runtime(format!("{arg} {POSITIVE_INT_ERR}"))),
    };
    u16::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| mlua::Error::runtime(format!("{arg} {POSITIVE_INT_ERR}")))
}

/// Wraps text to a width in display cells and returns a table of lines.
///
/// The input is a string or one line of `{text, style?}` spans, the shape
/// `buf:line()` takes. The output is always a table of lines, each a span
/// list, so a block renderer can `return maki.ui.wrap(...)` directly.
///
/// Text splits on `\n` into hard paragraphs, and every hard break is a line
/// break. Within a paragraph the wrap is a plain greedy fill: it adds words
/// until the next one would overflow, then breaks before it. A word wider
/// than `width` is cut at the width. Whitespace at a break is dropped, other
/// whitespace stays. A single character wider than the whole width gets a
/// line of its own. Cell widths come from `unicode-width`, so a CJK character
/// counts 2. Styled spans keep their style and split at break points.
///
/// This is not a copy of how the transcript draws wrapped Rust lines. Treat
/// it as its own predictable greedy wrap.
///
/// @param text string|table A string, or a line of `{text, style?}` spans.
/// @param width integer Wrap width in display cells, > 0.
/// @return (table) Lines: `{ { {text, style}, ... }, ... }`.
/// @example
/// maki.ui.set_block_renderer(function(prev, block, ctx)
///   if block.kind ~= "user" then
///     return prev(block, ctx)
///   end
///   return maki.ui.wrap({ { block.text, { italic = true } } }, ctx.width)
/// end)
#[lua_fn]
fn wrap(lua: &Lua, text: Value, width: Value) -> LuaResult<Table> {
    let width = positive_dimension(&width, WIDTH_ARG)?;
    let input = buf::parse_line(&text)?;
    let wrapped = wrap_lines(&input.spans, width);
    let out = lua.create_table_with_capacity(wrapped.len(), 0)?;
    for (i, line) in wrapped.iter().enumerate() {
        out.raw_set(i + 1, buf::line_to_lua(lua, line)?)?;
    }
    Ok(out)
}

/// Builds a render object for a raw terminal sequence: exactly
/// `{raw = seq, width = width, height = height}`.
///
/// This is a shape helper. The final writer validates the sequence and owns
/// where it goes, so calling this writes nothing. Return the object from a
/// block renderer to place the sequence in a rectangle `width` cells wide and
/// `height` rows tall. A sequence must be cursor-local: graphic rendition,
/// cursor movement, and save or restore are allowed, while OSC and graphics
/// payloads are refused. A refused sequence leaves the block on its Rust
/// rendering.
///
/// @param seq string The terminal sequence, e.g. an SGR escape.
/// @param width integer Rectangle width in cells, > 0.
/// @param height integer Rectangle height in rows, > 0.
/// @return (table) `{raw = seq, width = width, height = height}`.
/// @example
/// maki.ui.set_block_renderer(function(prev, block, ctx)
///   if block.kind ~= "user" then
///     return prev(block, ctx)
///   end
///   return {
///     { "cursor lands here:" },
///     maki.ui.raw("\27[7m cursor \27[0m", 8, 1),
///   }
/// end)
#[lua_fn]
fn raw(lua: &Lua, seq: String, width: Value, height: Value) -> LuaResult<Table> {
    let width = positive_dimension(&width, WIDTH_ARG)?;
    let height = positive_dimension(&height, HEIGHT_ARG)?;
    let object = lua.create_table_with_capacity(0, 3)?;
    object.raw_set("raw", seq)?;
    object.raw_set("width", width)?;
    object.raw_set("height", height)?;
    Ok(object)
}

/// Returns its arguments as a Lua array. A readability helper: this matches
/// `{ a, b, c }` and does nothing more, so a renderer body reads like a list
/// of lines.
///
/// @param values any Values to collect, in order.
/// @return (table) Array of the arguments.
/// @example
/// maki.ui.set_block_renderer(function(prev, block, ctx)
///   return maki.ui.lines("first", { { "styled", "bold" } }, "third")
/// end)
#[lua_fn]
fn lines(lua: &Lua, values: mlua::Variadic<Value>) -> LuaResult<Table> {
    let out = lua.create_table_with_capacity(values.len(), 0)?;
    for (i, value) in values.into_iter().enumerate() {
        out.raw_set(i + 1, value)?;
    }
    Ok(out)
}

/// Shows a brief message in the status bar. The message disappears
/// after a short time. Good for confirming an action like "copied!"
/// or showing a transient warning.
///
/// @param msg string Message text.
/// @return
/// @example
/// maki.ui.flash("Copied to clipboard!")
#[lua_fn]
fn flash(_lua: &Lua, #[ctx] tx: flume::Sender<UiAction>, msg: String) -> LuaResult<()> {
    let _ = tx.try_send(UiAction::Flash(msg));
    Ok(())
}

/// Sets the terminal emulator's window title. Pass an empty string to
/// clear it.
///
/// The title passes through tmux, GNU screen, and zellij untouched, and
/// control characters are stripped, so model text cannot inject escape
/// sequences into the terminal. On exit maki hands the title back to the
/// shell, on terminals that support the title stack.
///
/// @param title string New window title, e.g. `"● 3/5 tests"`.
/// @return
/// @example
/// maki.ui.set_window_title("maki: " .. session_name)
/// -- Give the title back to the shell:
/// maki.ui.set_window_title("")
#[lua_fn]
fn set_window_title(
    _lua: &Lua,
    #[ctx] tx: flume::Sender<UiAction>,
    title: String,
) -> LuaResult<()> {
    let _ = tx.try_send(UiAction::SetWindowTitle(title));
    Ok(())
}

/// Runs a built-in UI action by name, exactly as its default keybinding
/// would. Handy when a default key never reaches maki because tmux or
/// your terminal grabs it first: bind a new key with `maki.keymap.set`
/// and call this from it.
///
/// Valid names: `"file_picker"`, `"search"`, `"help"`,
/// `"plan_toggle"`, `"plan_editor"`, `"edit_input"`, `"pop_queue"`,
/// `"prev_chat"`, `"next_chat"`, `"model_picker"`.
///
/// For slash commands rather than keybound actions, see
/// `maki.api.run_command`.
///
/// @param name string Action name, e.g. `"file_picker"`.
/// @return (boolean|nil, string|nil) `true` on success, or nil and an error message for an unknown name.
/// @example
/// -- Open the built-in file picker with Ctrl+Q instead of Ctrl+S:
/// maki.keymap.set("n", "<C-q>", function()
///   maki.ui.action("file_picker")
/// end)
#[lua_fn]
fn action(_lua: &Lua, #[ctx] tx: flume::Sender<UiAction>, name: String) -> LuaResult<Pair<bool>> {
    let builtin = try_pair!(name.parse::<BuiltinAction>().map_err(|_| format!(
        "unknown action '{name}' (valid: {})",
        BuiltinAction::VARIANTS.join(", ")
    )));
    try_pair!(ui_send(Some(&tx), UiAction::Builtin(builtin)));
    Ok((Some(true), None))
}

/// Opens {path} in the user's `$EDITOR` (e.g. vim, nano) and waits for
/// it to close. This suspends the TUI while the editor is running.
/// Returns the editor's exit code so you can check if the user saved.
///
/// @param path string File to open.
/// @return (integer) Editor exit code, or -1 if the action could not be dispatched.
/// @example
/// local code = maki.ui.open_editor("/tmp/scratch.lua")
/// if code == 0 then
///   maki.ui.flash("File saved")
/// end
#[lua_fn]
async fn open_editor(
    _lua: Lua,
    #[ctx] tx: flume::Sender<UiAction>,
    path: String,
) -> LuaResult<i32> {
    let (reply_tx, reply_rx) = flume::bounded::<i32>(1);
    if tx
        .try_send(UiAction::OpenEditor {
            path: PathBuf::from(path),
            reply_tx,
        })
        .is_err()
    {
        return Ok(-1);
    }
    Ok(reply_rx.recv_async().await.unwrap_or(-1))
}

/// Opens a floating or split window that displays the contents of {buf}.
/// Returns a Win handle you can use to receive events, update layout,
/// and close the window when you are done.
///
/// @param buf Buf Buffer to display.
/// @param opts table Float configuration. Fields:
///   - width (integer|string): window width. Integer for absolute columns; "N%" for percent of terminal width. Default "60%".
///   - height (integer|string): window height. Integer for absolute rows; "N%" for percent of terminal height. Default "70%".
///   - row (integer?): row offset from the anchor corner. Negative values move up.
///   - col (integer?): column offset from the anchor corner.
///   - anchor (string): corner the (row, col) offset is relative to. One of "NW" (default), "NE", "SW", "SE".
///   - border (string): border style. One of "rounded" (default), "single", "double", "none".
///   - title (string): text shown in the top border. Default "".
///   - title_pos (string): title alignment. One of "left" (default), "center", "right".
///   - footer (table): key-hint pairs shown in the bottom border. Each entry is {key, label}.
///   - zindex (integer): stacking order. Default 50.
///   - cursor_line (boolean): highlight the focused row. Default false.
///   - reserved_top (integer): rows reserved at the top of the content area. Default 0.
///   - reserved_bottom (integer): rows reserved at the bottom of the content area. Default 0.
///   - split (string): dock the window to an edge instead of floating. One of "above", "below", "left", "right", "panel", or "" (floating, default).
///   - order (integer): paint order among split windows at the same edge. Default 50.
///   - focus (boolean): whether the window takes keyboard focus on open. Default true.
///   - visible (boolean): whether the window is initially visible. Default true.
///   - needs_input (boolean): whether the window means the session needs user input. Default false.
///   - stack (boolean): offset the window past the other stacked windows sharing its anchor, in open order, with a one row gap. Closing one moves the rest up. Floating windows only. Default false.
/// @return (Win) Window handle.
/// @example
/// local buf = maki.ui.buf()
/// buf:line("Pick an option:")
/// local win = maki.ui.open_win(buf, {
///   title = "Menu",
///   width = "50%",
///   height = 10,
///   cursor_line = true,
///   footer = { { "q", "quit" }, { "Enter", "select" } },
/// })
#[lua_fn]
fn open_win(
    _lua: &Lua,
    #[ctx] tx: flume::Sender<UiAction>,
    buf: mlua::AnyUserData,
    opts: Table,
) -> LuaResult<WinHandle> {
    let buf_handle = buf.borrow::<buf::BufHandle>()?;
    let title: String = opts.get("title").unwrap_or_default();
    let cursor_line = opt_bool(&opts, "cursor_line").unwrap_or(false);
    let footer = parse_footer(&opts)?;
    let reserved_bottom: usize = opts.get("reserved_bottom").unwrap_or(0);
    let reserved_top: usize = opts.get("reserved_top").unwrap_or(0);
    let focus = opt_bool(&opts, "focus").unwrap_or(true);
    let zindex: u16 = opts.get("zindex").unwrap_or(50);

    let width = parse_dimension(&opts, "width", Dimension::Percent(60));
    let height = parse_dimension(&opts, "height", Dimension::Percent(70));
    let row: Option<i16> = opts.get("row").ok();
    let col: Option<i16> = opts.get("col").ok();
    let anchor = parse_anchor(&opts);
    let border = parse_border(&opts);
    let title_pos = parse_title_pos(&opts);
    let split = parse_split(&opts);
    let order: u16 = opts.get("order").unwrap_or(50);
    let visible = opt_bool(&opts, "visible").unwrap_or(true);
    let needs_input = opt_bool(&opts, "needs_input").unwrap_or(false);
    let stack = opt_bool(&opts, "stack").unwrap_or(false);

    let config = FloatConfig {
        width,
        height,
        row,
        col,
        anchor,
        border,
        title,
        title_pos,
        footer,
        zindex,
        cursor_line,
        reserved_bottom,
        reserved_top,
        split,
        order,
        visible,
        needs_input,
        stack,
    };

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let border_chrome = match config.border {
        Border::None => 0,
        _ => 2,
    };
    let est_w = config
        .width
        .resolve(term_cols)
        .saturating_sub(border_chrome);
    let est_h = config
        .height
        .resolve(term_rows)
        .saturating_sub(border_chrome);

    // Unbounded on both sides: a full channel would silently drop keys or,
    // worse, a Close command, leaving a zombie modal. Producers are human- or
    // plugin-rate and both ends are drained every tick, so growth is bounded
    // in practice.
    let (event_tx, event_rx) = flume::unbounded::<WinEvent>();
    let (cmd_tx, cmd_rx) = flume::unbounded::<WinCommand>();

    let _ = tx.try_send(UiAction::OpenWin {
        buf: buf_handle.buf.clone(),
        config,
        focus,
        event_tx,
        cmd_rx,
    });

    Ok(WinHandle::new(event_rx, cmd_tx, est_w, est_h, visible))
}

#[allow(non_upper_case_globals)]
pub(crate) const register_decoration_group__doc: FnDoc = FnDoc {
    name: "register_decoration_group",
    args: "{name}, {style}",
    desc: "Registers a plugin-owned semantic decoration group. The returned decoration group is namespaced as `plugin.<plugin>.<name>` and may be used in a Lines decoration. Built-in groups are `syntax.keyword`, `diff.old`, `diff.new`, `diff.old_sign`, `diff.new_sign`, `diff.line_nr`, `diff.old_line_nr`, `diff.new_line_nr`, and `grep.match`.",
    params: &[
        ParamDoc {
            name: "{name}",
            ty: "string",
            desc: "Group suffix containing letters, digits, or underscores.",
        },
        ParamDoc {
            name: "{style}",
            ty: "string|table",
            desc: "Named or inline span style.",
        },
    ],
    returns: "",
    guard: None,
    example: "maki.ui.register_decoration_group(\"match\", { bold = true })\n-- use group = \"plugin.my_plugin.match\" in Lines decorations",
};

#[allow(non_upper_case_globals)]
pub(crate) const set_status_hint__doc: FnDoc = FnDoc {
    name: "set_status_hint",
    args: "{spans}",
    desc: "Shows key hints in the status bar for your plugin. Each hint is a {key, label} pair. Pass nil to clear your plugin's hints. Only your own hints are affected, other plugins keep theirs.",
    params: &[ParamDoc {
        name: "{spans}",
        ty: "table|nil",
        desc: "Sequence of {key, label} pairs, e.g. `{{\"q\", \"quit\"}, {\"j\", \"down\"}}`. Pass nil to remove the plugin's hints.",
    }],
    returns: "",
    guard: None,
    example: "maki.ui.set_status_hint({ {\"q\", \"quit\"}, {\"j\", \"down\"} })\n-- later, clear them:\nmaki.ui.set_status_hint(nil)",
};

lua_table! {
    /// Functions for building interactive UI. Create buffers to hold
    /// content, open floating or split windows to display them, highlight
    /// code, render markdown, and show status hints.
    ///
    /// ```lua
    /// local buf = maki.ui.buf()
    /// buf:line("hello from my plugin!")
    /// local win = maki.ui.open_win(buf, { title = "Greeting", width = "50%", height = 5 })
    /// ```
    extend "maki.ui" => pub(crate) fn add_ui_fns(), DOCS [
        buf, theme_color, theme_style, transcript_markdown, transcript_code, transcript_tool, spinner, todo_marker, right_info,
        highlight, markdown,
        humantime, terminal_size,
        display_width, truncate_text, wrap, raw, lines,
        manual flash, manual action, manual open_editor, manual open_win, manual set_status_hint,
        manual register_decoration_group, manual set_block_renderer, manual set_window_title,
    ]
}

pub(crate) fn create_ui_table(
    lua: &Lua,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    plugin: Arc<str>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    add_ui_fns(&t, lua)?;

    if let Some(tx) = ui_action_tx {
        flash__register(&t, lua, tx.clone())?;
        set_window_title__register(&t, lua, tx.clone())?;
        action__register(&t, lua, tx.clone())?;
        open_editor__register(&t, lua, tx.clone())?;
        open_win__register(&t, lua, tx)?;
    }

    let decoration_plugin = Arc::clone(&plugin);
    t.set(
        "register_decoration_group",
        lua.create_function(move |lua, (name, style): (String, Value)| {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return Err(mlua::Error::runtime(
                    "decoration group name must contain only letters, digits, or underscores",
                ));
            }
            let style = buf::parse_style(&style)?;
            let mut groups = lua
                .app_data_mut::<DecorationGroups>()
                .ok_or_else(|| mlua::Error::runtime("decoration group store is unavailable"))?;
            let group = format!("plugin.{decoration_plugin}.{name}");
            groups.register(Arc::clone(&decoration_plugin), name, style)?;
            Ok(group)
        })?,
    )?;

    let renderer_plugin = Arc::clone(&plugin);
    t.set(
        "set_block_renderer",
        lua.create_function(move |lua, value: mlua::Value| {
            render::set_block_renderer(lua, &renderer_plugin, value)
        })?,
    )?;

    let p = Arc::clone(&plugin);
    t.set(
        "set_status_hint",
        lua.create_function(move |lua, value: mlua::Value| {
            match value {
                mlua::Value::Nil => {
                    if let Some(mut store) = lua.app_data_mut::<HintStore>() {
                        store.clear_plugin(&p);
                    }
                }
                mlua::Value::Table(tbl) => {
                    let spans: Vec<(String, String)> = tbl
                        .sequence_values::<Table>()
                        .map(|entry| {
                            let entry = entry?;
                            Ok((
                                entry.get::<String>(1)?,
                                entry.get::<String>(2).unwrap_or_default(),
                            ))
                        })
                        .collect::<LuaResult<_>>()?;
                    if let Some(mut store) = lua.app_data_mut::<HintStore>() {
                        store.set(Arc::clone(&p), spans);
                    }
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "set_status_hint expects a table or nil",
                    ));
                }
            }
            publish_hint_snapshot(lua);
            Ok(())
        })?,
    )?;

    Ok(t)
}

pub(crate) fn try_parse_dimension(tbl: &Table, key: &str) -> Option<Dimension> {
    if let Ok(s) = tbl.get::<String>(key)
        && let Some(pct) = s.strip_suffix('%')
        && let Ok(v) = pct.parse::<u16>()
    {
        return Some(Dimension::Percent(v));
    }
    if let Ok(v) = tbl.get::<u16>(key) {
        return Some(Dimension::Abs(v));
    }
    None
}

pub(crate) fn parse_dimension(tbl: &Table, key: &str, default: Dimension) -> Dimension {
    try_parse_dimension(tbl, key).unwrap_or(default)
}

fn parse_anchor(tbl: &Table) -> Anchor {
    tbl.get::<String>("anchor")
        .map(|s| Anchor::parse(&s))
        .unwrap_or_default()
}

fn parse_split(tbl: &Table) -> Split {
    tbl.get::<String>("split")
        .map(|s| Split::parse(&s))
        .unwrap_or_default()
}

fn parse_border(tbl: &Table) -> Border {
    tbl.get::<String>("border")
        .map(|s| Border::parse(&s))
        .unwrap_or_default()
}

fn parse_title_pos(tbl: &Table) -> TitlePos {
    tbl.get::<String>("title_pos")
        .map(|s| TitlePos::parse(&s))
        .unwrap_or_default()
}

/// Palette colors stay symbolic (`"4"`, `"12"`, `"default"`) so the terminal
/// picks the actual shade; only true RGB is written as hex. Every spelling
/// here round-trips back through a span's `fg`/`bg`.
fn segment_color_to_lua(c: SegmentColor) -> String {
    match c {
        SegmentColor::Rgb((r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        SegmentColor::Ansi(i) => i.to_string(),
        SegmentColor::Default => DEFAULT_COLOR_NAME.to_owned(),
    }
}

fn segments_to_lua_lines(
    lua: &Lua,
    lines: &[Vec<maki_highlight::StyledSegment>],
) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, segs) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(segs.len(), 0)?;
        for (j, seg) in segs.iter().enumerate() {
            let span = lua.create_table_with_capacity(2, 0)?;
            span.raw_set(1, seg.text.as_str())?;
            let style = lua.create_table_with_capacity(0, 4)?;
            style.raw_set("fg", segment_color_to_lua(seg.fg))?;
            if seg.bold {
                style.raw_set("bold", true)?;
            }
            if seg.italic {
                style.raw_set("italic", true)?;
            }
            if seg.underline {
                style.raw_set("underline", true)?;
            }
            span.raw_set(2, style)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

/// Most spans become a named style string. `Highlight` tokens carry their
/// own rgb, so they become an inline `{fg, bold, italic, underline}` table.
///
/// No wildcard arm on `StyleToken`: adding a variant is a compile error
/// here, so we can't forget to map it.
fn span_style_to_lua(lua: &Lua, span: &maki_markdown::render::Span) -> LuaResult<mlua::Value> {
    use maki_markdown::render::StyleToken;

    let v = match &span.style {
        StyleToken::Text => {
            let name = emphasis_style_name(span.emphasis);
            mlua::Value::String(lua.create_string(name)?)
        }
        StyleToken::InlineCode => mlua::Value::String(lua.create_string("inline_code")?),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let tbl = lua.create_table()?;
            tbl.set("fg", segment_color_to_lua(*fg))?;
            if *bold {
                tbl.set("bold", true)?;
            }
            if *italic {
                tbl.set("italic", true)?;
            }
            if *underline {
                tbl.set("underline", true)?;
            }
            mlua::Value::Table(tbl)
        }
        StyleToken::CodeBar => mlua::Value::String(lua.create_string("code_gutter")?),
        StyleToken::Heading => mlua::Value::String(lua.create_string("heading")?),
        StyleToken::ListMarker => mlua::Value::String(lua.create_string("list_marker")?),
        StyleToken::TableBorder => mlua::Value::String(lua.create_string("table_border")?),
        StyleToken::HorizontalRule => mlua::Value::String(lua.create_string("horizontal_rule")?),
    };
    Ok(v)
}

/// Flatten `Emphasis` to a single named style. Strike wins over bold/italic
/// (the Lua theme has no combined slot). Underline only appears in
/// `Highlight` tokens, not here.
fn emphasis_style_name(e: maki_markdown::Emphasis) -> &'static str {
    if e.strike {
        "strikethrough"
    } else if e.bold && e.italic {
        "bold_italic"
    } else if e.bold {
        "bold"
    } else if e.italic {
        "italic"
    } else {
        ""
    }
}

fn markdown_lines_to_lua(lua: &Lua, lines: &[maki_markdown::render::Line]) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, rendered) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(rendered.spans.len(), 0)?;
        for (j, sp) in rendered.spans.iter().enumerate() {
            let span_tbl = lua.create_table_with_capacity(2, 0)?;
            span_tbl.raw_set(1, sp.text.as_str())?;
            span_tbl.raw_set(2, span_style_to_lua(lua, sp)?)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span_tbl)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_highlight::{StyledSegment, UiStyle};
    use mlua::Lua;
    use std::collections::HashMap;
    use test_case::test_case;

    const MISSING_KEY: &str = "missing";
    const ORANGE_HEX: &str = "#ff8000";

    fn footer_entry(lua: &Lua, key: &str, label: &str) -> Table {
        let t = lua.create_table().unwrap();
        t.raw_set(1, key).unwrap();
        t.raw_set(2, label).unwrap();
        t
    }

    #[test]
    fn parse_footer_missing_returns_empty() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert!(parse_footer(&tbl).unwrap().is_empty());
    }

    #[test]
    fn parse_footer_non_table_value_returns_empty() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("footer", "not a table").unwrap();
        assert!(parse_footer(&tbl).unwrap().is_empty());
    }

    #[test]
    fn parse_footer_preserves_entry_order() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        entries.raw_set(1, footer_entry(&lua, "q", "quit")).unwrap();
        entries.raw_set(2, footer_entry(&lua, "j", "down")).unwrap();
        entries.raw_set(3, footer_entry(&lua, "k", "up")).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        let parsed = parse_footer(&tbl).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("q".into(), "quit".into()),
                ("j".into(), "down".into()),
                ("k".into(), "up".into()),
            ]
        );
    }

    #[test]
    fn parse_footer_missing_label_errors() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        let one_elem = lua.create_table().unwrap();
        one_elem.raw_set(1, "q").unwrap();
        entries.raw_set(1, one_elem).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        assert!(parse_footer(&tbl).is_err());
    }

    #[test]
    fn parse_footer_non_string_element_errors() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        let bad = lua.create_table().unwrap();
        bad.raw_set(1, "q").unwrap();
        bad.raw_set(2, lua.create_table().unwrap()).unwrap();
        entries.raw_set(1, bad).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        assert!(parse_footer(&tbl).is_err());
    }

    #[test]
    fn try_parse_dimension_numeric_is_abs() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", 42u16).unwrap();
        assert_eq!(try_parse_dimension(&tbl, "width"), Some(Dimension::Abs(42)));
    }

    #[test_case("0%", Dimension::Percent(0) ; "zero_percent")]
    #[test_case("50%", Dimension::Percent(50) ; "half_percent")]
    #[test_case("100%", Dimension::Percent(100) ; "full_percent")]
    #[test_case("200%", Dimension::Percent(200) ; "over_hundred_accepted")]
    fn try_parse_dimension_percent_strings(input: &str, expected: Dimension) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", input).unwrap();
        assert_eq!(try_parse_dimension(&tbl, "width"), Some(expected));
    }

    #[test]
    fn try_parse_dimension_missing_key_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert!(try_parse_dimension(&tbl, MISSING_KEY).is_none());
    }

    #[test]
    fn try_parse_dimension_non_numeric_string_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "abc").unwrap();
        assert!(try_parse_dimension(&tbl, "width").is_none());
    }

    #[test]
    fn try_parse_dimension_malformed_percent_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "xx%").unwrap();
        assert!(try_parse_dimension(&tbl, "width").is_none());
    }

    #[test]
    fn parse_dimension_missing_key_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let default = Dimension::Percent(60);
        assert_eq!(parse_dimension(&tbl, MISSING_KEY, default), default);
    }

    #[test]
    fn parse_dimension_invalid_value_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "garbage").unwrap();
        let default = Dimension::Abs(80);
        assert_eq!(parse_dimension(&tbl, "width", default), default);
    }

    #[test_case("NW", Anchor::NW ; "nw")]
    #[test_case("NE", Anchor::NE ; "ne")]
    #[test_case("SW", Anchor::SW ; "sw")]
    #[test_case("SE", Anchor::SE ; "se")]
    #[test_case("garbage", Anchor::NW ; "invalid_falls_back_to_default")]
    fn parse_anchor_cases(input: &str, expected: Anchor) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("anchor", input).unwrap();
        assert_eq!(parse_anchor(&tbl), expected);
    }

    #[test]
    fn parse_anchor_missing_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert_eq!(parse_anchor(&tbl), Anchor::default());
    }

    #[test_case("none", Border::None ; "none")]
    #[test_case("single", Border::Single ; "single")]
    #[test_case("double", Border::Double ; "double")]
    #[test_case("rounded", Border::Rounded ; "rounded")]
    #[test_case("garbage", Border::Rounded ; "invalid_falls_back_to_default")]
    fn parse_border_cases(input: &str, expected: Border) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("border", input).unwrap();
        assert_eq!(parse_border(&tbl), expected);
    }

    #[test]
    fn parse_border_missing_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert_eq!(parse_border(&tbl), Border::default());
    }

    #[test_case("left", TitlePos::Left ; "left")]
    #[test_case("center", TitlePos::Center ; "center")]
    #[test_case("right", TitlePos::Right ; "right")]
    #[test_case("garbage", TitlePos::Left ; "invalid_falls_back_to_default")]
    fn parse_title_pos_cases(input: &str, expected: TitlePos) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("title_pos", input).unwrap();
        assert_eq!(parse_title_pos(&tbl), expected);
    }

    fn seg(text: &str, bold: bool) -> StyledSegment {
        StyledSegment {
            text: text.into(),
            fg: SegmentColor::Rgb((255, 128, 0)),
            bold,
            italic: false,
            underline: false,
        }
    }

    #[test]
    fn segments_to_lua_lines_empty_input() {
        let lua = Lua::new();
        let result = segments_to_lua_lines(&lua, &[]).unwrap();
        assert_eq!(result.len().unwrap(), 0);
    }

    #[test]
    fn segments_to_lua_lines_shape_and_fg_hex() {
        let lua = Lua::new();
        let lines = vec![vec![seg("fn ", true), seg("main", false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();

        assert_eq!(result.len().unwrap(), 1);
        let line: Table = result.get(1).unwrap();
        assert_eq!(line.len().unwrap(), 2);

        let span: Table = line.get(1).unwrap();
        let text: String = span.get(1).unwrap();
        assert_eq!(text, "fn ");
        let style: Table = span.get(2).unwrap();
        let fg: String = style.get("fg").unwrap();
        assert_eq!(fg, ORANGE_HEX);
        let bold: bool = style.get("bold").unwrap();
        assert!(bold);
        assert!(style.get::<Option<bool>>("italic").unwrap().is_none());

        let span2: Table = line.get(2).unwrap();
        let text2: String = span2.get(1).unwrap();
        assert_eq!(text2, "main");
        let style2: Table = span2.get(2).unwrap();
        assert!(style2.get::<Option<bool>>("bold").unwrap().is_none());
    }

    #[test]
    fn segments_to_lua_lines_preserves_utf8() {
        let lua = Lua::new();
        let utf8 = "héllo 🦀 ✨";
        let lines = vec![vec![seg(utf8, false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        let line: Table = result.get(1).unwrap();
        let span: Table = line.get(1).unwrap();
        let text: String = span.get(1).unwrap();
        assert_eq!(text, utf8);
    }

    const STYLE_BOLD: &str = "bold";
    const STYLE_BOLD_ITALIC: &str = "bold_italic";
    const STYLE_HEADING: &str = "heading";
    const STYLE_LIST_MARKER: &str = "list_marker";
    const STYLE_HR: &str = "horizontal_rule";
    const STYLE_PLAIN: &str = "";
    const STYLE_CODE: &str = "inline_code";
    const STYLE_CODE_BAR: &str = "code_gutter";
    const STYLE_ITALIC: &str = "italic";
    const STYLE_STRIKE: &str = "strikethrough";
    const STYLE_TABLE_BORDER: &str = "table_border";
    const MD_WIDTH: u16 = 80;

    fn render_markdown(lua: &Lua, input: &str) -> Table {
        let lines = maki_markdown::render::render(input, MD_WIDTH);
        markdown_lines_to_lua(lua, &lines).unwrap()
    }

    fn span_style(line: &Table, idx: usize) -> String {
        let span: Table = line.get(idx).unwrap();
        match span.get::<mlua::Value>(2).unwrap() {
            mlua::Value::String(s) => s.to_str().unwrap().to_string(),
            other => panic!("expected string style, got {other:?}"),
        }
    }

    fn span_text(line: &Table, idx: usize) -> String {
        let span: Table = line.get(idx).unwrap();
        span.get::<String>(1).unwrap()
    }

    #[test]
    fn markdown_returns_named_styles() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "hello **world**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "hello ");
        assert_eq!(span_style(&line, 1), STYLE_PLAIN);
        assert_eq!(span_text(&line, 2), "world");
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
    }

    #[test]
    fn markdown_bold_italic_emits_bold_italic_not_collapsed_to_bold() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "***x***");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_BOLD_ITALIC);
    }

    #[test]
    fn markdown_unknown_constructs_fall_through_as_plain() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "a*b");
        let line: Table = result.get(1).unwrap();
        for i in 1..=line.len().unwrap() {
            assert_eq!(span_style(&line, i as usize), STYLE_PLAIN);
        }
    }

    #[test]
    fn markdown_code_span_uses_inline_code_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "x `y` z");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 2), STYLE_CODE);
    }

    #[test]
    fn markdown_heading_overrides_inline_emphasis_with_heading_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# hello **world**");
        let line: Table = result.get(1).unwrap();
        for i in 1..=line.len().unwrap() {
            assert_eq!(
                span_style(&line, i as usize),
                STYLE_HEADING,
                "span {i} should be heading-styled"
            );
        }
    }

    #[test]
    fn markdown_list_marker_styled_separately_from_item_content() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "- **item**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_text(&line, 1), "• ");
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
        assert_eq!(span_text(&line, 2), "item");
    }

    #[test]
    fn markdown_horizontal_rule_emits_hr_span_filled_to_width() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "---");
        let line: Table = result.get(1).unwrap();
        assert_eq!(line.len().unwrap(), 1);
        assert_eq!(span_style(&line, 1), STYLE_HR);
        let text = span_text(&line, 1);
        assert_eq!(text.chars().count(), MD_WIDTH as usize);
        assert!(text.chars().all(|c| c == '─'));
    }

    #[test]
    fn markdown_code_inside_bold_collapses_to_inline_code_at_lua_boundary() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "**`code`**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_CODE);
    }

    #[test]
    fn markdown_multiline_emits_one_lua_line_per_logical_line() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "line one\nline two\nline three");
        assert_eq!(result.len().unwrap(), 3);
        let l1: Table = result.get(1).unwrap();
        let l2: Table = result.get(2).unwrap();
        let l3: Table = result.get(3).unwrap();
        assert_eq!(span_text(&l1, 1), "line one");
        assert_eq!(span_text(&l2, 1), "line two");
        assert_eq!(span_text(&l3, 1), "line three");
    }

    #[test]
    fn markdown_italic_alone_surfaces_as_italic_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "*italic*");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "italic");
        assert_eq!(span_style(&line, 1), STYLE_ITALIC);
    }

    #[test]
    fn markdown_strikethrough_surfaces_as_strikethrough_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "~~gone~~");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "gone");
        assert_eq!(span_style(&line, 1), STYLE_STRIKE);
    }

    #[test]
    fn markdown_ordered_list_marker_text_and_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "1. foo");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "1. ");
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_text(&line, 2), "foo");
        assert_eq!(span_style(&line, 2), STYLE_PLAIN);
    }

    #[test]
    fn markdown_ordered_list_marker_keeps_list_marker_style_with_bold_content() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "1. **item**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
        assert_eq!(span_text(&line, 2), "item");
    }

    #[test]
    fn markdown_code_fence_emits_code_bar_prefix_with_highlight_span_tables() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "```rust\nfn x() {}\n```");
        let lines = result.len().unwrap();
        let code_line: Table = (1..=lines)
            .find_map(|i| {
                let line: Table = result.get(i).ok()?;
                (line.len().ok()? > 0
                    && line
                        .get::<Table>(1)
                        .and_then(|s| s.get::<String>(2))
                        .ok()
                        .is_some_and(|s| s == STYLE_CODE_BAR))
                .then_some(line)
            })
            .expect("code bar line");
        assert_eq!(span_style(&code_line, 1), STYLE_CODE_BAR);
        let content_span: Table = code_line.get(2).unwrap();
        let style = content_span.get::<mlua::Value>(2).unwrap();
        assert!(
            matches!(style, mlua::Value::Table(_)),
            "highlight span style must be an inline table"
        );
    }

    #[test_case("# a" ; "h1")]
    #[test_case("## a" ; "h2")]
    #[test_case("### a" ; "h3")]
    #[test_case("###### a" ; "h6")]
    fn markdown_heading_levels_all_surface_as_heading_style(input: &str) {
        let lua = Lua::new();
        let result = render_markdown(&lua, input);
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_HEADING);
    }

    fn seg_full(text: &str, bold: bool, italic: bool, underline: bool) -> StyledSegment {
        StyledSegment {
            text: text.into(),
            fg: SegmentColor::Rgb((255, 128, 0)),
            bold,
            italic,
            underline,
        }
    }

    #[test]
    fn segments_to_lua_lines_modifier_flags_only_present_when_true() {
        let lua = Lua::new();
        let lines = vec![vec![
            seg_full("a", false, true, true),
            seg_full("b", false, false, false),
        ]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        let line: Table = result.get(1).unwrap();
        let s1: Table = line.get(1).unwrap();
        let st1: Table = s1.get(2).unwrap();
        assert!(st1.get::<bool>("italic").unwrap());
        assert!(st1.get::<bool>("underline").unwrap());
        let s2: Table = line.get(2).unwrap();
        let st2: Table = s2.get(2).unwrap();
        assert!(st2.get::<Option<bool>>("italic").unwrap().is_none());
        assert!(st2.get::<Option<bool>>("underline").unwrap().is_none());
    }

    #[test]
    fn segments_to_lua_lines_preserves_line_order() {
        let lua = Lua::new();
        let lines = vec![vec![seg("a", false)], vec![seg("b", false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        assert_eq!(result.len().unwrap(), 2);
        let l1: Table = result.get(1).unwrap();
        let l2: Table = result.get(2).unwrap();
        let s1: Table = l1.get(1).unwrap();
        let s2: Table = l2.get(1).unwrap();
        assert_eq!(s1.get::<String>(1).unwrap(), "a");
        assert_eq!(s2.get::<String>(1).unwrap(), "b");
    }

    #[test]
    fn markdown_table_has_border_and_data_spans() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "| col1 | col2 |\n|------|------|\n| a    | b    |");
        let mut saw_border = false;
        let mut saw_plain = false;
        for i in 1..=result.len().unwrap() {
            let line: Table = result.get(i).unwrap();
            for j in 1..=line.len().unwrap() {
                let span: Table = line.get(j).unwrap();
                if let mlua::Value::String(s) = span.get::<mlua::Value>(2).unwrap() {
                    let s = s.to_str().unwrap();
                    if s == STYLE_TABLE_BORDER {
                        saw_border = true;
                    } else if s == STYLE_PLAIN {
                        saw_plain = true;
                    }
                }
            }
        }
        assert!(saw_border, "table must have border spans");
        assert!(saw_plain, "table must have data/content spans");
    }

    #[test]
    fn markdown_large_input_does_not_panic() {
        let lua = Lua::new();
        let mut input = String::with_capacity(2048);
        for i in 0..200 {
            input.push_str(&format!(
                "# h{i}\n\npara **b{i}** *i{i}* `c{i}` ~~s{i}~~\n\n- item {i}\n\n"
            ));
        }
        assert!(input.len() >= 2000);
        let result = render_markdown(&lua, &input);
        assert!(result.len().unwrap() > 0);
    }

    #[test]
    fn markdown_code_inside_heading_keeps_inline_code_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# foo `bar`");
        let line: Table = result.get(1).unwrap();
        let bar_idx = (1..=line.len().unwrap())
            .find(|&i| span_text(&line, i as usize) == "bar")
            .expect("bar span");
        assert_eq!(span_style(&line, bar_idx as usize), STYLE_CODE);
        let foo_idx = (1..=line.len().unwrap())
            .find(|&i| span_text(&line, i as usize).contains("foo"))
            .expect("foo span");
        assert_eq!(span_style(&line, foo_idx as usize), STYLE_HEADING);
    }

    #[test_case(false, false, false, false, "" ; "default_emphasis_is_empty")]
    #[test_case(true, false, false, false, "bold" ; "bold_only")]
    #[test_case(false, true, false, false, "italic" ; "italic_only")]
    #[test_case(true, true, false, false, "bold_italic" ; "bold_and_italic")]
    #[test_case(false, false, true, false, "strikethrough" ; "strike_only")]
    #[test_case(true, false, true, false, "strikethrough" ; "strike_wins_over_bold")]
    #[test_case(false, true, true, false, "strikethrough" ; "strike_wins_over_italic")]
    #[test_case(false, false, false, true, "" ; "underline_alone_not_surfaced")]
    #[test_case(true, false, false, true, "bold" ; "underline_ignored_with_bold")]
    fn emphasis_style_name_combos(
        bold: bool,
        italic: bool,
        strike: bool,
        underline: bool,
        expected: &str,
    ) {
        let e = maki_markdown::Emphasis {
            bold,
            italic,
            strike,
            underline,
        };
        assert_eq!(emphasis_style_name(e), expected);
    }

    #[test]
    fn span_style_to_lua_highlight() {
        let lua = Lua::new();
        for (bold, italic, underline) in [(true, false, false), (true, true, true)] {
            let span = maki_markdown::render::Span {
                text: "tok".into(),
                style: maki_markdown::render::StyleToken::Highlight {
                    fg: SegmentColor::Rgb((255, 128, 0)),
                    bold,
                    italic,
                    underline,
                },
                emphasis: maki_markdown::Emphasis::default(),
            };
            let val = span_style_to_lua(&lua, &span).unwrap();
            let tbl = match val {
                mlua::Value::Table(t) => t,
                other => panic!("expected table, got {other:?}"),
            };
            assert_eq!(tbl.get::<String>("fg").unwrap(), ORANGE_HEX);
            assert_eq!(tbl.get::<bool>("bold").unwrap(), bold);
            assert_eq!(
                tbl.get::<Option<bool>>("italic").unwrap().unwrap_or(false),
                italic
            );
            assert_eq!(
                tbl.get::<Option<bool>>("underline")
                    .unwrap()
                    .unwrap_or(false),
                underline
            );
        }
    }

    #[test]
    fn markdown_mixed_document_routes_styles_per_block_kind() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# Title\n\nBody **bold** here.\n\n- item");
        assert!(result.len().unwrap() >= 5);

        let heading_line: Table = result.get(1).unwrap();
        for i in 1..=heading_line.len().unwrap() {
            assert_eq!(span_style(&heading_line, i as usize), STYLE_HEADING);
        }

        let body_line: Table = result.get(3).unwrap();
        let mut saw_bold = false;
        for i in 1..=body_line.len().unwrap() {
            if span_style(&body_line, i as usize) == STYLE_BOLD {
                assert_eq!(span_text(&body_line, i as usize), "bold");
                saw_bold = true;
            }
        }
        assert!(saw_bold, "body line should contain a bold span");

        let list_line: Table = result.get(5).unwrap();
        assert_eq!(span_style(&list_line, 1), STYLE_LIST_MARKER);
    }

    #[test]
    fn hint_store_set_and_clear() {
        let mut store = HintStore::new();
        store.set(Arc::from("plug"), vec![("a".into(), "b".into())]);
        assert_eq!(store.snapshot_entries().len(), 1);

        store.clear_plugin("plug");
        assert!(store.snapshot_entries().is_empty());
    }

    #[test]
    fn hint_store_deterministic_order() {
        let mut store = HintStore::new();
        store.set(Arc::from("zzz"), vec![("z".into(), "z".into())]);
        store.set(Arc::from("aaa"), vec![("a".into(), "a".into())]);
        let entries = store.snapshot_entries();
        assert_eq!(entries[0].0.as_ref(), "aaa");
        assert_eq!(entries[1].0.as_ref(), "zzz");
    }

    #[test]
    fn hint_store_empty_clears() {
        let mut store = HintStore::new();
        store.set(Arc::from("plug"), vec![("a".into(), "b".into())]);
        store.set(Arc::from("plug"), vec![]);
        assert!(store.snapshot_entries().is_empty());
    }

    fn ui_lua() -> Lua {
        let lua = Lua::new();
        let ui = create_ui_table(&lua, None, Arc::from("test")).unwrap();
        lua.globals().set("ui", ui).unwrap();
        lua
    }

    #[test]
    fn wrap_returns_a_table_of_lines() {
        let lua = ui_lua();
        let lines: Table = lua
            .load(r#"return ui.wrap("hello world", 5)"#)
            .eval()
            .unwrap();
        assert_eq!(lines.len().unwrap(), 2);
        let first: Table = lines.get(1).unwrap();
        let span: Table = first.get(1).unwrap();
        assert_eq!(span.get::<String>(1).unwrap(), "hello");
        let second: Table = lines.get(2).unwrap();
        let span: Table = second.get(1).unwrap();
        assert_eq!(span.get::<String>(1).unwrap(), "world");
    }

    #[test]
    fn raw_returns_exactly_the_render_object_shape() {
        let lua = ui_lua();
        let object: Table = lua
            .load(r#"return ui.raw("\27[7m cursor \27[0m", 8, 1)"#)
            .eval()
            .unwrap();
        let keys = object.pairs::<String, Value>().count();
        assert_eq!(keys, 3);
        assert_eq!(
            object.get::<String>("raw").unwrap(),
            "\u{1b}[7m cursor \u{1b}[0m"
        );
        assert_eq!(object.get::<u16>("width").unwrap(), 8);
        assert_eq!(object.get::<u16>("height").unwrap(), 1);
    }

    #[test]
    fn lines_collects_arguments_in_order() {
        let lua = ui_lua();
        let out: Table = lua
            .load(r#"return ui.lines("a", { "b" }, 3)"#)
            .eval()
            .unwrap();
        assert_eq!(out.len().unwrap(), 3);
        assert_eq!(out.get::<String>(1).unwrap(), "a");
        let second: Table = out.get(2).unwrap();
        assert_eq!(second.get::<String>(1).unwrap(), "b");
        assert_eq!(out.get::<i64>(3).unwrap(), 3);
    }

    #[test_case(r#"ui.wrap("hi", 0)"#, WIDTH_ARG ; "wrap_zero_width")]
    #[test_case(r#"ui.wrap("hi", -2)"#, WIDTH_ARG ; "wrap_negative_width")]
    #[test_case(r#"ui.wrap("hi", "wide")"#, WIDTH_ARG ; "wrap_non_numeric_width")]
    #[test_case(r#"ui.raw("x", 0, 1)"#, WIDTH_ARG ; "raw_zero_width")]
    #[test_case(r#"ui.raw("x", 1, -1)"#, HEIGHT_ARG ; "raw_negative_height")]
    #[test_case(r#"ui.raw("x", 1, "tall")"#, HEIGHT_ARG ; "raw_non_numeric_height")]
    fn dimension_errors_throw(code: &str, expected: &str) {
        let err = ui_lua().load(code).exec().unwrap_err().to_string();
        assert!(err.contains(expected), "expected {expected:?} in: {err}");
    }

    #[test]
    fn transcript_tool_returns_the_marker() {
        let lua = ui_lua();
        let object: Table = lua.load("return ui.transcript_tool()").eval().unwrap();
        assert!(object.get::<bool>("tool").unwrap());
    }

    #[test_case("", SPINNER_STYLE_NAME ; "default")]
    #[test_case(r#""tool_dim""#, "tool_dim" ; "named")]
    fn spinner_returns_the_semantic_span(argument: &str, expected: &str) {
        let lua = ui_lua();
        let span: Table = lua
            .load(format!("return ui.spinner({argument})"))
            .eval()
            .unwrap();
        assert_eq!(span.raw_get::<String>(1).unwrap(), "");
        let style: Table = span.raw_get(2).unwrap();
        assert_eq!(style.raw_get::<String>("spinner").unwrap(), expected);
    }

    #[test_case("pending", "[ ]", "todo.pending" ; "pending")]
    #[test_case("in_progress", "[•]", "todo.in_progress" ; "in_progress")]
    #[test_case("completed", "[✓]", "todo.completed" ; "completed")]
    #[test_case("cancelled", "[x]", "todo.cancelled" ; "cancelled")]
    fn todo_marker_uses_host_semantics(status: &str, text: &str, group: &str) {
        let lua = ui_lua();
        let span: Table = lua
            .load(format!(r#"return ui.todo_marker("{status}")"#))
            .eval()
            .expect("marker");
        assert_eq!(span.raw_get::<String>(1).expect("text"), text);
        assert_eq!(span.raw_get::<String>(2).expect("group"), group);
    }

    #[test]
    fn todo_marker_rejects_unknown_status() {
        let err = ui_lua()
            .load(r#"return ui.todo_marker("unknown")"#)
            .eval::<Table>()
            .expect_err("invalid status");
        assert!(err.to_string().contains("todo marker status"));
    }

    #[test]
    fn spinner_rejects_unknown_style() {
        let lua = ui_lua();
        let err = lua
            .load(r#"return ui.spinner("not_registered")"#)
            .eval::<Table>()
            .expect_err("unknown style");
        assert!(err.to_string().contains("registered semantic style"));
    }

    #[test]
    fn right_info_appends_the_host_tail_of_the_measured_width() {
        fn build(
            header_width: usize,
            usage: Option<&str>,
            timestamp: Option<&str>,
            width: u16,
        ) -> Vec<SnapshotSpan> {
            vec![SnapshotSpan {
                text: format!("[{header_width}|{usage:?}|{timestamp:?}|{width}]"),
                style: SpanStyle::Default,
            }]
        }
        let _ = RIGHT_INFO.set(build);
        let lua = ui_lua();
        let spans: Table = lua
            .load(
                r#"local s = { { "hi" }, ui.spinner() }; ui.right_info(s, "u", "t", 40); return s"#,
            )
            .eval()
            .unwrap();
        assert_eq!(spans.raw_len(), 3, "the tail appends one span");
        let tail: Table = spans.raw_get(3).unwrap();
        assert_eq!(
            tail.raw_get::<String>(1).unwrap(),
            "[4|Some(\"u\")|Some(\"t\")|40]",
            "the header spinner must count as its baked width"
        );
    }

    #[test]
    fn theme_style_returns_the_published_style_or_nil() {
        const NAME: &str = "tool_success";
        maki_highlight::set_ui_styles(HashMap::from([(
            NAME.to_owned(),
            UiStyle {
                fg: Some(SegmentColor::Ansi(2)),
                bold: true,
                hidden: true,
                slow_blink: true,
                rapid_blink: true,
                underline_color: Some(SegmentColor::Ansi(3)),
                ..UiStyle::default()
            },
        )]));
        let lua = ui_lua();
        let style: Table = lua
            .load(format!(r#"return ui.theme_style("{NAME}")"#))
            .eval()
            .unwrap();
        assert_eq!(style.get::<String>("fg").unwrap(), "2");
        assert_eq!(style.get::<String>("underline_color").unwrap(), "3");
        assert!(style.get::<bool>("bold").unwrap());
        assert!(style.get::<bool>("hidden").unwrap());
        assert!(style.get::<bool>("slow_blink").unwrap());
        assert!(style.get::<bool>("rapid_blink").unwrap());
        assert!(style.get::<Option<bool>>("italic").unwrap().is_none());
        let unknown: Value = lua.load(r#"return ui.theme_style("nope")"#).eval().unwrap();
        assert!(matches!(unknown, Value::Nil));
    }
}
