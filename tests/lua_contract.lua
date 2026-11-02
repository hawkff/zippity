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
assert(plugin:Capabilities().copy_progressive == false)

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
print("Lua preview toggling, safety guards, read-only, and cleanup contracts passed")
