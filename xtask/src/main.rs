use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Read,
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
    let source = build(&root, options.build_directory)?;
    ensure!(
        source.is_file()
            && source
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")),
        "expected a Windows executable"
    );
    let original_hash = file_hash(&source)?;
    let original_bytes = source.metadata()?.len();
    check_startup(&source, &root)?;

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
    let candidate = stage.path().join("OpenUUYC.exe");
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
    check_startup(&candidate, &root)?;
    ensure!(
        file_hash(&source)? == original_hash,
        "original build changed during packaging"
    );

    let destination = target
        .join("dist")
        .join(if options.upx { "upx" } else { "raw" });
    fs::create_dir_all(&destination)?;
    fs::copy(&candidate, destination.join("OpenUUYC.exe"))
        .context("publish executable (close it first if in use)")?;

    println!(
        "EXE: {:.2} -> {:.2} MiB; output: {}",
        original_bytes as f64 / 1048576.0,
        candidate.metadata()?.len() as f64 / 1048576.0,
        destination.display()
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

fn build(root: &Path, directory: Option<PathBuf>) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = command(cargo)
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
            executables.push(PathBuf::from(path));
        }
    }
    ensure!(output.status.success(), "Cargo release build failed");
    ensure!(
        executables.len() == 1,
        "Cargo did not identify one OpenUUYC executable"
    );
    // UPX does not accept Rust's Windows verbatim-path prefix on ordinary paths.
    dunce::canonicalize(executables.pop().unwrap()).context("resolve built executable")
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

fn check_startup(executable: &Path, root: &Path) -> Result<()> {
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
            && String::from_utf8_lossy(&output)
                .trim()
                .starts_with("OpenUUYC "),
        "release startup check failed: {}",
        String::from_utf8_lossy(&errors)
    );
    Ok(())
}
