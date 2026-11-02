use crate::{
    engine,
    error::{Error, Result},
    source::Source,
    write::{Change, validate_input},
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub fn read(change: &Change, offset: u64, length: usize) -> Result<Vec<u8>> {
    if length > 1024 * 1024 {
        return Err(Error::new("limit", "Read requests are limited to 1 MiB"));
    }
    let entry = validate_input(change)?;
    if entry.directory {
        return Err(Error::new("request", "Cannot read a directory as a file"));
    }
    let Change::Put { source, .. } = change else {
        unreachable!()
    };
    let mut file = fs::File::open(source)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut output = Vec::new();
    file.take(length as u64).read_to_end(&mut output)?;
    validate_input(change)?;
    Ok(output)
}

pub fn copy(
    archive: &Path,
    revision: &str,
    change: &Change,
    destination: &Path,
    overwrite: bool,
    preserve_metadata: bool,
) -> Result<u64> {
    let source = Source::open(archive)?;
    if source.revision != revision {
        return Err(Error::new(
            "changed",
            "Archive changed; discard the pending edits",
        ));
    }
    source.check_destination(destination)?;
    let Change::Put { source: input, .. } = change else {
        return Err(Error::new("request", "Expected an added file"));
    };
    validate_input(change)?;
    let before = engine::destination_stamp(destination)?;
    if before.is_some() && !overwrite {
        return Err(Error::new("exists", "Destination exists"));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".zippity-")
        .tempfile_in(parent)?;
    let mut reader = fs::File::open(input)?;
    let mut buffer = vec![0; 1024 * 1024];
    let mut size = 0;
    loop {
        if crate::cancelled() {
            return Err(Error::new("cancelled", "Copy cancelled"));
        }
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        temporary.write_all(&buffer[..count])?;
        size += count as u64;
        writeln!(std::io::stdout().lock(), "{{\"bytes\":{count}}}")?;
    }
    let entry = validate_input(change)?;
    engine::finish(
        temporary.as_file(),
        before.as_ref(),
        &entry,
        preserve_metadata,
    )?;
    source.unchanged()?;
    source.check_destination(destination)?;
    if engine::destination_stamp(destination)? != before {
        return Err(Error::new("changed", "Destination changed during copy"));
    }
    engine::publish(temporary.into_temp_path(), destination, before.is_some())?;
    Ok(size)
}
