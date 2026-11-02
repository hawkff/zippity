local A = {}

local state_get = ya.sync(function(state, section, key)
	local values = state[section] or {}
	if key then return values[key] end
	return values
end)
local state_set = ya.sync(function(state, section, key, value)
	state[section] = state[section] or {}
	state[section][key] = value
end)
local claim = ya.sync(function(state, archive)
	state.busy = state.busy or {}
	if state.busy[archive] then return false end
	state.busy[archive] = true
	return true
end)

local indicator = ya.sync(function(state)
	if state.indicator then return end
	state.indicator = true
	Header:children_add(function(self)
		local cwd = self._current.cwd
		if cwd.spec.scheme ~= "zippity" then return "" end
		local draft = (state.drafts or {})[tostring(cwd.base.path)]
		if not draft then return "" end
		return ui.Line(#draft.changes == 0 and " [EDIT] " or (" [UNSAVED " .. #draft.changes .. "] ")):style(ui.Style():fg("yellow"))
	end, 500, Header.RIGHT)
end)

local function fail(message) return { kind = "operation", message = message } end
local function input(title, value)
	local result, event = ya.input { pos = { "center", w = 65 }, title = title, value = value or "", history = "" }
	return event == 1 and result or nil
end
local function archive_at(api)
	local hovered, _, cwd = api.context()
	if api.is_archive(cwd) then return api.address(cwd), hovered, cwd end
	if hovered and hovered.spec.is_regular then return tostring(hovered.path), hovered, cwd end
end
local function load(api, archive)
	for _ = 1, 3 do
		local value, err = api.ensure(archive)
		if value or err.kind ~= "password" then return value, err end
		if not api.prompt(archive) then return nil, { kind = "cancelled", message = "Cancelled" } end
	end
	return nil, fail("Cannot unlock the archive")
end

function A:is_nested(archive) return state_get("nested", archive) ~= nil end

function A:resolve(api, archive)
	local nested = state_get("nested", archive)
	if not nested then return archive end
	local parent, err = api.ensure(nested.parent_archive)
	if not parent then return nil, err end
	if parent.source.revision ~= nested.revision or (parent.edit_time or 0) ~= nested.edit_time then
		return nil, fail("The outer archive changed; reopen the nested archive")
	end
	return nested.path
end

function A:protected(api, archive)
	local paths = {}
	for _ = 1, 8 do
		local nested = state_get("nested", archive)
		if not nested then return paths end
		local parent, err = api.ensure(nested.parent_archive)
		if not parent then return nil, err end
		paths[#paths + 1] = parent.source.archive
		archive = nested.parent_archive
	end
	return paths
end

-- The pending edit session, whose members stand in for the archive's own.
function A:draft(archive, revision)
	local draft = state_get("drafts", archive)
	if draft and draft.revision ~= revision then
		return nil, fail("Archive changed outside the edit session; discard pending changes")
	end
	return draft
end

function A:dirty(archive)
	local draft = state_get("drafts", archive)
	return draft and #draft.changes > 0
end

function A:save(api, archive)
	local draft = state_get("drafts", archive)
	if not draft then return true end
	if #draft.changes == 0 then
		state_set("drafts", archive)
		api.invalidate(archive)
		ya.emit("refresh", {})
		return true
	end
	local index, err = api.rpc { op = "apply", archive = draft.source, revision = draft.revision,
		password = api.password(archive), changes = draft.changes }
	if not index then api.notify(err); return false end
	state_set("drafts", archive)
	api.invalidate(archive)
	ya.emit("refresh", {})
	ya.notify { title = "Zippity", content = "Archive saved.", level = "info", timeout = 4 }
	return true
end

function A:discard(api, archive)
	state_set("drafts", archive)
	api.invalidate(archive)
	ya.emit("refresh", {})
end

function A:before_close(api, archive)
	for key in pairs(state_get("busy")) do
		if not archive or key == archive then api.notify(fail("An archive operation is still running")); return false end
	end
	for key, draft in pairs(state_get("drafts")) do
		if (not archive or key == archive) and #draft.changes > 0 then
			local answer = ya.which { cands = {
				{ on = "s", desc = "Save pending archive changes" },
				{ on = "d", desc = "Discard pending archive changes" },
				{ on = "c", desc = "Cancel closing" },
			} }
			if answer == 1 then if not self:save(api, key) then return false end
			elseif answer == 2 then self:discard(api, key)
			else return false end
		end
	end
	return true
end

function A:close_nested(api, archive)
	for key, nested in pairs(state_get("nested")) do
		if nested.parent_archive == archive then
			self:close_nested(api, key)
			for root in pairs(api.forget(key)) do
				local _, err = api.rpc { op = "clean_cache", directory = root }
				if err then api.notify(err) else api.cache_dirs(root) end
			end
			state_set("nested", key)
		end
	end
end

function A:enter_nested(api, url)
	local item, err = api.locate(url)
	if not item then return api.notify(err) end
	local depth = 1
	local parent = state_get("nested", item.archive)
	if parent then depth = parent.depth + 1 end
	if depth > 8 then return api.notify(fail("Nested archives are limited to eight levels")) end
	local path, materialize_err = api.materialize(url)
	if not path then return api.notify(materialize_err) end
	local visible = tostring(url.path)
	state_set("nested", visible, { path = path, parent = tostring(url), parent_archive = item.archive,
		revision = item.record.source.revision, edit_time = item.record.edit_time or 0, depth = depth })
	local record, open_err = load(api, visible)
	if not record then state_set("nested", visible); return api.notify(open_err) end
	api.announce(record)
	ya.emit("cd", { Url("zippity://archive/" .. visible) })
end

function A:leave(api, cwd)
	if api.is_archive(cwd) then
		local archive, member = api.address(cwd)
		local nested = state_get("nested", archive)
		if nested and member == "" then return ya.emit("reveal", { Url(nested.parent) }) end
	end
	return ya.emit("leave", {})
end

function A:reader(api, item, offset, length)
	local current = state_get("readers", item.archive)
	if not current or current.revision ~= item.record.source.revision or not api.stat_at(Url(current.root)) then
		if current then api.rpc { op = "clean_cache", directory = current.root } end
		-- Nothing reads the reader's stderr here, so it must not fill a pipe and block.
		local child, err = Command("zippity"):arg("--serve"):stdin(Command.PIPED):stdout(Command.PIPED):stderr(Command.NULL):spawn()
		if not child then return nil, fail(tostring(err)) end
		local encoded = ya.json_encode { archive = item.record.source.archive, revision = item.record.source.revision, password = api.password(item.archive) }
		local ok, write_err = child:write_all(encoded .. "\n")
		if ok then ok, write_err = child:flush() end
		if not ok then child:start_kill(); return nil, fail(tostring(write_err)) end
		local line, event = child:read_line()
		local ready = line and ya.json_decode(line)
		if event ~= 0 or not ready or not ready.socket then
			child:start_kill()
			return nil, ready or fail("Cannot start the archive reader")
		end
		ready.revision = item.record.source.revision
		current = ready
		state_set("readers", item.archive, current)
		api.track_cache(current.root, item.record.source.archive)
		ya.hold(child)
	end
	return api.rpc({ op = "read_service", socket = current.socket, member = item.entry.origin or item.member,
		revision = item.record.source.revision, offset = offset, length = length,
		password = api.password(item.archive, item.entry.origin or item.member) }, true)
end

function A:edit(api, archive)
	if self:is_nested(archive) then return api.notify(fail("Nested archives are read-only; extract them before editing")) end
	local record, err = load(api, archive)
	if not record then return api.notify(err) end
	local format = record.source.format
	if #record.source.volumes ~= 1 or (format ~= "zip" and format ~= "7z" and format ~= "rar") then
		return api.notify(fail("Editing supports single-volume ZIP, 7z, and optional RAR archives"))
	end
	if (record.hidden or 0) > 0 then
		return api.notify(fail("Archives with hidden members, such as links, cannot be edited"))
	end
	if record.source.format == "rar" then
		local writers, writer_err = api.rpc { op = "writers" }
		if not writers then return api.notify(writer_err) end
		if not writers.rar then return api.notify(fail("RAR editing needs a separately installed, appropriately licensed rar executable")) end
	end
	if state_get("drafts", archive) then return end
	local entries, plan_err = api.rpc { op = "plan", archive = record.source.archive,
		revision = record.source.revision, password = api.password(archive) }
	if not entries then return api.notify(plan_err) end
	indicator()
	state_set("drafts", archive, { source = record.source.archive, revision = record.source.revision,
		changes = {}, entries = entries, updated = api.seconds(ya.time()) })
	api.invalidate(archive)
	ya.emit("refresh", {})
end

function A:stage(api, archive, additions)
	local draft = state_get("drafts", archive)
	if not draft then return api.notify(fail("Begin an edit session before changing the archive")) end
	for _, change in ipairs(additions) do draft.changes[#draft.changes + 1] = change end
	local entries, err = api.rpc { op = "plan", archive = draft.source, revision = draft.revision,
		password = api.password(archive), changes = draft.changes }
	if not entries then return api.notify(err) end
	draft.entries, draft.updated = entries, api.seconds(ya.time())
	state_set("drafts", archive, draft)
	api.invalidate(archive)
	ya.emit("refresh", {})
end

function A:add(api, archive, prefix, replace)
	local files, cut = api.clipboard()
	if cut then return api.notify(fail("Use copy, not cut, to add archive members")) end
	if #files == 0 then return api.notify(fail("Yank the local files to add first")) end
	local paths = {}
	for _, file in ipairs(files) do
		if not file.spec.is_regular then return api.notify(fail("Add local files, not virtual archive members")) end
		paths[#paths + 1] = tostring(file.path)
	end
	local changes, err = api.rpc { op = "stage", sources = paths, prefix = prefix or "" }
	if not changes then return api.notify(err) end
	if replace then
		if #changes ~= 1 then return api.notify(fail("Replacement needs one regular file")) end
		changes[1].path = replace
	end
	return self:stage(api, archive, changes)
end

function A:paste_into(api, cwd)
	local archive, member = api.address(cwd)
	return self:add(api, archive, member)
end

function A:search(api, archive)
	local record, err = load(api, archive)
	if not record then return api.notify(err) end
	local query = input("Search archive filenames")
	if not query then return end
	local matches = api.search_entries(archive, query)
	if #matches == 0 then return ya.notify { title = "Zippity", content = "No matching members.", timeout = 4 } end
	local keys = { "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j" }
	local page = 0
	while true do
		local cands, paths = {}, {}
		for i = 1, 20 do
			local path = matches[page * 20 + i]
			if not path then break end
			cands[#cands + 1] = { on = keys[i], desc = path }
			paths[#paths + 1] = path
		end
		local previous = #cands + 1
		cands[previous] = { on = "p", desc = "Previous results" }
		local following = #cands + 1
		cands[following] = { on = "n", desc = "Next results" }
		local choice = ya.which { cands = cands }
		if not choice then return end
		if choice == previous then page = math.max(0, page - 1)
		elseif choice == following then if (page + 1) * 20 < #matches then page = page + 1 end
		else return ya.emit("reveal", { Url("zippity://archive/" .. archive):join(paths[choice]) }) end
	end
end

-- Asks for a new archive's format, destination, compression, encryption, and volumes.
-- `default` names the destination for a format. Returns nil when cancelled or refused.
local function ask(api, default)
	local format = ya.which { cands = { { on = "z", desc = "ZIP" }, { on = "7", desc = "7z" }, { on = "r", desc = "RAR, requires separately installed rar" } } }
	if not format then return end
	format = ({ "zip", "7z", "rar" })[format]
	if format == "rar" then
		local writers, err = api.rpc { op = "writers" }
		if not writers then return api.notify(err) end
		if not writers.rar then return api.notify(fail("RAR creation needs a separately installed, appropriately licensed rar executable")) end
	end
	local destination = input("New archive path", default(format))
	if not destination then return end
	-- The last settings for each format are offered again until Yazi restarts.
	local key = "compression:" .. format
	local compression = input("Compression level: 0, 1, 3, 5, 7, 9; then method= dict= word= solid=", state_get("options", key) or "5")
	if not compression then return end
	local options, compression_err = A:compression(compression)
	if not options then return api.notify(fail(compression_err)) end
	state_set("options", key, compression)
	local encrypt = ya.which { cands = { { on = "n", desc = "No encryption" }, { on = "a", desc = "AES encryption" }, { on = "z", desc = "ZipCrypto, ZIP only" } } }
	if not encrypt then return end
	local password
	if encrypt ~= 1 then
		local value, event = ya.input { pos = { "center", w = 50 }, title = "New archive password", obscure = true, history = "" }
		if event ~= 1 then return end
		local repeated, confirmed = ya.input { pos = { "center", w = 50 }, title = "Repeat archive password", obscure = true, history = "" }
		if confirmed ~= 1 then return end
		if value == "" or value ~= repeated then return api.notify(fail("Passwords are empty or do not match")) end
		password = value
	end
	local headers = false
	if password and format ~= "zip" then
		headers = ya.confirm { pos = { "center", w = 50, h = 7 }, title = "Encrypt filenames?", body = "Hide archive filenames until a password is supplied?" }
	end
	local volume = input("Split volume size in MiB, 0 for one file", "0")
	if not volume then return end
	volume = tonumber(volume)
	if not volume or volume < 0 or volume % 1 ~= 0 then return api.notify(fail("Volume size must be a whole number of MiB")) end
	if volume > 0 then
		destination = input("New directory for the complete volume set", destination .. ".parts")
		if not destination then return end
	end
	options.encrypt_headers, options.zip_crypto = headers, encrypt == 3
	options.volume_bytes = volume > 0 and volume * 1024 * 1024 or nil
	state_set("options", "last_format", format)
	return { format = format, destination = destination, password = password, options = options }
end

-- The selected local inputs as paths, with the selection and the current folder.
local function inputs(api)
	local selected, cwd = api.selection()
	if not cwd.spec.is_regular or #selected == 0 then return api.notify(fail("Select local files or folders to archive")) end
	local sources = {}
	for _, url in ipairs(selected) do
		if not url.spec.is_regular then return api.notify(fail("Archive creation requires local inputs")) end
		sources[#sources + 1] = tostring(url.path)
	end
	return sources, selected, cwd
end

function A:create(api)
	local sources, _, cwd = inputs(api)
	if not sources then return end
	local choice = ask(api, function(format) return tostring(cwd:join("archive." .. format)) end)
	if not choice then return end
	local result, err = api.rpc { op = "create", sources = sources, destination = choice.destination, format = choice.format,
		password = choice.password, options = choice.options }
	if not result then return api.notify(err) end
	ya.emit("reveal", { Url(result.path) })
end

-- Archives the selection with the last dialog's format and compression and no password,
-- under a free name such as notes.txt.zip, photos.zip, or archive_1.zip. One item is
-- archived beside itself; several, which may come from other folders, in the current one.
function A:quick(api)
	local sources, selected, cwd = inputs(api)
	if not sources then return end
	local folder = #selected == 1 and selected[1].parent or cwd
	local format = state_get("options", "last_format") or "zip"
	local options = self:compression(state_get("options", "compression:" .. format) or "5")
	local base = #selected == 1 and selected[1].name or "archive"
	for attempt = 0, 99 do
		local name = (attempt == 0 and base or base .. "_" .. attempt) .. "." .. format
		local result, err = api.rpc { op = "create", sources = sources, destination = tostring(folder:join(name)),
			format = format, options = options }
		if result then return ya.emit("reveal", { Url(result.path) }) end
		-- The helper refuses an existing destination, so a taken name moves on to the next.
		if err.kind ~= "exists" then return api.notify(err) end
	end
	api.notify(fail("Too many archives with this name"))
end

-- Writes every member into a new archive: another format, a new or no password, or a copy.
function A:save_as(api, archive)
	if self:is_nested(archive) then return api.notify(fail("Nested archives are read-only; extract them before saving a copy")) end
	local record, err = load(api, archive)
	if not record then return api.notify(err) end
	-- A draft is kept under the archive's resolved path, as when it was opened through a link.
	if self:dirty(archive) or self:dirty(record.source.archive) then
		return api.notify(fail("Save or discard the pending changes first"))
	end
	if record.encrypted and not api.password(archive) and not api.prompt(archive) then return end
	local source = record.source.archive
	local folder, name = source:match("^(.*)/([^/]+)$")
	local stem = folder .. "/" .. self:archive_stem(name)
	local choice = ask(api, function(format)
		local path = stem .. "." .. format
		return path == source and stem .. "-copy." .. format or path
	end)
	if not choice then return end
	if record.encrypted and not choice.password and not ya.confirm { pos = { "center", w = 55, h = 8 },
		title = "Save without a password?", body = "The new archive will not be encrypted. The source stays unchanged." } then return end
	local result, rpc_err = api.rpc { op = "convert", archive = source, revision = record.source.revision,
		password = api.password(archive), destination = choice.destination, format = choice.format,
		options = choice.options, new_password = choice.password }
	if not result then return api.notify(rpc_err) end
	ya.emit("reveal", { Url(result.path) })
end

function A:entry(api, action)
	if action == "create" then return self:create(api) end
	if action == "quick" then return self:quick(api) end
	local archive, hovered, cwd = archive_at(api)
	if not archive then return end
	if action == "menu" then
		local choice = ya.which { cands = {
			{ on = "f", desc = "Search all member names" }, { on = "t", desc = "Check archive integrity" },
			{ on = "e", desc = "Begin edit session" }, { on = "a", desc = "Add yanked local files" },
			{ on = "r", desc = "Rename hovered member" }, { on = "d", desc = "Stage selected member deletion" },
			{ on = "p", desc = "Replace hovered member with yanked file" }, { on = "s", desc = "Save pending changes" },
			{ on = "x", desc = "Discard pending changes" }, { on = "m", desc = "Toggle extraction metadata preservation" },
			{ on = "k", desc = "Toggle partial copies of damaged members" }, { on = "c", desc = "Save as a new archive" },
		} }
		if not choice then return end
		action = ({ "search", "check", "edit", "add", "rename", "delete", "replace", "save", "discard", "metadata", "damaged", "save_as" })[choice]
	end
	if action == "save_as" then return self:save_as(api, archive) end
	if action == "search" then return self:search(api, archive) end
	if action == "edit" then return self:edit(api, archive) end
	if action == "save" then return self:save(api, archive) end
	if action == "discard" then
		if self:dirty(archive) and not ya.confirm { pos = { "center", w = 55, h = 8 }, title = "Discard archive changes?", body = "Pending changes will be lost. The source archive stays unchanged." } then return end
		return self:discard(api, archive)
	end
	if action == "metadata" then
		local enabled = not state_get("options", "preserve_metadata")
		state_set("options", "preserve_metadata", enabled)
		return ya.notify { title = "Zippity", content = enabled and "Extraction preserves safe permissions and modification times." or "Extraction uses private file permissions.", level = "info", timeout = 5 }
	end
	if action == "damaged" then
		local enabled = not state_get("options", "keep_damaged")
		state_set("options", "keep_damaged", enabled)
		return ya.notify { title = "Zippity", content = enabled and "Paste saves what it can decode of a damaged member as name.damaged. Those copies are not verified."
			or "Paste skips damaged members without partial copies.", level = "info", timeout = 5 }
	end
	local record, err = load(api, archive)
	if not record then return api.notify(err) end
	if action == "check" then
		local _, check_err = api.rpc { op = "check", archive = record.source.archive, revision = record.source.revision, password = api.password(archive) }
		if check_err then return api.notify(check_err) end
		return ya.notify { title = "Zippity", content = self:dirty(archive) and "Source archive verified. Pending changes are checked when saved." or "Archive integrity verified.", level = "info", timeout = 5 }
	end
	if action == "add" then
		-- Outside an archive, additions go to its root.
		return self:add(api, archive, api.is_archive(cwd) and select(2, api.address(cwd)) or "")
	end
	if not hovered or not api.is_archive(hovered) then return api.notify(fail("Select an archive member")) end
	local _, member = api.address(hovered)
	if action == "replace" then return self:add(api, archive, "", member) end
	if action == "rename" then
		local target = input("New member path", member)
		if target then return self:stage(api, archive, { { kind = "rename", from = member, to = target } }) end
	end
	if action == "delete" then
		local selected = api.selection()
		local changes = {}
		for _, url in ipairs(selected) do
			if not api.is_archive(url) then return api.notify(fail("Select members from this archive only")) end
			local owner, path = api.address(url)
			if owner ~= archive then return api.notify(fail("Select members from this archive only")) end
			changes[#changes + 1] = { kind = "delete", path = path }
		end
		if ya.confirm { pos = { "center", w = 55, h = 8 }, title = "Stage archive deletion?", body = "The source changes only after Save." } then self:stage(api, archive, changes) end
	end
end

function A:preserve_metadata() return state_get("options", "preserve_metadata") == true end
function A:keep_damaged() return state_get("options", "keep_damaged") == true end

local function unsuffixed(name)
	local lower = name:lower()
	-- name.part1.rar and name.z01 lose the whole suffix; name.7z.001 keeps .7z for below.
	local at = lower:find("%.part%d+%.rar$", 2) or lower:find("%.z%d%d+$", 2)
	if at then return name:sub(1, at - 1) end
	at = lower:find("%.%d%d%d+$", 2)
	if at then name, lower = name:sub(1, at - 1), lower:sub(1, at - 1) end
	for _, suffix in ipairs { ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lzma", ".tar.z", ".tar.lz4" } do
		if #lower > #suffix and lower:sub(-#suffix) == suffix then return name:sub(1, -#suffix - 1) end
	end
	return name:match("^(.+)%.[^.]+$") or name
end

-- Reads "level [method=M] [dict=D] [word=W] [solid=S]" into creation options.
function A:compression(text)
	local level, rest = text:match("^%s*(%d+)(.*)$")
	if not level then return nil, "Start with a compression level" end
	local options, fields = { level = tonumber(level) }, { method = "method", dict = "dictionary", word = "word_size", solid = "solid" }
	for token in rest:gmatch("%S+") do
		local name, value = token:match("^(%a+)=(.+)$")
		local field = fields[name or ""]
		if not field then return nil, "Unknown compression setting: " .. token end
		-- Word sizes reach 273 at most; the helper checks each method's range.
		if field == "word_size" then
			if not value:match("^%d%d?%d?$") then return nil, "Word size must be a whole number" end
			value = tonumber(value)
		end
		options[field] = type(value) == "string" and value:lower() or value
	end
	return options
end

-- The archive's name without its archive extensions or volume number.
function A:archive_stem(name)
	local stem = unsuffixed(name)
	-- A stem of only dots would name the archive's own folder or its parent.
	return stem:find("[^.]") and stem or name
end

for _, name in ipairs({ "edit", "stage", "save", "discard" }) do
	local operation = A[name]
	A[name] = function(self, api, archive, ...)
		if not claim(archive) then api.notify(fail("An archive operation is already running")); return false end
		local ok, result = pcall(operation, self, api, archive, ...)
		state_set("busy", archive)
		if not ok then api.notify(fail(tostring(result))); return false end
		return result
	end
end

return A
