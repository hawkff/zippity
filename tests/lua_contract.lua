local state = {}
local events, notifications, attempts, previews = {}, {}, {}, {}
ya = {
	sync = function(fn) return function(...) return fn(state, ...) end end,
	drop = function() end,
	time = function() return 1 end,
	json_encode = function(value) return value end,
	json_decode = function(value) return value end,
	emit = function(action, args) events[#events + 1] = { action = action, args = args } end,
	notify = function(value) notifications[#notifications + 1] = value end,
	preview_widget = function(_, value) previews[#previews + 1] = value.text end,
	confirm = function() error("Automatic previews must not open confirmation dialogs") end,
	input = function() error("Automatic previews must not open password dialogs") end,
}
ui = { Wrap = { YES = 1 }, Text = function(text)
	local widget = { text = text }
	function widget:area() return self end
	function widget:wrap() return self end
	return widget
end }
Error = { fs = function(value) return value end }
Url = function(path)
	local url = { path = path, spec = { scheme = "", domain = "", is_regular = true } }
	function url:join(name) return Url(self.path .. "/" .. name) end
	return url
end
cx = { active = { current = { cwd = Url("/files") } } }
Command = setmetatable({ PIPED = 1, NULL = 2 }, {
	__call = function()
		local command = {}
		function command:stdin() return self end
		function command:stdout() return self end
		function command:stderr() return self end
		function command:spawn()
			local child = {}
			function child:write_all(value) self.request = value; return true end
			function child:flush() self.flushed = true; return true end
			function child:take_stdin() assert(self.flushed); return {} end
			function child:wait_with_output()
				assert(self.request.op == "clean_cache")
				local root = self.request.directory
				attempts[#attempts + 1] = root
				if root == "/failed" then
					return { status = { success = false }, stderr = { kind = "cache", message = "blocked" } }
				end
				return { status = { success = true }, stdout = { cleaned = true } }
			end
			return child
		end
		return command
	end,
})

local actions = dofile("zippity.yazi/actions.lua")
require = function(name) assert(name == ".actions"); return actions end
local plugin = dofile("zippity.yazi/main.lua")
for _, operation in ipairs({
	"Write", "SetLen", "SetAttrs", "CreateDir", "RemoveFile", "RemoveDir",
	"RemoveDirAll", "Rename", "Symlink", "HardLink", "Trash", "Copy", "CopyProgressive",
}) do
	local ok, err = plugin:provide { op = operation }
	assert(ok == false and err.kind == "PermissionDenied", operation)
end
for _, flag in ipairs({ "write", "append", "truncate", "create", "create_new" }) do
	local result, err = plugin:Open { demand = { [flag] = true } }
	assert(result == nil and err.kind == "PermissionDenied", flag)
end
assert(next(plugin:Capabilities()) == nil, "The provider offers no links, trash, or copies")

plugin:entry { args = { "auto-preview" } }
assert(events[#events].action == "forward", "L must retain native navigation outside archives")
local archive = "/files/example.zip"
local function member(path)
	return { path = archive .. "/" .. path, base = { path = archive }, spec = { scheme = "zippity", domain = "archive" } }
end
cx.active.current.cwd = member("")
plugin:entry { args = { "auto-preview" } }
assert(state.auto_previews[archive] and events[#events].action == "peek" and events[#events].args.force)
local materialize, requests = plugin.materialize, 0
plugin.materialize = function(_, url, automatic)
	assert(url.path == archive .. "/notes.txt" and automatic)
	requests = requests + 1
	return "/cached-preview"
end
plugin:peek { file = { url = member("notes.txt") }, area = {} }
assert(requests == 1 and events[#events].action == "refresh")
assert(state.auto_previews[archive], "Previewing must not toggle the mode")
plugin:entry { args = { "auto-preview" } }
assert(not state.auto_previews[archive])
plugin:peek { file = { url = member("notes.txt") }, area = {} }
assert(requests == 1 and previews[#previews]:match("Press L"))
plugin.materialize = materialize
state.archives = { [archive] = { source = { archive = archive, revision = "fixture" }, stamps = {}, preview_limit = 32 * 1024 * 1024,
	entries = { ["large.bin"] = { size = 33 * 1024 * 1024 }, ["locked.txt"] = { size = 1, encrypted = true } } } }
local path, err = plugin:materialize(member("large.bin"), true)
assert(not path and err.kind == "limit")
path, err = plugin:materialize(member("locked.txt"), true)
assert(not path and err.kind == "password")
fs = { cha = function() return {} end }
state.archives[archive].entries["large.bin"].backing = "/cached-preview"
assert(plugin:materialize(member("large.bin"), true) == "/cached-preview")

local cached, targets, nested = {}, {}, nil
state.archives[archive].cache = "/cache"
state.archives[archive].entries["notes.txt"] = { size = 1 }
Path = { os = function(path) return path end }
fs.cha = function(url) return cached[url.path] end
fs.remove = function(kind, url) assert(kind == "file"); cached[url.path] = nil; return true end
local extract = plugin.extract
plugin.extract = function(self, job)
	targets[#targets + 1] = job.to
	if #targets == 1 then nested = self:materialize(member("notes.txt"), true) end
	cached[job.to] = { len = 1 }
	return 1
end
local concurrent = plugin:materialize(member("notes.txt"), true)
assert(targets[1] ~= targets[2], "Concurrent previews must have separate extraction destinations")
assert(concurrent == nested and concurrent == state.archives[archive].entries["notes.txt"].backing)
assert(not cached[targets[1]] and cached[targets[2]], "Keep only the published member cache")
plugin.extract = extract
cx.active.current.cwd = Url("/files")

state = { cache_dirs = { ["/failed"] = archive, ["/clean"] = archive }, auto_previews = { [archive] = true } }
plugin:entry { args = { "quit" } }
assert(#attempts == 2)
assert(events[#events].action == "quit")
assert(state.cache_dirs["/failed"] and not state.cache_dirs["/clean"])
assert(next(state.auto_previews) == nil)

state = {
	auto_previews = { [archive] = true },
	cache_dirs = { ["/failed"] = archive, ["/clean"] = archive },
	archives = { [archive] = { source = { archive = archive }, cache = "/failed", entries = { ["notes.txt"] = { backing = "/clean/member-1" } } } },
	passwords = { [archive] = { [""] = "fixture-password" } },
}
attempts, events, notifications = {}, {}, {}
cx.active.current.cwd = { path = archive, base = { path = archive }, spec = { scheme = "zippity", domain = "archive" } }
plugin:entry { args = { "lock" } }
assert(#attempts == 2)
assert(events[#events].action == "reveal")
assert(events[1].action == "update_mimes")
local reset_url, reset_mime = next(events[1].args.updates)
assert(reset_url.path == "zippity://archive/" .. archive .. "/notes.txt" and reset_mime == "zippity/member")
assert(not state.passwords[archive] and not state.archives[archive] and not state.auto_previews[archive])
assert(state.cache_dirs["/failed"] and not state.cache_dirs["/clean"])
assert(notifications[#notifications].level == "warn")
assert(notifications[#notifications].content:match("could not be removed"))

local staged
actions:entry({
	context = function() return Url(archive), false, Url("/files") end,
	is_archive = function(url) return url.spec.scheme == "zippity" end,
	-- Yazi addresses a local folder by its parent and name.
	address = function(url) return "/", url.path:match("[^/]+$") end,
	ensure = function() return { source = { archive = archive } } end,
	clipboard = function() return { Url("/files/notes.txt") }, false end,
	rpc = function(request) staged = request; return nil, { kind = "stop", message = "Stop after staging" } end,
	notify = function() end,
}, "add")
assert(staged.op == "stage" and staged.prefix == "", "Additions from outside an archive belong at its root")
for name, stem in pairs {
	["photos.zip"] = "photos", ["backup.tar.gz"] = "backup", ["logs.TAR.Z"] = "logs", ["disk.7z.001"] = "disk",
	["my.backup.part1.rar"] = "my.backup", ["my.backup.z01"] = "my.backup", [".hidden.zip"] = ".hidden", ["README"] = "README",
	["..zip"] = "..zip", ["...zip"] = "...zip", ["..tar.gz"] = "..tar.gz", ["..part1.rar"] = "..part1.rar",
} do assert(actions:archive_stem(name) == stem, name) end
local tuned = actions:compression("9 method=PPMd dict=256M word=16 solid=off")
assert(tuned.level == 9 and tuned.method == "ppmd" and tuned.dictionary == "256m" and tuned.word_size == 16 and tuned.solid == "off")
assert(actions:compression(" 5").level == 5 and not actions:compression("5").method)
for _, text in ipairs { "fast", "5 speed=fast", "5 word=x", "5 word=16.5", "5 word=-1", "5 word=1e1", "5 method" } do assert(not actions:compression(text), text) end

-- Yazi 26.9.1 lists { file, cha } pairs and reads one range per call. Nightly has Stat,
-- streams Files and member bytes from an offset, and returns times as Time values.
local shapes = "/files/shapes.zip"
local function node(path)
	local url = { path = shapes .. "/" .. path, base = { path = shapes }, spec = { scheme = "zippity", domain = "archive" } }
	function url:join(name) return node(path == "" and name or path .. "/" .. name) end
	return url
end
local big, reads = string.rep("z", 1024 * 1024 + 10), {}
actions.reader = function(_, _, _, offset, length)
	reads[#reads + 1] = { offset = offset, length = length }
	return big:sub(offset + 1, offset + length)
end
ya.co = function(fn) return coroutine.create(fn) end
local function drain(co)
	local items = {}
	while true do
		local ok, value, err = coroutine.resume(co)
		assert(ok, value)
		if coroutine.status(co) == "dead" then return items, err end
		items[#items + 1] = value
	end
end
File = function(value) return value end
for _, nightly in ipairs { false, true } do
	Stat, Cha = nil, nil
	if nightly then Stat = function(value) return value end else Cha = function(value) return value end end
	local shaped = dofile("zippity.yazi/main.lua")
	state = { archives = { [shapes] = { source = { archive = shapes, revision = "fixture", volumes = {} }, stamps = {},
		preview_limit = 32 * 1024 * 1024, cache = "/cache", children = { [""] = { "big.bin" } },
		entries = { [""] = { path = "", directory = true, size = 0 }, ["big.bin"] = { path = "big.bin", size = #big } } } } }
	assert(next(shaped:Capabilities()) == nil and shaped:Close())
	local listing, handle = shaped:ReadDir { url = node("") }, shaped:Open { url = node("big.bin"), demand = {} }
	local metadata = shaped:Metadata { url = node("big.bin") }
	assert(metadata.len == #big)
	reads = {}
	if nightly then
		local files, err = drain(listing)
		assert(not err and #files == 1 and files[1].stat.len == #big and files[1].stat == files[1].lstat)
		assert(handle.random == "read")
		local chunks, read_err = drain(shaped:Read { url = node("big.bin"), offset = { value = 2 } })
		assert(not read_err and table.concat(chunks) == big:sub(3))
		assert(#reads == 2 and reads[1].length == 1024 * 1024 and reads[2].offset == 1024 * 1024 + 2,
			"Each read stays within the helper's limit")
		local missing, missing_err = drain(shaped:ReadDir { url = node("absent") })
		assert(#missing == 0 and missing_err.message:match("missing"), "Listing errors arrive through the stream")
		ya.time, fs = function() return { unix = 9 } end, { stat = function() end, remove = function() return true end }
		shaped.extract = function() return #big end
		assert(shaped:materialize(node("big.bin"), true))
		assert(state.archives[shapes].entries["big.bin"].cached_at == 9, "Times cross ya.sync as numbers")
		ya.time = function() return 1 end
	else
		assert(#listing == 1 and listing[1].cha.len == #big and listing[1].cha == listing[1].file.cha and handle == 0)
		assert(shaped:Read { url = node("big.bin"), offset = 2, len = 3 } == "zzz" and #reads == 1)
	end
end
print("Lua preview toggling, safety guards, read-only, cleanup, and provider contracts passed")
