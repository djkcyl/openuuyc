use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Options {
    upx: bool,
    build_directory: Option<PathBuf>,
}

fn main() -> Result<()> {
    let mut options = Options::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--upx") => options.upx = true,
            Some("--build-directory") => {
                options.build_directory = Some(
                    args.next()
                        .context("--build-directory needs a path")?
                        .into(),
                );
            }
            Some("--help" | "-h") => {
                println!(
                    "cargo dist [--upx] [--build-directory PATH]\n\
                    Builds the Windows release, checks it and creates target/dist/raw or target/dist/upx.\n\
                    UPX must be on PATH when --upx is selected."
                );
                return Ok(());
            }
            _ => bail!("unknown release option: {}", arg.to_string_lossy()),
        }
    }
    ensure!(
        cfg!(windows),
        "native release packaging is currently implemented for Windows only"
    );
    let root = dunce::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .context("xtask must be inside the project")?,
    )?;
    if options.upx {
        ensure!(
            command("upx")
                .arg("--version")
                .stdout(Stdio::null())
                .status()
                .context("UPX must be installed and available on PATH")?
                .success(),
            "UPX is unavailable"
        );
    }
    let (source, version) = build(&root, options.build_directory)?;
    ensure!(
        source.is_file()
            && source
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")),
        "expected a Windows executable"
    );
    let original_hash = file_hash(&source)?;
    let original_bytes = source.metadata()?.len();
    let architecture = pe_architecture(&source)?;
    let file_name = format!("OpenUUYC-v{version}-windows-{architecture}.exe");
    check_startup(&source, &root, &version)?;

    let target = root.join("target");
    fs::create_dir_all(&target)?;
    let stage = tempfile::Builder::new()
        .prefix(".dist-")
        .tempdir_in(&target)?;
    ensure!(
        stage
            .path()
            .canonicalize()?
            .starts_with(target.canonicalize()?),
        "invalid staging directory"
    );
    let candidate = stage.path().join(&file_name);
    if options.upx {
        ensure!(
            command("upx")
                .args(["--best", "--lzma", "-o"])
                .arg(&candidate)
                .arg(&source)
                .status()
                .context("compress executable")?
                .success(),
            "UPX compression failed; original build retained"
        );
        ensure!(
            command("upx").arg("-t").arg(&candidate).status()?.success(),
            "UPX integrity check failed"
        );
    } else {
        fs::copy(&source, &candidate)?;
    }
    check_startup(&candidate, &root, &version)?;
    ensure!(
        file_hash(&source)? == original_hash,
        "original build changed during packaging"
    );

    let destination = target
        .join("dist")
        .join(if options.upx { "upx" } else { "raw" });
    fs::create_dir_all(&destination)?;
    let published = destination.join(file_name);
    fs::copy(&candidate, &published).context("publish executable (close it first if in use)")?;

    println!(
        "EXE: {:.2} -> {:.2} MiB; output: {}",
        original_bytes as f64 / 1048576.0,
        candidate.metadata()?.len() as f64 / 1048576.0,
        published.display()
    );
    // Only this freshly created directory is removed; build and published files remain.
    stage.close()?;
    Ok(())
}

fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW for build/check helpers.
    }
    command
}

fn build(root: &Path, directory: Option<PathBuf>) -> Result<(PathBuf, String)> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = command(&cargo)
        .current_dir(root)
        .args([
            "build",
            "--release",
            "--locked",
            "--bin",
            "OpenUUYC",
            "--message-format=json",
        ])
        .arg("--target-dir")
        .arg(root.join(directory.unwrap_or_else(|| "target/build-release".into())))
        .stderr(Stdio::inherit())
        .output()
        .context("build release executable")?;
    let mut executables = Vec::new();
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: serde_json::Value =
            serde_json::from_slice(line).context("read Cargo build message")?;
        if message["reason"] == "compiler-message" {
            if let Some(rendered) = message["message"]["rendered"].as_str() {
                eprint!("{rendered}");
            }
        } else if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == "OpenUUYC"
            && let Some(path) = message["executable"].as_str()
        {
            executables.push((
                PathBuf::from(path),
                message["package_id"]
                    .as_str()
                    .context("artifact package ID missing")?
                    .to_owned(),
            ));
        }
    }
    ensure!(output.status.success(), "Cargo release build failed");
    ensure!(
        executables.len() == 1,
        "Cargo did not identify one OpenUUYC executable"
    );
    let (path, package_id) = executables.pop().unwrap();
    let metadata = command(cargo)
        .current_dir(root)
        .args(["metadata", "--locked", "--no-deps", "--format-version=1"])
        .stderr(Stdio::inherit())
        .output()
        .context("read built package version")?;
    ensure!(metadata.status.success(), "Cargo metadata failed");
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let package = metadata["packages"]
        .as_array()
        .context("missing Cargo packages")?
        .iter()
        .find(|package| package["id"] == package_id)
        .context("built package no longer matches Cargo metadata")?;
    let version = package["version"]
        .as_str()
        .context("missing package version")?
        .to_owned();
    ensure!(
        version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
        "package version is not a valid filename component"
    );
    // UPX does not accept Rust's Windows verbatim-path prefix on ordinary paths.
    Ok((
        dunce::canonicalize(path).context("resolve built executable")?,
        version,
    ))
}

fn pe_architecture(path: &Path) -> Result<&'static str> {
    let mut file = File::open(path)?;
    let mut dos = [0; 64];
    file.read_exact(&mut dos)
        .context("read executable DOS header")?;
    ensure!(&dos[..2] == b"MZ", "executable is not a PE image");
    let offset = u32::from_le_bytes(dos[60..64].try_into()?);
    file.seek(SeekFrom::Start(u64::from(offset)))?;
    let mut pe = [0; 6];
    file.read_exact(&mut pe)
        .context("read executable PE header")?;
    ensure!(&pe[..4] == b"PE\0\0", "invalid PE signature");
    // Read the built image, not the host running this task, to label cross builds correctly.
    match u16::from_le_bytes([pe[4], pe[5]]) {
        0x8664 => Ok("x86_64"),
        0x014c => Ok("i686"),
        0xaa64 => Ok("aarch64"),
        machine => bail!("unsupported Windows architecture: {machine:#x}"),
    }
}

fn file_hash(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().to_vec())
}

fn check_startup(executable: &Path, root: &Path, version: &str) -> Result<()> {
    let mut child = command(executable)
        .arg("--version")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start release check")?;
    let mut stdout = child.stdout.take().context("missing check output pipe")?;
    let mut stderr = child.stderr.take().context("missing check error pipe")?;
    let output = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let errors = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(15) {
            // This child was created here with --version and has no GUI session.
            let _ = child.kill();
            let _ = child.wait();
            let _ = output.join();
            let _ = errors.join();
            bail!("release startup check timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("check output reader failed"))??;
    let errors = errors
        .join()
        .map_err(|_| anyhow::anyhow!("check error reader failed"))??;
    ensure!(
        status.success()
            && String::from_utf8_lossy(&output).trim() == format!("OpenUUYC {version}"),
        "release startup check failed: {}",
        String::from_utf8_lossy(&errors)
    );
    Ok(())
}
