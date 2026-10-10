//! Render `deploy/nats/ratatoskr.conf` with generated nkeys.
//!
//! ```text
//! render-nats-config <conf> <out_dir>
//! ```
//!
//! Writes `<out_dir>/ratatoskr.conf`, identical to the input except for the nkey tokens, and one
//! `<identity>.nkey` seed file per identity (`edge.nkey`, `extractor-browser-worker.nkey`, ...).
//! The files are mode 0644 because the consumers are throwaway containers in a CI temp directory
//! that run as other users. Never point this at a production directory.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::ExitCode;

fn write_readable(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents)
        .map_err(|error| format!("{} could not be written: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        .map_err(|error| format!("{} could not be made readable: {error}", path.display()))
}

fn run(conf_path: &Path, out_dir: &Path) -> Result<(), String> {
    let conf = std::fs::read_to_string(conf_path)
        .map_err(|error| format!("{} could not be read: {error}", conf_path.display()))?;
    let rendered = platform_nats_profile::render(&conf).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(out_dir)
        .map_err(|error| format!("{} could not be created: {error}", out_dir.display()))?;
    write_readable(&out_dir.join("ratatoskr.conf"), &rendered.conf)?;
    for seed in &rendered.seeds {
        write_readable(&out_dir.join(seed.file_name()), &seed.seed)?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [conf, out_dir] = arguments.as_slice() else {
        eprintln!("usage: render-nats-config <conf> <out_dir>");
        return ExitCode::from(64);
    };
    match run(Path::new(conf), Path::new(out_dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("render-nats-config: {message}");
            ExitCode::FAILURE
        }
    }
}
