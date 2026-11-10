use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

use serde_json::{Value, json};
use tempfile::TempDir;

fn invoke(request: Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zippity"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.take().unwrap(), &request).unwrap();
    child.wait_with_output().unwrap()
}

fn index(archive: &Path) -> Value {
    let output = invoke(json!({"op": "list", "archive": archive}));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Copies `member` out, and returns the helper's result with what the copy holds.
fn extract(
    archive: &Path,
    member: &str,
    listing: &Value,
    password: Option<&str>,
) -> (Output, Vec<u8>) {
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("member");
    let output = invoke(json!({"op":"copy", "archive":archive, "member":member,
        "revision":listing["source"]["revision"], "password":password,
        "destination":destination}));
    (output, fs::read(destination).unwrap_or_default())
}

fn fixture(encrypted: bool, large: bool) -> (TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir_all(source.join("資料")).unwrap();
    for (name, content) in [
        ("notes.txt", "hello archive\n"),
        ("資料/é.txt", "unicode\n"),
        ("a*?.txt", "literal wildcard\n"),
        ("@list.txt", "literal at sign\n"),
        ("-switch.txt", "literal hyphen\n"),
        ("2024-01-01 10:30.log", "literal colon\n"),
    ] {
        fs::write(source.join(name), content).unwrap();
    }
    if large {
        fs::File::create(source.join("large.bin"))
            .unwrap()
            .set_len(48 * 1024 * 1024)
            .unwrap();
    }
    let archive = root.path().join("example.zip");
    let mut command = Command::new("7zz");
    command.args(["a", "-tzip", "-mx=1", "-y"]);
    if encrypted {
        command.args(["-pfixture-password", "-mem=AES256"]);
    }
    let output = command
        .arg(&archive)
        .arg(".")
        .current_dir(source)
        .output()
        .expect("7zz is required for archive integration tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (root, archive)
}

#[test]
fn lists_and_copies_literal_member_names_without_touching_the_archive() {
    let (root, archive) = fixture(false, false);
    let original = fs::read(&archive).unwrap();
    let listing = index(&archive);
    let entries = listing["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["path"] == "資料" && entry["directory"] == true)
    );
    for (member, expected) in [
        ("a*?.txt", "literal wildcard\n"),
        ("@list.txt", "literal at sign\n"),
        ("-switch.txt", "literal hyphen\n"),
        ("資料/é.txt", "unicode\n"),
        ("2024-01-01 10:30.log", "literal colon\n"),
    ] {
        let (output, copied) = extract(&archive, member, &listing, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(copied, expected.as_bytes());
    }
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
}

#[test]
fn password_failure_preserves_the_destination_and_cleans_partial_files() {
    let (root, archive) = fixture(true, false);
    let listing = index(&archive);
    let destination = root.path().join("out.txt");
    fs::write(&destination, "keep this").unwrap();
    let mut request = json!({"op":"copy", "archive":archive, "member":"notes.txt",
        "revision":listing["source"]["revision"], "destination":destination,
        "overwrite":true, "password":"wrong-fixture-password"});
    let failed = invoke(request.clone());
    assert!(!failed.status.success());
    let error: Value = serde_json::from_slice(&failed.stderr).unwrap();
    assert_eq!(error["kind"], "password");
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("wrong-fixture-password"));
    assert_eq!(fs::read_to_string(&destination).unwrap(), "keep this");
    assert!(!fs::read_dir(root.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".zippity-")
    }));
    request["password"] = json!("fixture-password");
    let success = invoke(request);
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    assert_eq!(fs::read_to_string(destination).unwrap(), "hello archive\n");
}

#[test]
fn damaged_members_with_password_like_names_report_damage() {
    let root = tempfile::tempdir().unwrap();
    let member = "report. Wrong password?";
    fs::write(root.path().join(member), "stored member\n").unwrap();
    let archive = root.path().join("encrypted.zip");
    let output = Command::new("7zz")
        .args(["a", "-tzip", "-mx=0", "-spd", "-y"])
        .arg(&archive)
        .arg(member)
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut bytes = fs::read(&archive).unwrap();
    let data = bytes
        .windows(13)
        .position(|window| window == b"stored member")
        .unwrap();
    bytes[data] ^= 1;
    fs::write(&archive, bytes).unwrap();
    let listing = index(&archive);
    let (output, _) = extract(&archive, member, &listing, None);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["kind"], "damaged");
}

#[test]
fn copying_cannot_overwrite_an_archive_or_an_unapproved_destination() {
    let (root, archive) = fixture(false, false);
    let original = fs::read(&archive).unwrap();
    let listing = index(&archive);
    let request = json!({"op":"copy", "archive":archive, "member":"notes.txt",
        "revision":listing["source"]["revision"], "destination":archive, "overwrite":true});
    assert!(!invoke(request).status.success());
    assert_eq!(fs::read(&archive).unwrap(), original);
    let destination = root.path().join("out.txt");
    fs::write(&destination, "keep this").unwrap();
    assert!(
        !invoke(json!({"op":"copy", "archive":archive, "member":"notes.txt",
        "revision":listing["source"]["revision"], "destination":destination}))
        .status
        .success()
    );
    assert_eq!(fs::read_to_string(destination).unwrap(), "keep this");
}

#[test]
fn large_members_copy_with_progress() {
    let (root, archive) = fixture(false, true);
    let listing = index(&archive);
    let destination = root.path().join("large.bin");
    let request = json!({"op":"copy", "archive":archive, "member":"large.bin",
        "revision":listing["source"]["revision"], "destination":destination});
    let output = invoke(request);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let progress: u64 = events.iter().filter_map(|e| e["bytes"].as_u64()).sum();
    assert_eq!(progress, 48 * 1024 * 1024);
    assert_eq!(events.last().unwrap()["done"], progress);
    assert_eq!(fs::metadata(destination).unwrap().len(), progress);
}

#[test]
fn new_files_are_private_and_overwrites_preserve_access_bits() {
    use std::os::unix::fs::PermissionsExt;
    let (root, archive) = fixture(false, false);
    let listing = index(&archive);
    let destination = root.path().join("out.txt");
    let mut request = json!({"op":"copy", "archive":archive, "member":"notes.txt",
        "revision":listing["source"]["revision"], "destination":destination});
    assert!(invoke(request.clone()).status.success());
    assert_eq!(
        fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o4751)).unwrap();
    request["overwrite"] = json!(true);
    assert!(invoke(request).status.success());
    assert_eq!(
        fs::metadata(&destination).unwrap().permissions().mode() & 0o7777,
        0o751
    );
}

#[test]
fn stale_revisions_fail_before_replacing_files() {
    let (root, archive) = fixture(false, false);
    let listing = index(&archive);
    let destination = root.path().join("out.txt");
    fs::OpenOptions::new()
        .append(true)
        .open(&archive)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let output = invoke(json!({"op":"copy", "archive":archive, "member":"notes.txt",
        "revision":listing["source"]["revision"], "destination":destination}));
    assert!(!output.status.success());
    assert!(!destination.exists());
}

#[test]
fn reads_rar4_and_rar5_with_encrypted_headers_and_solid_blocks() {
    for version in [4, 5] {
        for kind in [
            "encrypted",
            "encrypted_filenames",
            "solid_encrypted",
            "solid_encrypted_filenames",
        ] {
            let archive = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("test_read_format_rar{version}_{kind}.rar"));
            if kind.ends_with("filenames") {
                let output = invoke(json!({"op":"list", "archive":archive}));
                assert!(!output.status.success());
                let error: Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(error["kind"], "password");
            }
            let output = invoke(json!({"op":"list", "archive":archive, "password":"password"}));
            assert!(
                output.status.success(),
                "{archive:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let listing: Value = serde_json::from_slice(&output.stdout).unwrap();
            for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
                let password = if kind == "encrypted" && name == "d.txt" {
                    "password2"
                } else {
                    "password"
                };
                let (output, copied) = extract(&archive, name, &listing, Some(password));
                assert!(
                    output.status.success(),
                    "{archive:?}/{name}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(copied.starts_with(format!("This is from {name}").as_bytes()));
            }
        }
    }
}

#[test]
fn reads_multipart_rar_and_rejects_a_missing_middle_volume() {
    let root = tempfile::tempdir().unwrap();
    for part in 1..=4 {
        let name = format!("test_read_format_rar5_multiarchive_solid.part{part:02}.rar");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(&name),
            root.path().join(name),
        )
        .unwrap();
    }
    let archive = root
        .path()
        .join("test_read_format_rar5_multiarchive_solid.part03.rar");
    let listing = index(&archive);
    assert_eq!(listing["source"]["volumes"].as_array().unwrap().len(), 4);
    let entry = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["directory"] == false && e["size"].as_u64().unwrap() > 0)
        .unwrap();
    let destination = root.path().join("out.bin");
    let output = invoke(
        json!({"op":"copy", "archive":archive, "member":entry["path"],
        "revision":listing["source"]["revision"], "destination":destination}),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::metadata(destination).unwrap().len(),
        entry["size"].as_u64().unwrap()
    );
    fs::remove_file(
        root.path()
            .join("test_read_format_rar5_multiarchive_solid.part02.rar"),
    )
    .unwrap();
    assert!(
        !invoke(json!({"op":"list", "archive":archive}))
            .status
            .success()
    );
}
