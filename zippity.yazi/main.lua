--- @since 26.9.1

local M = {}
local api = {}

-- Yazi's nightly builds call Cha Stat, return times as Time values, and stream directory
-- listings and reads. Yazi 26.9.1 does none of that.
local NIGHTLY = rawget(_G, "Stat") ~= nil
local Stat = rawget(_G, "Stat") or rawget(_G, "Cha")
-- The most the helper's reader returns for one request.
local READ_CHUNK = 1024 * 1024

local function seconds(time) return type(time) == "number" and time or time and time.unix end
local function stat_of(file) return NIGHTLY and file.stat or file.cha end
local function stat_at(...) return (fs.stat or fs.cha)(...) end

-- Members with these names open as nested archives without a content probe. Every
-- other file opens when the helper recognizes its content.
local SUFFIXES = {}
for suffix in ([[zip zipx jar apk aar cbz epub xpi ipa appx rar cbr 7z tar tgz tbz tbz2 txz
	tzst taz tar.gz tar.bz2 tar.xz tar.zst tar.lzma tar.z tar.lz4 gz bz2 xz zst lzma z lz4 iso a ar deb udeb arj cab chm cpio cramfs dmg exe lzh lha
	msi msp msm pkg xar xip rpm wim swm esd squashfs img fat ntfs ext ext2 ext3 ext4 hfs hfsx
	apfs gpt qcow qcow2 vdi vhd vhdx vmdk simg sit sitx sea lzx]]):gmatch("%S+") do SUFFIXES[suffix] = true end

local function supported(name)
	name = name:lower()
	if name:match("%.[r-z]%d%d$") or name:match("%.00%d$") then return true end
	return SUFFIXES[name:match("%.([^.]+%.[^.]+)$") or ""] or SUFFIXES[name:match("%.([^.]+)$") or ""] or false
end
local DIRECTORY_MODE = tonumber("40555", 8)
local FILE_MODE = tonumber("100444", 8)

local context = ya.sync(function()
	local file = cx.active.current.hovered
	return file and file.url, file and stat_of(file).is_dir, cx.active.current.cwd
end)

local clipboard = ya.sync(function()
	local files = {}
	for _, current in pairs(cx.yanked) do
		files[#files + 1] = current.url
	end
	return files, cx.yanked.is_cut
end)

local selection = ya.sync(function()
	local files = {}
	for _, file in pairs(cx.active.selected) do files[#files + 1] = file.url end
	if #files == 0 and cx.active.current.hovered then files[1] = cx.active.current.hovered.url end
	return files, cx.active.current.cwd
end)

local invalidate = ya.sync(function(state, archive)
	local record = (state.archives or {})[archive]
	for key, candidate in pairs(state.archives or {}) do
		if key == archive or (record and candidate.source.archive == record.source.archive) then state.archives[key] = nil end
	end
end)

local search_entries = ya.sync(function(state, archive, query)
	local found, needle = {}, query:lower()
	local record = (state.archives or {})[archive]
	for name in pairs(record and record.entries or {}) do
		if name ~= "" and name:lower():find(needle, 1, true) then found[#found + 1] = name end
	end
	table.sort(found)
	return found
end)

local track_cache = ya.sync(function(state, root, owner)
	state.cache_dirs = state.cache_dirs or {}
	state.cache_dirs[root] = owner
end)

local password = ya.sync(function(state, archive, member)
	local passwords = (state.passwords or {})[archive] or {}
	return passwords[member or ""] or passwords[""]
end)

local remember = ya.sync(function(state, archive, member, value)
	state.passwords = state.passwords or {}
	state.passwords[archive] = state.passwords[archive] or {}
	state.passwords[archive][member or ""] = value
end)

local automatic_preview = ya.sync(function(state, archive, toggle)
	state.auto_previews = state.auto_previews or {}
	if toggle then state.auto_previews[archive] = not state.auto_previews[archive] or nil end
	return state.auto_previews[archive] == true
end)

local info = ya.sync(function(state, archive)
	local record = (state.archives or {})[archive]
	if record then
		return { source = record.source, stamps = record.stamps, encrypted = record.encrypted, hidden = record.hidden,
			preview_limit = record.preview_limit, cache = record.cache, cache_updated = record.cache_updated, edit_time = record.edit_time }
	end
end)

-- Yazi's nightly builds cancel file system jobs they no longer need, and a cancelled
-- loader never stores its listing. Its claim lapses, and another caller lists instead.
local CLAIM_SECONDS = 5

local claim = ya.sync(function(state, archive)
	state.loading = state.loading or {}
	local now = seconds(ya.time())
	if (state.loading[archive] or 0) > now then
		return false
	end
	state.loading[archive] = now + CLAIM_SECONDS
	return true
end)

-- Builds an archive's record on Yazi's main thread from the helper's line format, or from
-- the members of a pending edit session. Handing the members over as one string keeps
-- ya.sync from converting them one value at a time. Without a head it only releases the
-- claim on the archive.
local store = ya.sync(function(state, archive, head, body, stamps, draft)
	state.loading[archive] = nil
	if not head then
		return
	end
	local record = { source = head.source, edit_time = draft and draft.updated, preview_limit = head.preview_limit,
		hidden = head.hidden or 0, stamps = stamps, entries = { [""] = { path = "", directory = true, size = 0 } },
		children = {}, encrypted = false }
	local function add(entry)
		record.entries[entry.path] = entry
		record.encrypted = record.encrypted or entry.encrypted
		local parent = entry.path:match("^(.*)/[^/]+$") or ""
		local siblings = record.children[parent]
		if not siblings then
			siblings = {}
			record.children[parent] = siblings
		end
		siblings[#siblings + 1] = entry.path
	end
	if draft then
		for _, entry in ipairs(draft.entries) do add(entry) end
	else
		for path, flags, size, modified, mode, link in body:gmatch("([^\t\n]*)\t(%d+)\t(%d+)\t([^\t\n]*)\t(%d*)\t([^\n]*)\n") do
			flags = tonumber(flags)
			add { path = path, directory = flags & 1 ~= 0, unknown_size = flags & 2 ~= 0 or nil, encrypted = flags & 4 ~= 0,
				finder = flags & 8 ~= 0 or nil, size = tonumber(size), modified = tonumber(modified), mode = tonumber(mode),
				link = link ~= "" and link or nil }
		end
	end
	state.archives = state.archives or {}
	local previous = state.archives[archive]
	if previous and previous.source.revision == record.source.revision then
		record.cache, record.cache_updated = previous.cache, previous.cache_updated
		for path, entry in pairs(record.entries) do
			local old = previous.entries[path]
			if old then entry.backing, entry.cached_at = old.backing, old.cached_at end
		end
	end
	state.archives[archive] = record
	state.archives[record.source.archive] = record
	state.passwords = state.passwords or {}
	state.passwords[record.source.archive] = state.passwords[archive] or state.passwords[record.source.archive]
end)

local lookup = ya.sync(function(state, archive, member, children)
	local record = (state.archives or {})[archive]
	if record then
		if not children then return record.entries[member] end
		local result = {}
		for _, path in ipairs(record.children[member] or {}) do
			result[#result + 1] = record.entries[path]
		end
		return result
	end
end)

-- The members inside `member`, by folder, for planning a copy in one round trip.
local subtree = ya.sync(function(state, archive, member)
	local record = (state.archives or {})[archive]
	local result, pending = {}, { member }
	while record and #pending > 0 do
		local folder = table.remove(pending)
		local list = {}
		for _, path in ipairs(record.children[folder] or {}) do
			local entry = record.entries[path]
			list[#list + 1] = entry
			if entry.directory then pending[#pending + 1] = path end
		end
		result[folder] = list
	end
	return result
end)

local secrets = ya.sync(function(state, archive)
	return (state.passwords or {})[archive] or {}
end)

local forget = ya.sync(function(state, archive)
	local source = ((state.archives or {})[archive] or {}).source
	local roots, mimes = {}, {}
	local canonical = source and source.archive or archive
	for path, owner in pairs(state.cache_dirs or {}) do
		if owner == canonical then roots[path] = true end
	end
	for key, record in pairs(state.archives or {}) do
		if key == archive or (source and record.source.archive == source.archive) then
			for member, entry in pairs(record.entries or {}) do
				if entry.backing then mimes[Url("zippity://archive/" .. key):join(member)] = "zippity/member" end
			end
			if record.cache then roots[record.cache] = true end
			state.archives[key] = nil
			if state.passwords then state.passwords[key] = nil end
			if state.auto_previews then state.auto_previews[key] = nil end
		end
	end
	if state.passwords then state.passwords[archive] = nil end
	if state.auto_previews then state.auto_previews[archive], state.auto_previews[canonical] = nil, nil end
	if next(mimes) then ya.emit("update_mimes", { updates = mimes }) end
	return roots
end)

local cache_root = ya.sync(function(state, archive, revision, root)
	local record = (state.archives or {})[archive]
	if not record or record.source.revision ~= revision then return nil end
	if root and not record.cache then
		record.cache = root
		state.cache_dirs = state.cache_dirs or {}
		state.cache_dirs[root] = record.source.archive
		if not state.cleanup_registered then
			state.cleanup_registered = true
			ps.sub("bye", function()
				for path in pairs(state.cache_dirs or {}) do
					os.execute("zippity --clean-cache " .. ya.quote(path))
				end
			end)
		end
	end
	return record.cache
end)

local preview_path = ya.sync(function(state, root, member)
	state.preview_sequence = (state.preview_sequence or 0) + 1
	-- The member's extensions let a nested archive or a previewer recognize the copy.
	local suffix = (member:match("[^/]*$") or ""):match("^[^.]*(%..*)$") or ""
	if #suffix > 32 or suffix:find("[^%w._-]") then suffix = "" end
	return root .. "/member-" .. state.preview_sequence .. suffix
end)

local set_backing = ya.sync(function(state, archive, member, root, path, previous)
	local record = (state.archives or {})[archive]
	if not record or record.cache ~= root then return nil end
	local entry = record.entries[member]
	if not entry then return nil end
	if entry.backing ~= previous then return entry.backing end
	entry.backing, entry.cached_at = path, seconds(ya.time())
	record.cache_updated = entry.cached_at
	return path
end)

local cache_dirs = ya.sync(function(state, removed)
	state.cache_dirs = state.cache_dirs or {}
	if removed then state.cache_dirs[removed] = nil end
	return state.cache_dirs
end)

local clear_session = ya.sync(function(state)
	state.archives, state.passwords, state.auto_previews = {}, {}, {}
end)

local function failure(kind, message)
	return { kind = kind, message = message }
end

local function fs_error(err)
	local kind = err.kind == "password" and "PermissionDenied" or "Other"
	local message = err.message
	if err.kind == "password" then
		message = message .. ". Run plugin zippity -- unlock, then retry"
	end
	return Error.fs { kind = kind, message = message }
end

local function notify(err)
	if err and err.kind ~= "cancelled" then
		ya.notify { title = "Zippity", content = err.message, level = "error", timeout = 8 }
	end
end

-- The plugin sends only requests its own helper accepts, so a rejected request means an
-- older helper on PATH.
local function helper_error(err)
	if err and err.kind == "request" and err.message == "Invalid JSON request" then
		return failure("dependency", "The zippity helper on PATH is older than the plugin; install both from the same release")
	end
	return err
end

local function spawn(request)
	local encoded, err = ya.json_encode(request)
	if not encoded then
		return nil, failure("request", tostring(err))
	end
	local child, spawn_err = Command("zippity"):stdin(Command.PIPED):stdout(Command.PIPED):stderr(Command.PIPED):spawn()
	if not child then
		return nil, failure("dependency", "Cannot start zippity; install the Rust helper on PATH: " .. tostring(spawn_err))
	end
	local ok, write_err = child:write_all(encoded)
	if ok then ok, write_err = child:flush() end
	ya.drop(child:take_stdin())
	if not ok then
		child:start_kill()
		return nil, failure("io", tostring(write_err))
	end
	return child
end

local function rpc(request, raw)
	local child, err = spawn(request)
	if not child then
		return nil, err
	end
	local output, wait_err = child:wait_with_output()
	if not output then
		return nil, failure("io", tostring(wait_err))
	end
	if not output.status.success then
		return nil, helper_error(ya.json_decode(output.stderr)) or failure("io", "Archive helper failed")
	end
	if raw then
		return output.stdout
	end
	local result, decode_err = ya.json_decode(output.stdout)
	return result, not result and failure("io", tostring(decode_err)) or nil
end

local function stamp(path)
	local stat = stat_at(Url(path))
	return stat and { path = path, size = stat.len, mtime = seconds(stat.mtime), ctime = seconds(stat.ctime) }
end

local function fresh(record)
	for _, before in ipairs(record.stamps) do
		local after = stamp(before.path)
		if not after or before.size ~= after.size or before.mtime ~= after.mtime or before.ctime ~= after.ctime then
			return false
		end
	end
	return true
end

local function ensure(archive)
	local real, resolve_err = require(".actions"):resolve(api, archive)
	if not real then return nil, resolve_err end
	for _ = 1, 1300 do
		local record = info(archive)
		if record and fresh(record) then
			return record
		end
		if claim(archive) then
			local text, err = rpc({ op = "list", archive = real, password = password(archive), lines = true }, true)
			local split = text and text:find("\n", 1, true)
			local head = split and ya.json_decode(text:sub(1, split - 1))
			if not head then
				store(archive)
				return nil, err or failure("io", "Cannot read the archive index")
			end
			-- Only multipart names depend on their folder; other archives ignore files created beside them.
			local paths, stamps = {}, {}
			if head.source.multipart then paths[1] = tostring(Url(head.source.archive).parent) end
			for _, volume in ipairs(head.source.volumes) do
				paths[#paths + 1] = volume.path
			end
			for _, path in ipairs(paths) do
				local current = stamp(path)
				if not current then
					store(archive)
					return nil, failure("changed", "Archive volumes changed; reopen the archive")
				end
				stamps[#stamps + 1] = current
			end
			-- An edit session lives in the actions module's own state.
			local draft, draft_err = require(".actions"):draft(archive, head.source.revision)
			if draft_err then
				store(archive)
				return nil, draft_err
			end
			store(archive, head, not draft and text:sub(split + 1) or "", stamps, draft)
			return info(archive)
		end
		ya.sleep(0.05)
	end
	return nil, failure("busy", "Archive index is busy; retry the operation")
end

local function is_archive(url)
	return url and url.spec.scheme == "zippity" and url.spec.domain == "archive"
end

local function address(url)
	local base = tostring(url.base.path)
	local member = tostring(url.path):sub(#base + 2)
	return base, member
end

local function locate(url)
	local archive, member = address(url)
	local record, err = ensure(archive)
	if not record then
		return nil, err
	end
	local entry = lookup(archive, member)
	if not entry then
		return nil, failure("missing", "Archive member is missing; reopen the archive")
	end
	return { archive = archive, member = member, record = record, entry = entry }
end

local function file(url, entry, record)
	local changed, modified = record.edit_time or 0, record.edit_time or 0
	for _, volume in ipairs(record.source.volumes) do
		changed = math.max(changed, volume.changed_seconds + volume.changed_nanoseconds / 1e9)
		modified = math.max(modified, volume.modified_seconds + volume.modified_nanoseconds / 1e9)
	end
	if entry.directory then
		changed = math.max(changed, record.cache_updated or 0)
		modified = math.max(modified, record.cache_updated or 0)
	else
		changed = math.max(changed, entry.cached_at or 0)
	end
	local name = entry.path:match("[^/]+$") or ""
	local stat = Stat {
		mode = entry.directory and DIRECTORY_MODE or FILE_MODE,
		kind = name:sub(1, 1) == "." and 2 or 0,
		len = entry.size,
		ctime = changed,
		mtime = entry.directory and math.max(entry.modified or 0, modified) or (entry.modified or modified),
	}
	-- Yazi 26.9.1 reads cha. Nightly reads stat and lstat, which are the same for members.
	return File { url = url, backing = entry.backing and Path.os(entry.backing), cha = stat, stat = stat, lstat = stat }
end

-- Says once per opening how many members the listing hides.
local function announce(record)
	if (record.hidden or 0) > 0 then
		ya.notify { title = "Zippity", level = "info", timeout = 6, content = string.format(
			"%d %s hidden: links, special files, or names zippity cannot use safely. They are never extracted.",
			record.hidden, record.hidden == 1 and "member is" or "members are") }
	end
end

local function prompt(archive, member)
	local value, event = ya.input {
		pos = { "center", w = 50 }, title = "Archive password", obscure = true, history = "",
	}
	if event ~= 1 then
		return false
	end
	remember(archive, member, value)
	return true
end

function M:entry(job)
	local url, directory, cwd = context()
	local action = job.args[1] or "enter"
	if action == "leave" then return require(".actions"):leave(api, cwd) end
	if action == "auto-preview" then
		if not is_archive(cwd) then return ya.emit("forward", {}) end
		local enabled = automatic_preview(address(cwd), true)
		ya.notify { title = "Zippity", content = enabled and "Automatic previews enabled for this archive. Large files still need confirmation."
			or "Automatic previews disabled. Cached previews remain until Lock or Quit.", level = "info", timeout = 5 }
		return ya.emit("peek", { force = true })
	end
	if ({ menu=true, search=true, check=true, edit=true, add=true, rename=true, delete=true, replace=true, save=true, discard=true, create=true, quick=true, metadata=true, damaged=true })[action] then
		return require(".actions"):entry(api, action)
	end
	if action == "paste" or action == "paste-force" then
		return self:paste(cwd, action == "paste-force")
	end
	if action == "extract" then return self:extract_archives(selection()) end
	if action == "quit" or action == "quit-no-cwd" then
		if not require(".actions"):before_close(api) then return end
		for root in pairs(cache_dirs()) do
			local _, err = rpc { op = "clean_cache", directory = root }
			if err then notify(err) else cache_dirs(root) end
		end
		clear_session()
		return ya.emit("quit", { no_cwd_file = action == "quit-no-cwd" or is_archive(cwd) })
	end
	if action == "lock" then
		local target = is_archive(cwd) and cwd or url
		if target then
			local archive = is_archive(target) and address(target) or tostring(target.path)
			if not require(".actions"):before_close(api, archive) then return end
			require(".actions"):close_nested(api, archive)
			local failed = false
			for root in pairs(forget(archive)) do
				local _, err = rpc { op = "clean_cache", directory = root }
				if err then failed = true; notify(err) else cache_dirs(root) end
			end
			ya.emit("reveal", { Url(archive) })
			ya.notify { title = "Zippity",
				content = failed and "Password forgotten. Some member caches could not be removed."
					or "Password forgotten and member cache removed. Previewer caches may remain.",
				level = failed and "warn" or "info", timeout = 6 }
		end
		return
	end
	if action == "unlock" and not url and is_archive(cwd) then
		url, directory = cwd, true
	end
	if not url then
		return
	end
	if action == "cut" then
		if is_archive(url) then
			return notify(failure("read_only", "Archive members are read-only; use copy instead of cut"))
		end
		return ya.emit("yank", { cut = true })
	end
	if action == "unlock" then
		local archive, member
		if is_archive(url) then
			archive, member = address(url)
		else
			archive = tostring(url.path)
		end
		if directory then member = nil end
		if prompt(archive, member) then
			ya.notify { title = "Zippity", content = "Password stored for this session. Retry the preview or copy.", level = "info", timeout = 5 }
		end
		return
	end
	if is_archive(url) and not directory then
		if action == "enter" and supported(url.name or "") then
			return require(".actions"):enter_nested(api, url)
		end
		local path, err = self:materialize(url)
		if err then return notify(err) end
		-- A member that is an archive by content opens like one; others stay previews.
		if action == "enter" then
			local probe, probe_err = rpc { op = "probe", path = path }
			if probe and probe.archive then return require(".actions"):enter_nested(api, url) end
			if not probe then notify(probe_err) end
		end
		return ya.emit("refresh", {})
	end
	if directory then
		return ya.emit("enter", {})
	end
	if not url.spec.is_regular then
		return notify(failure("unsupported", "Open a local archive"))
	end
	local archive = tostring(url.path)
	local record, err
	for _ = 1, 3 do
		record, err = ensure(archive)
		if record or err.kind ~= "password" then break end
		if not prompt(archive) then return end
	end
	-- Files open by content; an ordinary file keeps Yazi's own enter.
	if not record and err.kind == "not_archive" and action == "enter" then
		return ya.emit("enter", {})
	end
	if not record then return notify(err) end
	announce(record)
	if record.encrypted and not password(record.source.archive) then
		prompt(record.source.archive)
	end
	ya.emit("cd", { Url("zippity://archive/" .. record.source.archive) })
end

function M:materialize(url, automatic)
	local item, err = locate(url)
	if not item then return nil, err end
	if item.entry.link then return nil, failure("link", "Symbolic link to " .. item.entry.link) end
	if item.entry.encrypted and not password(item.archive, item.entry.origin or item.member) then
		if automatic then return nil, failure("password", "Press Alt-u to unlock this member, then retry the preview.") end
		if not prompt(item.archive) then return nil, failure("cancelled", "Preview cancelled") end
	end
	if item.entry.backing and stat_at(Url(item.entry.backing)) then return item.entry.backing end
	if item.entry.unknown_size or item.entry.size > item.record.preview_limit then
		if automatic then return nil, failure("limit", "Large member: press Alt-p or l to confirm previewing it.") end
		local body = item.entry.unknown_size and "Its size is unknown until it is decompressed. This writes it to a private cache. Extraction does not need this preview copy."
			or string.format("This writes %.1f MiB to a private cache. Extraction does not need this preview copy.", item.entry.size / 1024 / 1024)
		if not ya.confirm {
			pos = { "center", w = 60, h = 9 }, title = "Preview large archive member?", body = body,
		} then return nil, failure("cancelled", "Preview cancelled") end
	end
	local root = cache_root(item.archive, item.record.source.revision)
	if not root then
		local created, create_err = rpc { op = "cache" }
		if not created then return nil, create_err end
		root = cache_root(item.archive, item.record.source.revision, created.path)
		if root ~= created.path then rpc { op = "clean_cache", directory = created.path } end
		if not root then return nil, failure("changed", "Archive was locked or changed") end
	end
	local previous = item.entry.backing
	local target = preview_path(root, item.member)
	-- The member cache also keeps the archive's listing, so the next preview skips listing it.
	local size, copy_err = self:extract { from = url, to = Path.os(target), overwrite = false, cache = root }
	if size == nil then return nil, failure("preview", tostring(copy_err)) end
	local backing = set_backing(item.archive, item.member, root, target, previous)
	if backing ~= target then fs.remove("file", Url(target)) end
	if not backing then return nil, failure("changed", "Archive was locked or changed") end
	return backing
end

-- No links, trash, or copies by the provider. Both Yazi versions read an empty table as
-- every option off, and nightly rejects the names Yazi 26.9.1 used.
function M:Capabilities() return {} end

function M:ReadDir(job)
	local item, err = locate(job.url)
	if item and not item.entry.directory then item, err = nil, failure("invalid", "Archive member is not a directory") end
	local files = {}
	for _, entry in ipairs(item and lookup(item.archive, item.member, true) or {}) do
		files[#files + 1] = file(job.url:join(entry.path:match("[^/]+$")), entry, item.record)
	end
	-- Nightly takes the listing, and any error, as a stream.
	if NIGHTLY then
		return ya.co(function()
			if not item then return nil, fs_error(err) end
			for _, current in ipairs(files) do coroutine.yield(current) end
		end)
	end
	if not item then return nil, fs_error(err) end
	for i, current in ipairs(files) do files[i] = { file = current, cha = current.cha } end
	return files
end

function M:File(job)
	local item, err = locate(job.url)
	if not item then return nil, fs_error(err) end
	return file(job.url, item.entry, item.record)
end

function M:Metadata(job)
	local result, err = self:File(job)
	return result and stat_of(result), err
end
M.SymlinkMetadata = M.Metadata

function M:Revalidate(job)
	local result, err = self:File { url = job.file.url }
	if result and stat_of(result):hash(true) == stat_of(job.file):hash(true) then return nil end
	return result, err
end

function M:Open(job)
	local demand = job.demand
	if demand.write or demand.append or demand.truncate or demand.create or demand.create_new then
		return nil, Error.fs { kind = "PermissionDenied", message = "Archive members are read-only" }
	end
	local item, err = locate(job.url)
	if not item then return nil, fs_error(err) end
	if item.entry.directory then return nil, fs_error(failure("invalid", "Cannot read an archive directory as a file")) end
	if item.entry.unknown_size then
		return nil, fs_error(failure("unsupported", "This member's size is unknown until it is decompressed; copy it with y and p"))
	end
	-- Nightly takes a handle, and the helper's reader starts at any offset.
	return NIGHTLY and { id = 1, random = "read" } or 0
end

function M:Close() return true end

local function read_range(item, offset, length)
	if item.entry.input then
		return rpc({ op = "read_input", change = { kind="put", path=item.member, source=item.entry.input, stamp=item.entry.stamp }, offset=offset, length=length }, true)
	end
	return require(".actions"):reader(api, item, offset, length)
end

function M:Read(job)
	local item, err = locate(job.url)
	-- Yazi 26.9.1 asks for one range at a time.
	if job.len then
		if not item then return nil, fs_error(err) end
		local bytes, read_err = read_range(item, job.offset, job.len)
		return bytes, read_err and fs_error(read_err)
	end
	-- Nightly streams the member from an offset for as long as Yazi keeps reading.
	return ya.co(function()
		if not item then return nil, fs_error(err) end
		local offset, size = job.offset.value, item.entry.size
		while offset < size do
			local bytes, read_err = read_range(item, offset, math.min(READ_CHUNK, size - offset))
			if not bytes then return nil, fs_error(read_err) end
			if #bytes == 0 then return nil, fs_error(failure("io", "Archive member ended early")) end
			coroutine.yield(bytes)
			offset = offset + #bytes
		end
	end)
end

-- Runs a copy request and returns the helper's final report.
local function copied(request)
	local child, spawn_err = spawn(request)
	if not child then return nil, spawn_err end
	local done, copy_err
	while true do
		local line, event = child:read_line()
		if event == 2 then break end
		if not line then
			child:start_kill()
			return nil, failure("io", "Cannot read extraction progress")
		end
		local value = ya.json_decode(line)
		if event == 1 then
			copy_err = helper_error(value) or failure("io", "Archive helper failed")
		elseif value and value.done then
			done = value
		end
	end
	local status = child:wait()
	if copy_err or not status or not status.success or not done then
		return nil, copy_err or failure("io", "Extraction did not complete")
	end
	return done
end

function M:extract(job)
	local item, err = locate(job.from)
	if not item then return nil, fs_error(err) end
	local protected, protected_err = require(".actions"):protected(api, item.archive)
	if not protected then return nil, fs_error(protected_err) end
	local request = { op = "copy", archive = item.record.source.archive, member = item.entry.origin or item.member,
		revision = item.record.source.revision, password = password(item.archive, item.entry.origin or item.member),
		destination = tostring(job.to), overwrite = job.overwrite ~= false, protected = #protected > 0 and protected or nil,
		preserve_metadata = job.preserve_metadata == true, cache = job.cache }
	if item.entry.input then
		request.op, request.member, request.password, request.cache = "copy_input", nil, nil, nil
		request.change = { kind="put", path=item.member, source=item.entry.input, stamp=item.entry.stamp }
	end
	local report, copy_err = copied(request)
	if not report or (report.done ~= item.entry.size and not item.entry.unknown_size) then
		return nil, fs_error(copy_err or failure("io", "Extraction did not complete"))
	end
	return report.done
end

-- Picks a free name, or the target itself when forced. `taken` holds names already planned.
local function destination(target, directory, force, taken)
	local parent, name = target.parent, target.name
	local stem = directory and name or target.stem
	local extension = not directory and target.ext and ("." .. target.ext) or ""
	for attempt = 0, 10000 do
		-- Nightly's Url() takes over a Url it receives, so pass Urls on as they are.
		local candidate = attempt == 0 and target or parent:join(stem .. "_" .. attempt .. extension)
		local existing, err = stat_at(candidate, false)
		if not taken[tostring(candidate)] and (force or not existing) then
			if not existing and err and err.kind ~= "NotFound" then return nil, err end
			return candidate, nil, existing
		end
	end
	return nil, fs_error(failure("limit", "Too many destination name collisions"))
end

function M:paste(cwd, force)
	if is_archive(cwd) then return require(".actions"):paste_into(api, cwd) end
	local files, cut = clipboard()
	local archives = 0
	for _, source in ipairs(files) do
		if is_archive(source) then archives = archives + 1 end
	end
	if archives == 0 then return ya.emit("paste", { force = force }) end
	if archives ~= #files then
		return notify(failure("unsupported", "Copy archive members separately from ordinary files"))
	end
	if cut then return notify(failure("read_only", "Archive members cannot be cut; use copy")) end
	if not cwd.spec.is_regular then
		return notify(failure("unsupported", "Extract archive members into a local directory"))
	end
	local targets = {}
	for _, source in ipairs(files) do targets[#targets + 1] = { source = source, target = cwd:join(source.name) } end
	return self:unpack(targets, force)
end

-- Unpacks whole archives beside themselves. A single top-level member lands as it is;
-- several go into a folder named after the archive. Free names keep existing files.
function M:extract_archives(files)
	-- Every archive opens before any folder is made, so a later failure leaves none behind.
	local plans = {}
	for _, url in ipairs(files) do
		if not url.spec.is_regular then
			return notify(failure("unsupported", "Extract whole archives from a local directory"))
		end
		local archive = tostring(url.path)
		local record, err
		for _ = 1, 3 do
			record, err = ensure(archive)
			if record or err.kind ~= "password" then break end
			if not prompt(archive) then return end
		end
		if not record then return notify(err) end
		-- Members encrypted under plain headers list without a password but cannot be copied.
		if record.encrypted and not password(record.source.archive) and not prompt(record.source.archive) then return end
		local top = lookup(archive, "", true) or {}
		if #top == 0 then return notify(failure("empty", "The archive has no members to extract")) end
		plans[#plans + 1] = { url = url, record = record, top = top }
	end
	local targets = {}
	for _, plan in ipairs(plans) do
		announce(plan.record)
		local into = plan.url.parent
		if #plan.top > 1 then
			local name = require(".actions"):archive_stem(plan.url.name)
			local folder, folder_err = destination(into:join(name), true, false, {})
			if not folder then return notify(failure("extract", tostring(folder_err))) end
			local ok, create_err = fs.create("dir", folder)
			if not ok then return notify(failure("extract", tostring(create_err))) end
			into = folder
		end
		local root = Url("zippity://archive/" .. plan.record.source.archive)
		for _, entry in ipairs(plan.top) do
			local name = entry.path:match("[^/]+$")
			targets[#targets + 1] = { source = root:join(name), target = into:join(name) }
		end
	end
	return self:unpack(targets, false)
end

-- Copies archive members to local targets in as few helper requests as possible.
function M:unpack(targets, force)
	local preserve, taken, groups, order, folders = require(".actions"):preserve_metadata(), {}, {}, {}, {}
	-- Members of one archive that share a password go to the helper in one request.
	-- Folder metadata goes in separate requests that run after every copy, since a
	-- read-only folder would block later writes into it.
	-- Lists start empty as nil: an empty table would encode as a JSON object.
	-- `root` is the folder that link targets must stay inside: where the top-level item goes.
	local function add(item, secret, list, target, root)
		local key = list .. "\0" .. item.record.source.archive .. "\0" .. (secret or "") .. "\0" .. root
		-- A member selected twice, alone and inside a selected folder, needs a second pass.
		while list == "files" and groups[key] and groups[key].members[target.member] do key = key .. "\0" end
		if not groups[key] then
			local protected, err = require(".actions"):protected(api, item.archive)
			if not protected then return nil, fs_error(err) end
			groups[key] = { size = 0, sizes = {}, members = {}, request = { op = "copy_many", archive = item.record.source.archive,
				revision = item.record.source.revision, password = secret, overwrite = force,
				preserve_metadata = preserve, keep_damaged = require(".actions"):keep_damaged(), root = root, cache = item.record.cache,
				protected = #protected > 0 and protected or nil } }
			local queue = list == "files" and order or folders
			queue[#queue + 1] = groups[key]
		end
		local group = groups[key]
		group.request[list] = group.request[list] or {}
		group.request[list][#group.request[list] + 1] = target
		if list == "files" then group.members[target.member] = true end
		return group
	end
	-- Each selected item resolves its archive once and fetches what lies inside it in one
	-- round trip to the main thread, instead of several for every member.
	local function plan(source, target, root)
		local item, err = locate(source)
		if not item then return false, fs_error(err) end
		local children = item.entry.directory and subtree(item.archive, item.member) or {}
		local passwords = secrets(item.archive)
		local function copy(entry, path)
			taken[path] = true
			local member = entry.origin or entry.path
			local batch, group_err = add(item, passwords[member] or passwords[""], "files", { member = member, destination = path }, root)
			if not batch then return false, group_err end
			batch.size = batch.size + entry.size
			batch.sizes[member] = entry.size
			batch.unknown = batch.unknown or entry.unknown_size
			return true
		end
		-- `created` marks a target inside a folder this plan made, where nothing can exist yet.
		local function visit(entry, from, to, created)
			local output, existing = to, nil
			if not created or taken[tostring(to)] then
				local path_err
				output, path_err, existing = destination(to, entry.directory, force, taken)
				if not output then return false, path_err end
			end
			if not entry.directory then
				if not entry.input then return copy(entry, tostring(output.path)) end
				taken[tostring(output)] = true
				local size, copy_err = self:extract { from = from, to = output.path, overwrite = force, preserve_metadata = preserve }
				return size ~= nil, copy_err
			end
			taken[tostring(output)] = true
			if existing and (not existing.is_dir or existing.is_link) then
				return false, fs_error(failure("exists", "Destination is not a regular directory"))
			end
			if not existing then
				local ok, create_err = fs.create("dir", output)
				if not ok then return false, create_err end
			end
			local folder = tostring(output.path)
			for _, child in ipairs(children[entry.path] or {}) do
				local name = child.path:match("[^/]+$")
				local ok, child_err
				-- Files in a new folder need neither a check nor a Url, which costs the most per member.
				if not existing and not child.directory and not child.input and not taken[folder .. "/" .. name] then
					ok, child_err = copy(child, folder .. "/" .. name)
				else
					ok, child_err = visit(child, from:join(name), output:join(name), not existing)
				end
				if not ok then return false, child_err end
			end
			-- Folders with Finder metadata get it on macOS whether or not preservation is on.
			if not entry.input and entry.path ~= "" and (entry.finder or (preserve and (entry.mode or entry.modified))) then
				local batch, group_err = add(item, passwords[""], "directories", { member = entry.origin or entry.path, destination = folder }, root)
				if not batch then return false, group_err end
			end
			return true
		end
		return visit(item.entry, source, target, false)
	end
	local function stop(err)
		ya.emit("refresh", {})
		notify(failure("extract", tostring(err) .. ". Extraction stopped; earlier copies remain"))
	end
	ya.notify { title = "Zippity", content = "Extracting selected members. See the task manager for the running operation.", level = "info", timeout = 4 }
	for _, target in ipairs(targets) do
		local ok, err = plan(target.source, target.target, tostring(target.target.parent.path))
		if not ok then return stop(err) end
	end
	-- The helper copies what it can verify and reports damaged members, members lost
	-- after damage in their solid stream, and partial copies.
	local damaged, lost, partial, outside = {}, {}, 0, 0
	for _, queue in ipairs { order, folders } do
		for _, batch in ipairs(queue) do
			local report, err = copied(batch.request)
			if not report then return stop(fs_error(err)) end
			local expected = batch.size
			for _, member in ipairs(report.damaged or {}) do
				damaged[#damaged + 1] = member
				expected = expected - (batch.sizes[member] or 0)
			end
			for _, member in ipairs(report.lost or {}) do
				lost[#lost + 1] = member
				expected = expected - (batch.sizes[member] or 0)
			end
			partial = partial + #(report.partial or {})
			outside = outside + #(report.skipped or {})
			if report.done ~= expected and not batch.unknown then return stop(fs_error(failure("io", "Extraction did not complete"))) end
		end
	end
	ya.emit("refresh", {})
	local links = outside > 0 and string.format(". Skipped %d %s pointing outside the destination", outside, outside == 1 and "link" or "links") or ""
	if #damaged + #lost > 0 then
		local skipped, counts = {}, {}
		for _, list in ipairs { damaged, lost } do
			for _, member in ipairs(list) do skipped[#skipped + 1] = member end
		end
		if #damaged > 0 then counts[#counts + 1] = #damaged .. " damaged" end
		if #lost > 0 then counts[#counts + 1] = #lost .. " unrecoverable" end
		local names = table.concat(skipped, ", ", 1, math.min(#skipped, 5)) .. (#skipped > 5 and ", ..." or "")
		local saved = partial > 0 and string.format(". Saved %d unverified partial %s as .damaged", partial, partial == 1 and "copy" or "copies") or ""
		return ya.notify { title = "Zippity", level = "warn", timeout = 10, content = string.format(
			"Skipped %s %s: %s%s%s", table.concat(counts, " and "), #skipped == 1 and "member" or "members", names, saved, links) }
	end
	if outside > 0 then
		return ya.notify { title = "Zippity", level = "warn", timeout = 8, content = "Extracted the other members" .. links }
	end
	ya.notify { title = "Zippity", content = "Selected members extracted.", level = "info", timeout = 3 }
end

function M:provide(job)
	if job.op == "Absolute" or job.op == "Canonicalize" or job.op == "Casefold" then
		return job.url
	end
	local method = self[job.op]
	if method then
		return method(self, job)
	end
	return false, Error.fs { kind = "PermissionDenied", message = "Archive members are read-only" }
end

function M:fetch(job)
	return ya.co(function()
		local cached, updates = {}, {}
		for _, current in ipairs(job.files) do
			local archive, member = address(current.url)
			local entry = lookup(archive, member)
			if entry and entry.backing and stat_at(Url(entry.backing)) then
				cached[#cached + 1] = current
			else
				local mime = stat_of(current).is_dir and "folder/archive" or "zippity/member"
				if coroutine.yield(current, { mime }) then updates[current.url] = mime end
			end
		end
		if next(updates) then ya.emit("update_mimes", { updates = updates }) end
		if #cached > 0 then
			local generator = require("mime.local"):fetch(ya.dict_merge(job, { files = cached }))
			local current, result = generator()
			while current do
				current, result = generator(coroutine.yield(current, result))
			end
		end
	end)
end

function M:peek(job)
	local text = "Press Alt-p or l to preview this member.\nPress L to toggle automatic previews for this archive.\nCopy with y, then paste outside the archive."
	local archive = address(job.file.url)
	if automatic_preview(archive) then
		ya.preview_widget(job, ui.Text("Preparing preview..."):area(job.area):wrap(ui.Wrap.YES))
		local path, err = self:materialize(job.file.url, true)
		if path then return ya.emit("refresh", {}) end
		text = err.message .. "\nPress L to disable automatic previews."
	end
	ya.preview_widget(job, ui.Text(text):area(job.area):wrap(ui.Wrap.YES))
end

function M:seek() end

api.context, api.selection, api.clipboard = context, selection, clipboard
api.rpc, api.ensure, api.info, api.locate = rpc, ensure, info, locate
api.password, api.prompt, api.notify = password, prompt, notify
api.address, api.is_archive, api.announce = address, is_archive, announce
api.invalidate, api.search_entries = invalidate, search_entries
api.track_cache, api.forget, api.cache_dirs = track_cache, forget, cache_dirs
api.materialize = function(url) return M:materialize(url) end
api.seconds, api.stat_at = seconds, stat_at

return M
