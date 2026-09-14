-- maki's default transcript block renderer. Third-party plugins layer over it
-- and delegate with prev().

local LEADING_BREAKS = "^\n+"
local BREAK_LINE = "([^\n]*)\n"
local DONE = "done"
local ERROR = "error"
local SUCCESS_STYLE = "tool_success"
local ERROR_STYLE = "error"
local RESPONSES = {
  assistant = { prefix = "maki> ", prefix_style = "assistant_prefix", text_style = "assistant" },
  user = { prefix = "you> ", prefix_style = "user", text_style = "assistant" },
}

-- Mirrors Rust `plain_lines` with an empty prefix: strip leading breaks, then
-- one line per remaining '\n', keeping a trailing empty line.
local function plain_lines(text, style)
  local out = {}
  for line in (text:gsub(LEADING_BREAKS, "") .. "\n"):gmatch(BREAK_LINE) do
    out[#out + 1] = { { line, style } }
  end
  return out
end

-- The success bubble is bold, which no named style carries, so it needs the
-- resolved style table rather than a name.
local function done_lines(text)
  local style = maki.ui.theme_style(SUCCESS_STYLE)
  if not style then
    return nil
  end
  style.bold = true
  return plain_lines(text, style)
end

local function response_lines(block, ctx)
  local role = RESPONSES[block.kind]
  if not role then
    return nil
  end
  local text_style = maki.ui.theme_style(role.text_style)
  local prefix_style = maki.ui.theme_style(role.prefix_style)
  if not (text_style and prefix_style) then
    return nil
  end
  return maki.ui.transcript_markdown(block.text, ctx.width, {
    prefix = role.prefix,
    text_style = text_style,
    prefix_style = prefix_style,
  })
end

maki.ui.set_block_renderer(function(prev, block, ctx)
  local lines
  if block.kind == DONE then
    lines = done_lines(block.text)
  elseif block.kind == ERROR then
    lines = plain_lines(block.text, ERROR_STYLE)
  else
    lines = response_lines(block, ctx)
  end
  return lines or prev(block, ctx)
end)
