use std::{
    fs,
    io::{self, Read as _},
    path::PathBuf,
};

use nanocodex_vm::tools::GuestRuntimeDisk;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

#[derive(Serialize)]
struct Output<'a> {
    version: u32,
    root_image: String,
    image_manifest: String,
    image_manifest_commitment_sha256: &'a str,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    fs::create_dir_all(&options.output)?;
    let runtime = GuestRuntimeDisk::prepare(&options.guest, &options.cache)?;
    validate_runtime(&runtime)?;
    let root_image = options.output.join("guest-root.ext4.verity");
    let manifest_path = options.output.join("guest-image-manifest.json");
    copy_exact(runtime.path(), &root_image)?;
    write_exact(
        &manifest_path,
        &serde_json::to_vec_pretty(runtime.manifest())?,
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            version: 1,
            root_image: root_image.display().to_string(),
            image_manifest: manifest_path.display().to_string(),
            image_manifest_commitment_sha256: runtime.manifest_digest(),
        })?
    );
    Ok(())
}

fn copy_exact(source: &std::path::Path, destination: &std::path::Path) -> Result<(), io::Error> {
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
    {
        Ok(mut output) => {
            let mut input = fs::File::open(source)?;
            io::copy(&mut input, &mut output)?;
            output.sync_all()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if files_equal(source, destination)? {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "reproducible guest output already exists with different bytes: {}",
                        destination.display()
                    ),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

fn write_exact(destination: &std::path::Path, bytes: &[u8]) -> Result<(), io::Error> {
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
    {
        Ok(mut output) => {
            use std::io::Write as _;

            output.write_all(bytes)?;
            output.sync_all()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::read(destination)? == bytes {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "reproducible guest output already exists with different bytes: {}",
                        destination.display()
                    ),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

fn files_equal(first: &std::path::Path, second: &std::path::Path) -> Result<bool, io::Error> {
    let mut first = fs::File::open(first)?;
    let mut second = fs::File::open(second)?;
    let mut first_buffer = [0_u8; 64 * 1024];
    let mut second_buffer = [0_u8; 64 * 1024];
    loop {
        let first_read = first.read(&mut first_buffer)?;
        let second_read = second.read(&mut second_buffer)?;
        if first_read != second_read || first_buffer[..first_read] != second_buffer[..second_read] {
            return Ok(false);
        }
        if first_read == 0 {
            return Ok(true);
        }
    }
}

fn validate_runtime(runtime: &GuestRuntimeDisk) -> Result<(), io::Error> {
    let metadata = fs::symlink_metadata(runtime.path())?;
    let root_image = runtime.manifest().root_image();
    if !metadata.file_type().is_file()
        || metadata.len() != root_image.bytes()
        || sha256_file(runtime.path())? != root_image.sha256()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepared guest image does not match its artifact manifest",
        ));
    }
    Ok(())
}

fn sha256_file(path: &std::path::Path) -> Result<String, io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

struct Options {
    guest: PathBuf,
    output: PathBuf,
    cache: PathBuf,
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut guest = None;
        let mut output = None;
        let mut cache = PathBuf::from(".cache/nanocodex/reproducible-guest");
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--guest" => guest = Some(value(&mut arguments, "--guest")?.into()),
                "--output" => output = Some(value(&mut arguments, "--output")?.into()),
                "--cache" => cache = value(&mut arguments, "--cache")?.into(),
                "--help" | "-h" => {
                    println!(
                        "usage: build_reproducible_guest --guest PATH --output DIRECTORY [--cache DIRECTORY]"
                    );
                    std::process::exit(0);
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {other:?}"),
                    ));
                }
            }
        }
        Ok(Self {
            guest: guest.ok_or_else(|| missing("--guest"))?,
            output: output.ok_or_else(|| missing("--output"))?,
            cache,
        })
    }
}

fn value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, io::Error> {
    arguments
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}")))
}

fn missing(option: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}"))
}
