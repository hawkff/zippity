#!/usr/bin/env python3
"""Exercise the supplied configuration in a separate tmux server."""

import io
import json
import os
from pathlib import Path
import plistlib
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from zipfile import ZIP_DEFLATED, ZipFile


REPO = Path(__file__).resolve().parents[1]
TAGS = "com.apple.metadata:_kMDItemUserTags"
# A binary property list holding the one tag "Red".
TAG_VALUE = plistlib.dumps(["Red\n6"], fmt=plistlib.FMT_BINARY).hex()


def run():
    with tempfile.TemporaryDirectory(prefix="zippity-smoke-") as temporary:
        root = Path(temporary).resolve()
        config, data, out = (root / name for name in ("config", "data", "out"))
        for path in (config, data, out, root / "tmp", root / "state", root / "cache"):
            path.mkdir()
        shutil.copytree(REPO / "zippity.yazi", config / "plugins" / "zippity.yazi")
        for name in ("vfs.toml", "keymap.toml", "yazi.toml"):
            shutil.copyfile(REPO / "examples" / name, config / name)

        archive, encrypted, damaged = data / "example.zip", data / "encrypted.rar", data / "damaged.zip"
        editable, additional, locked = data / "editable.zip", data / "additional.txt", data / "locked.zip"
        additional.write_text("Added through Yazi.\n")
        inner = io.BytesIO()
        with ZipFile(inner, "w") as fixture:
            fixture.writestr("inner.txt", "Nested archive preview.\n")
        with ZipFile(editable, "w") as fixture:
            fixture.writestr("one.txt", "Editable archive member.\n")
            fixture.writestr("two.txt", "Keep this member.\n")
        with ZipFile(archive, "w") as fixture:
            fixture.writestr("folder/nested.txt", "Nested directory content.\n")
            fixture.writestr("notes.txt", "Preview works inside Yazi.\n")
            fixture.writestr("z-inner.zip", inner.getvalue())
            fixture.writestr("z-large.bin", b"Z" * (40 * 1024 * 1024), compress_type=ZIP_DEFLATED)
        shutil.copyfile(
            REPO / "tests/fixtures/test_read_format_rar5_encrypted_filenames.rar", encrypted
        )
        with ZipFile(damaged, "w") as fixture:
            fixture.writestr("pack/a-good.txt", "First good member.\n")
            fixture.writestr("pack/b-bad.txt", "Damaged member content.\n")
            fixture.writestr("pack/c-good.txt", "Last good member.\n")
        broken = bytearray(damaged.read_bytes())
        broken[broken.index(b"Damaged member content.")] ^= 1
        damaged.write_bytes(bytes(broken))
        (root / "secret.txt").write_text("Encrypted member content.\n")
        subprocess.run(
            ["7zz", "a", "-tzip", "-plocked-password", "-bso0", "-bsp0", str(locked), "secret.txt"], cwd=root, check=True
        )
        # An archive made by zippity, which keeps Finder tags on macOS under a hidden __MACOSX.
        tagged, sources = data / "tagged.zip", root / "tag-sources"
        (sources / "tagged-dir").mkdir(parents=True)
        (sources / "tagged-dir/t.txt").write_text("Tagged member.\n")
        if sys.platform == "darwin":
            for path in (sources / "tagged-dir", sources / "tagged-dir/t.txt"):
                subprocess.run(["xattr", "-wx", TAGS, TAG_VALUE, str(path)], check=True)
        request = {"op": "create", "sources": [str(sources / "tagged-dir")], "destination": str(tagged),
            "format": "zip", "options": {"level": 5}}
        subprocess.run([REPO / "target/debug/zippity"], input=json.dumps(request).encode(), capture_output=True, check=True)
        originals = {path: path.read_bytes() for path in (archive, encrypted)}
        keymap = (config / "keymap.toml").read_text()
        shortcuts = "\n".join(
            f"  {{ on = {json.dumps(key)}, run = {json.dumps(command)} }},"
            for key, command in (
                ("<F6>", f"cd {shlex.quote(str(out))}"),
                ("<F7>", f"reveal {shlex.quote(str(encrypted))}"),
                ("<F8>", f"reveal {shlex.quote(str(archive))}"),
                ("<F9>", "paste"),
                ("<F10>", f"reveal {shlex.quote(str(editable))}"),
                ("<F11>", "plugin zippity -- rename"),
                ("<F12>", "plugin zippity -- delete"),
                ("<F5>", f"reveal {shlex.quote(str(additional))}"),
                ("<F4>", "plugin zippity -- metadata"),
                ("<F3>", f"reveal {shlex.quote(str(damaged))}"),
                ("<F2>", f"reveal {shlex.quote(str(locked))}"),
                ("<F1>", f"reveal {shlex.quote(str(tagged))}"),
            )
        )
        assert keymap.count("\n]\n") == 1
        (config / "keymap.toml").write_text(keymap.replace("\n]\n", f"\n{shortcuts}\n]\n"))

        env = os.environ.copy()
        env.update(
            YAZI_CONFIG_HOME=str(config),
            XDG_CACHE_HOME=str(root / "cache"),
            XDG_STATE_HOME=str(root / "state"),
            TMPDIR=str(root / "tmp"),
            PATH=str(REPO / "target/debug") + os.pathsep + env["PATH"],
        )
        for tool in ("yazi", "zippity", "7zz", "file", "tmux"):
            assert shutil.which(tool, path=env["PATH"]), f"{tool} is required"
        socket = f"zippity-smoke-{os.getpid()}"

        def tmux(*args, check=True):
            return subprocess.run(
                ["tmux", "-L", socket, *args], env=env, capture_output=True,
                text=True, check=check, timeout=10,
            )

        def screen():
            return tmux("capture-pane", "-pt", "smoke", check=False).stdout

        def wait(predicate, description, timeout=15):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if predicate():
                    return
                time.sleep(0.1)
            raise AssertionError(description + "\n" + screen().replace(str(root), "<temporary>"))

        def keys(*values):
            tmux("send-keys", "-t", "smoke", *values)

        def caches():
            return list((root / "tmp").glob("zippity-*"))

        def idle(description, timeout=30):
            # Background tasks come and go; one look without "left" is enough.
            deadline = time.monotonic() + timeout
            while "left" in screen().splitlines()[-1]:
                if time.monotonic() > deadline:
                    keys("w")
                    time.sleep(0.3)
                    raise AssertionError(description + "\n" + screen().replace(str(root), "<temporary>"))
                time.sleep(0.1)

        try:
            command = shlex.join([shutil.which("yazi", path=env["PATH"]), str(archive)])
            tmux("new-session", "-d", "-s", "smoke", "-x", "140", "-y", "32", command)
            wait(lambda: "example.zip" in screen(), "Yazi did not start")
            keys("l")
            wait(lambda: "zippity://" in screen() and "notes.txt" in screen(), "Archive did not open")
            assert not caches(), "Browsing materialized archive content"
            keys("l")
            wait(lambda: screen().splitlines()[0].split()[0].endswith("/folder"), "Directory navigation failed")
            keys("h")
            wait(lambda: not screen().splitlines()[0].split()[0].endswith("/folder"), "Leaving the directory failed")
            keys("j", "M-p")
            wait(lambda: "Preview works inside Yazi." in screen(), "Member preview failed")
            assert len(caches()) == 1
            assert stat.S_IMODE(caches()[0].stat().st_mode) == 0o700

            keys("y", "F6")
            wait(lambda: screen().splitlines()[0].split()[0].endswith("/out"), "Destination navigation failed")
            keys("p")
            wait(lambda: (out / "notes.txt").exists(), "Selected copy failed")
            assert (out / "notes.txt").read_text() == "Preview works inside Yazi.\n"
            assert len(caches()) == 1, "Copying created another preview cache"

            keys("F8")
            wait(lambda: "example.zip" in screen(), "Archive was not selected")
            keys("l")
            wait(lambda: "zippity://" in screen(), "Archive did not reopen")
            keys("g", "g")
            wait(lambda: "folder" in screen().splitlines()[-1], "Folder was not selected")
            keys("y", "F6")
            wait(lambda: screen().splitlines()[0].split()[0].endswith("/out"), "Destination navigation failed")
            keys("p")
            wait(lambda: (out / "folder/nested.txt").exists(), "Folder copy failed")
            assert (out / "folder/nested.txt").read_text() == "Nested directory content.\n"

            keys("F3")
            wait(lambda: "damaged.zip" in screen().splitlines()[-1], "Damaged archive was not selected")
            keys("l")
            wait(lambda: "zippity://" in screen() and "pack" in screen(), "Damaged archive did not open")
            keys("y", "F6")
            wait(lambda: screen().splitlines()[0].split()[0].endswith("/out"), "Destination navigation failed")
            keys("p")
            wait(lambda: "Skipped 1 damaged member" in screen(), "Damaged member was not reported")
            assert (out / "pack/a-good.txt").read_text() == "First good member.\n"
            assert (out / "pack/c-good.txt").read_text() == "Last good member.\n"
            assert not (out / "pack/b-bad.txt").exists()

            keys("F8")
            wait(lambda: "example.zip" in screen(), "Archive was not selected")
            keys("l")
            wait(lambda: "zippity://" in screen(), "Archive did not reopen")
            keys("G", "M-p")
            wait(lambda: "Preview large archive member?" in screen(), "Large preview lacks confirmation")
            keys("n")
            wait(lambda: "Preview large archive member?" not in screen(), "Preview cancellation failed")
            assert len(caches()) == 1
            keys("y", "F6")
            wait(lambda: screen().splitlines()[0].split()[0].endswith("/out"), "Destination navigation failed")
            keys("p")
            wait(lambda: (out / "z-large.bin").exists(), "Large selected copy failed")
            assert (out / "z-large.bin").stat().st_size == 40 * 1024 * 1024
            with (out / "z-large.bin").open("rb") as copied:
                assert copied.read(64) == b"Z" * 64
                copied.seek(-64, 2)
                assert copied.read(64) == b"Z" * 64
            assert len(caches()) == 1, "Large copy created a preview cache"
            idle("Large copy task failed")

            keys("F9")
            native = out / "z-large_1.bin"
            def native_complete():
                if not native.exists():
                    return False
                with native.open("rb") as copied:
                    if copied.read(64) != b"Z" * 64:
                        return False
                    copied.seek(-64, 2)
                    return copied.read(64) == b"Z" * 64
            wait(native_complete, "Native large copy did not finish", timeout=60)
            with native.open("rb") as copied:
                assert copied.read(64) == b"Z" * 64
                copied.seek(-64, 2)
                assert copied.read(64) == b"Z" * 64

            # Alt-x unpacks a whole archive beside it: several top-level members get a
            # folder named after the archive, and a second run picks a free name.
            keys("F8")
            wait(lambda: "example.zip" in screen().splitlines()[-1], "Archive was not selected")
            keys("M-x")
            wait(lambda: (data / "example/notes.txt").exists(), "Whole-archive extraction failed", timeout=60)
            idle("Whole-archive extraction did not finish")
            assert (data / "example/folder/nested.txt").read_text() == "Nested directory content.\n"
            assert (data / "example/z-large.bin").stat().st_size == 40 * 1024 * 1024
            keys("M-x")
            wait(lambda: (data / "example_1/notes.txt").exists(), "A second extraction did not pick a free name", timeout=60)
            idle("The second whole-archive extraction did not finish")
            assert (data / "example/notes.txt").read_text() == "Preview works inside Yazi.\n"
            # A single top-level folder lands as it is, not inside another folder.
            keys("F3")
            wait(lambda: "damaged.zip" in screen().splitlines()[-1], "Damaged archive was not selected")
            # Yazi drops a notification equal to one still queued, and the earlier paste of
            # this archive showed the same warning; wait until every notification box closes.
            wait(lambda: "╭" not in screen(), "Earlier notifications did not clear", timeout=40)
            keys("M-x")
            wait(lambda: (data / "pack/c-good.txt").exists(), "A single top-level folder did not land as it is", timeout=60)
            wait(lambda: "Skipped 1 damaged member" in screen(), "Whole-archive extraction did not report damage")
            idle("Extracting the damaged archive did not finish")
            assert (data / "pack/a-good.txt").read_text() == "First good member.\n"
            assert not (data / "pack/b-bad.txt").exists() and not (data / "damaged").exists()
            # A selection made in another folder still extracts beside its archive.
            keys("F3")
            wait(lambda: "damaged.zip" in screen().splitlines()[-1], "Damaged archive was not hovered")
            keys("Space", "F6")
            wait(lambda: screen().splitlines()[0].split()[0].endswith("/out"), "Did not leave the archive's folder")
            keys("M-x")
            wait(lambda: (data / "pack_1/c-good.txt").exists(), "Extraction did not land beside the archive", timeout=60)
            idle("Extracting from another folder did not finish")
            # The earlier paste left out/pack, so an extraction into out would make out/pack_1.
            assert (data / "pack_1/a-good.txt").read_text() == "First good member.\n" and not (out / "pack_1").exists()
            # Unselect the archive; a bare Escape would merge with the next key's sequence.
            keys("F3")
            wait(lambda: "damaged.zip" in screen().splitlines()[-1], "Damaged archive was not hovered again")
            keys("Space")
            # The hidden __MACOSX folder does not count, so the one folder lands as it is, and
            # on macOS the folder and its file get their Finder tags.
            keys("F1")
            wait(lambda: "tagged.zip" in screen().splitlines()[-1], "Tagged archive was not hovered")
            keys("M-x")
            wait(lambda: (data / "tagged-dir/t.txt").exists(), "Tagged archive was not extracted", timeout=60)
            idle("Extracting the tagged archive did not finish")
            assert not (data / "tagged").exists() and not (data / "__MACOSX").exists()
            if sys.platform == "darwin":
                for path in (data / "tagged-dir", data / "tagged-dir/t.txt"):
                    found = subprocess.run(["xattr", "-px", TAGS, str(path)], capture_output=True, text=True)
                    assert "".join(found.stdout.split()).lower() == TAG_VALUE, (path, found.stdout, found.stderr)

            keys("F8")
            wait(lambda: "example.zip" in screen(), "Archive was not selected")
            keys("l")
            wait(lambda: "zippity://" in screen(), "Archive did not reopen")
            keys("M-f")
            wait(lambda: "Search archive filenames" in screen(), "Archive search prompt is missing")
            tmux("send-keys", "-t", "smoke", "-l", "z-inner.zip")
            keys("Enter")
            wait(lambda: "Next results" in screen(), "Search results are missing")
            keys("1")
            wait(lambda: "z-inner.zip" in screen().splitlines()[-1], "Search did not reveal its result")
            keys("l")
            wait(lambda: "inner.txt" in screen() and "/z-inner.zip" in screen().splitlines()[0], "Nested archive did not open")
            keys("M-p")
            wait(lambda: "Nested archive preview." in screen(), "Nested member preview failed")
            keys("h")
            wait(lambda: not screen().splitlines()[0].split()[0].endswith("/z-inner.zip"), "Nested archive return failed")
            keys("M-t")
            wait(lambda: "Archive integrity verified" in screen(), "Integrity action failed")

            keys("F10")
            wait(lambda: "editable.zip" in screen(), "Editable archive was not selected")
            keys("l")
            wait(lambda: "one.txt" in screen() and "zippity://" in screen(), "Editable archive did not open")
            original_editable = editable.read_bytes()
            keys("M-e")
            wait(lambda: "[EDIT]" in screen(), "Edit mode indicator is missing")
            keys("F11")
            wait(lambda: "New member path" in screen(), "Rename prompt is missing")
            keys("C-u")
            tmux("send-keys", "-t", "smoke", "-l", "newdir/renamed.txt")
            keys("Enter")
            wait(lambda: "[UNSAVED 1]" in screen() and "newdir" in screen(), "Rename was not staged")
            assert editable.read_bytes() == original_editable
            keys("g", "g")
            wait(lambda: "newdir" in screen().splitlines()[-1], "Draft directory was not selected")
            keys("F4")
            wait(lambda: "preserves safe permissions" in screen(), "Metadata preservation did not turn on")
            keys("y", "F6")
            wait(lambda: screen().splitlines()[0].split()[0].endswith("/out"), "Draft extraction destination was not selected")
            keys("p")
            wait(lambda: "Selected members extracted." in screen(), "Draft-only directory metadata failed")
            assert (out / "newdir/renamed.txt").read_text() == "Editable archive member.\n"
            keys("F10")
            wait(lambda: "editable.zip" in screen().splitlines()[-1], "Editable archive was not selected")
            keys("l")
            wait(lambda: "zippity://" in screen(), "Edit session did not reopen")
            keys("M-s")
            wait(lambda: "Archive saved." in screen(), "Save did not complete")
            with ZipFile(editable) as saved:
                assert set(saved.namelist()) == {"newdir/renamed.txt", "two.txt"}
            original_editable = editable.read_bytes()
            keys("M-e")
            wait(lambda: "[EDIT]" in screen(), "Second edit session did not start")
            keys("F12")
            wait(lambda: "Stage archive deletion?" in screen(), "Delete confirmation is missing")
            keys("y")
            wait(lambda: "[UNSAVED 1]" in screen(), "Deletion was not staged")
            keys("q")
            wait(lambda: "Cancel closing" in screen(), "Quit ignored pending changes")
            keys("c")
            keys("M-d")
            wait(lambda: "Discard archive changes?" in screen(), "Discard confirmation is missing")
            keys("y")
            wait(lambda: "[UNSAVED" not in screen() and "newdir" in screen(), "Discard did not restore the view")
            assert editable.read_bytes() == original_editable
            keys("F5")
            wait(lambda: "additional.txt" in screen().splitlines()[-1], "Input file was not selected")
            keys("y", "F10")
            wait(lambda: "editable.zip" in screen().splitlines()[-1], "Editable archive was not selected")
            keys("l")
            wait(lambda: "zippity://" in screen(), "Editable archive did not reopen")
            keys("M-e")
            wait(lambda: "[EDIT]" in screen(), "Add edit session did not start")
            keys("p")
            wait(lambda: "[UNSAVED" in screen() and "additional.txt" in screen(), "Yanked input was not staged")
            keys("M-s")
            wait(lambda: "[UNSAVED" not in screen(), "Added input was not saved")
            with ZipFile(editable) as saved:
                assert saved.read("additional.txt") == b"Added through Yazi.\n"

            keys("F5")
            wait(lambda: "additional.txt" in screen().splitlines()[-1], "Creation input was not selected")
            keys("M-n")
            wait(lambda: "requires separately" in screen(), "Archive format picker is missing")
            keys("z")
            wait(lambda: "New archive path" in screen(), "Archive destination prompt is missing")
            keys("C-u")
            created = out / "created.zip"
            tmux("send-keys", "-t", "smoke", "-l", str(created))
            keys("Enter")
            wait(lambda: "Compression level" in screen(), "Compression prompt is missing")
            keys("Enter")
            wait(lambda: "No encryption" in screen(), "Encryption picker is missing")
            keys("n")
            wait(lambda: "Split volume size" in screen(), "Volume prompt is missing")
            keys("Enter")
            wait(lambda: created.exists(), "Archive creation failed")
            with ZipFile(created) as saved:
                assert saved.read("additional.txt") == b"Added through Yazi.\n"

            # Save As writes every member into a new archive beside the source, under a
            # free name when the format stays the same.
            keys("F10")
            wait(lambda: "editable.zip" in screen().splitlines()[-1], "Editable archive was not hovered")
            keys("M-a")
            wait(lambda: "Save as a new archive" in screen(), "Archive actions menu is missing")
            keys("c")
            wait(lambda: "requires separately" in screen(), "Save As format picker is missing")
            keys("z")
            wait(lambda: "New archive path" in screen(), "Save As destination prompt is missing")
            keys("Enter")
            wait(lambda: "Compression level" in screen(), "Compression prompt is missing")
            keys("Enter")
            wait(lambda: "No encryption" in screen(), "Encryption picker is missing")
            keys("n")
            wait(lambda: "Split volume size" in screen(), "Volume prompt is missing")
            keys("Enter")
            copy = data / "editable-copy.zip"
            wait(lambda: copy.exists(), "Save As failed")
            with ZipFile(copy) as saved, ZipFile(editable) as original:
                files = lambda archive: {n: archive.read(n) for n in archive.namelist() if not n.endswith("/")}
                assert files(saved) == files(original)

            # Alt-z archives the selection beside it with the last settings, under a free name.
            for name in ("additional.txt.zip", "additional.txt_1.zip"):
                keys("F5")
                # The status line pads the hovered name, which tells it from additional.txt.zip.
                wait(lambda: " additional.txt " in screen().splitlines()[-1], "Quick compress input was not hovered")
                keys("M-z")
                wait(lambda: (data / name).exists(), f"Quick compress did not write {name}", timeout=30)
                with ZipFile(data / name) as quick:
                    assert quick.read("additional.txt") == b"Added through Yazi.\n"
            # The dialog's format and compression carry over to the next quick compress.
            keys("F5")
            wait(lambda: " additional.txt " in screen().splitlines()[-1], "Creation input was not hovered")
            keys("M-n")
            wait(lambda: "requires separately" in screen(), "Archive format picker is missing")
            keys("7")
            wait(lambda: "New archive path" in screen(), "Archive destination prompt is missing")
            keys("Enter")
            wait(lambda: "Compression level" in screen(), "Compression prompt is missing")
            keys("C-u")
            tmux("send-keys", "-t", "smoke", "-l", "9 method=PPMd")
            keys("Enter")
            wait(lambda: "No encryption" in screen(), "Encryption picker is missing")
            keys("n")
            wait(lambda: "Split volume size" in screen(), "Volume prompt is missing")
            keys("Enter")
            wait(lambda: (data / "archive.7z").exists(), "7z creation failed")
            keys("F5")
            wait(lambda: " additional.txt " in screen().splitlines()[-1], "Quick compress input was not hovered")
            keys("M-z")
            wait(lambda: (data / "additional.txt.7z").exists(), "Quick compress did not reuse the format", timeout=30)
            listing = subprocess.run(["7zz", "l", "-slt", str(data / "additional.txt.7z")], capture_output=True, text=True, check=True).stdout
            assert "Method = PPMD" in listing, listing

            baseline_caches = len(caches())
            keys("F7")
            wait(lambda: "encrypted.rar" in screen(), "Encrypted archive was not selected")
            keys("l")
            wait(lambda: "Archive password" in screen(), "Password prompt is missing")
            tmux("send-keys", "-t", "smoke", "-l", "password")
            keys("Enter")
            wait(lambda: "a.txt" in screen() and "Archive password" not in screen(), "Encrypted archive did not open")
            keys("M-p")
            wait(lambda: "This is from a.txt" in screen(), "Encrypted preview failed")
            assert len(caches()) == baseline_caches + 1
            keys("M-l")
            wait(lambda: "zippity://" not in screen().splitlines()[0], "Lock did not leave the archive")
            assert len(caches()) == baseline_caches, "Lock did not remove the member cache"
            keys("l")
            wait(lambda: "Archive password" in screen(), "Lock did not forget the password")
            keys("C-c")
            wait(lambda: "Archive password" not in screen(), "Password cancellation failed")
            # Every selected archive opens before anything is written, so a file that is not
            # an archive or a cancelled password prompt leaves no output from the others.
            def hover(key, name):
                keys(key)
                wait(lambda: name in screen().splitlines()[-1], f"{name} was not hovered")

            hover("F8", "example.zip")
            keys("Space")
            hover("F5", "additional.txt")
            keys("Space", "M-x")
            wait(lambda: "Not an archive zippity can open" in screen(), "A file that is not an archive was not refused")
            hover("F5", "additional.txt")
            keys("Space")
            hover("F2", "locked.zip")
            keys("Space", "M-x")
            wait(lambda: "Archive password" in screen(), "The locked archive did not ask for the password")
            keys("C-c")
            wait(lambda: "Archive password" not in screen(), "Password cancellation failed")
            assert not (data / "example_2").exists() and not (data / "secret.txt").exists()
            for key, name in (("F8", "example.zip"), ("F2", "locked.zip")):
                hover(key, name)
                keys("Space")

            # Members encrypted under plain headers ask for the password before extraction.
            keys("F2")
            wait(lambda: "locked.zip" in screen().splitlines()[-1], "Locked archive was not hovered")
            keys("M-x")
            wait(lambda: "Archive password" in screen(), "Whole-archive extraction did not ask for the password")
            tmux("send-keys", "-t", "smoke", "-l", "locked-password")
            keys("Enter")
            wait(lambda: (data / "secret.txt").exists(), "Encrypted members were not extracted", timeout=60)
            idle("Extracting encrypted members did not finish")
            assert (data / "secret.txt").read_text() == "Encrypted member content.\n"

            # Quit asks for confirmation while tasks run, so let the native copy finish first.
            idle("Tasks did not finish before quit")
            keys("q")
            wait(lambda: tmux("has-session", "-t", "smoke", check=False).returncode != 0, "Quit failed")
            assert not caches(), "Quit left a member cache behind"
            for path, content in originals.items():
                assert path.read_bytes() == content, "Source archive changed"
            print("Yazi navigation, preview, copy, passwords, lock, and cleanup passed")
        finally:
            tmux("kill-server", check=False)
            time.sleep(0.3)


if __name__ == "__main__":
    run()
