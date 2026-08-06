//! `plamenix` — plugin authoring CLI binary.
//!
//! Thin entry point that parses CLI args via `clap` and delegates
//! to the library's subcommand handlers. Adding a new subcommand
//! means extending the `Subcommand` enum + dispatch match.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use plamenix_cli::{
    BuildOptions, NewPluginOptions, build_plugin, install_plugin, keygen, pack_plugin,
    render_report, scaffold_new_plugin, sign as sign_archive_cli, validate_target,
};

#[derive(Debug, Parser)]
#[command(
    name = "plamenix",
    version,
    about = "Plamenix plugin authoring CLI (scaffold, build, pack, validate)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scaffold a new plugin directory.
    New(NewArgs),
    /// Compile the wasm + UI halves and assemble a `.plx` archive.
    Build(BuildArgs),
    /// Assemble a `.plx` from a directory that already has its
    /// compiled artifacts in place (no cargo / npm invocation).
    Pack(PackArgs),
    /// Validate a `.plx` archive or plugin source directory.
    Validate(ValidateArgs),
    /// Install a `.plx` into a running host (M1 stub — HTTP client
    /// deferred to M2).
    Install(InstallArgs),
    /// Generate a fresh Ed25519 signing keypair.
    Keygen(KeygenArgs),
    /// Sign a `.plx` archive in-place with an Ed25519 secret key.
    Sign(SignArgs),
}

#[derive(Debug, Parser)]
struct KeygenArgs {
    /// Output path for the 32-byte secret key file (mode 0600). The
    /// matching verifying key is written to `<path>.pub` (mode 0644).
    path: PathBuf,
}

#[derive(Debug, Parser)]
struct SignArgs {
    /// `.plx` archive to sign in-place.
    archive: PathBuf,
    /// Path to the 32-byte Ed25519 secret key file.
    #[arg(long)]
    key: PathBuf,
    /// Optional key id embedded in the signature (free-form label).
    #[arg(long, default_value = "")]
    key_id: String,
}

#[derive(Debug, Parser)]
struct PackArgs {
    /// Plugin source directory. Defaults to the current working
    /// directory.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Output directory for the produced `.plx`. Defaults to the
    /// plugin directory.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct ValidateArgs {
    /// `.plx` archive OR plugin source directory.
    target: PathBuf,
}

#[derive(Debug, Parser)]
struct InstallArgs {
    /// `.plx` archive to install.
    archive: PathBuf,
    /// Server base URL (e.g. `http://localhost:3000`). The CLI will
    /// POST to `<server>/api/plugins/install` once the HTTP client
    /// lands.
    #[arg(long)]
    server: String,
    /// Optional permissions to grant at install time. May repeat:
    /// `--grant network.fetch --grant fs.read`.
    #[arg(long = "grant")]
    grant: Vec<String>,
}

#[derive(Debug, Parser)]
struct BuildArgs {
    /// Plugin source directory. Defaults to the current working
    /// directory.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Output directory for the produced `.plx`. Defaults to the
    /// plugin directory.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Skip `cargo build` even when `Cargo.toml` is present.
    #[arg(long)]
    skip_wasm: bool,
    /// Skip the UI build even when `package.json` is present.
    #[arg(long)]
    skip_ui: bool,
    /// Skip `npm/pnpm install` (useful when CI installed already).
    #[arg(long)]
    skip_install: bool,
}

#[derive(Debug, Parser)]
struct NewArgs {
    /// Plugin id (e.g. `org.example.fmt`). Becomes the manifest id +
    /// the derived crate / npm package names.
    plugin_id: String,
    /// Destination directory. Defaults to `<plugin_id>` in the
    /// current working directory.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Display name shown to users (defaults to a humanised version
    /// of the trailing path segment of `plugin_id`).
    #[arg(long)]
    name: Option<String>,
    /// One-line description.
    #[arg(long)]
    description: Option<String>,
    /// Author string written to the manifest.
    #[arg(long)]
    author: Option<String>,
    /// Initial SemVer (default `0.1.0`).
    #[arg(long)]
    version: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::New(args) => match run_new(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Build(args) => match run_build(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Pack(args) => match run_pack(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Validate(args) => match run_validate(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Install(args) => match run_install(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Keygen(args) => match run_keygen(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Sign(args) => match run_sign(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

fn run_keygen(args: KeygenArgs) -> Result<(), plamenix_cli::SignCliError> {
    let out = keygen(&args.path)?;
    println!(
        "secret: {}\npublic: {} ({})",
        out.secret_path.display(),
        out.public_path.display(),
        out.public_key_hex,
    );
    Ok(())
}

fn run_sign(args: SignArgs) -> Result<(), plamenix_cli::SignCliError> {
    let sig = sign_archive_cli(&args.archive, &args.key, &args.key_id)?;
    println!(
        "signed {} (algorithm: {}, public_key: {})",
        args.archive.display(),
        sig.algorithm,
        sig.public_key,
    );
    Ok(())
}

fn run_pack(args: PackArgs) -> Result<(), plamenix_cli::PackError> {
    let dir = args
        .path
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let output = pack_plugin(&dir, args.out.as_deref())?;
    println!(
        "packed {} v{} → {}",
        output.plugin_id,
        output.plugin_version,
        output.plx_path.display(),
    );
    Ok(())
}

fn run_validate(args: ValidateArgs) -> Result<(), plamenix_cli::ValidateError> {
    let report = validate_target(&args.target)?;
    println!("{}", render_report(&report));
    Ok(())
}

fn run_install(args: InstallArgs) -> Result<(), plamenix_cli::InstallError> {
    let output = install_plugin(&args.archive, &args.server, &args.grant)?;
    println!(
        "installed {} v{} (grants: {})",
        output.plugin_id,
        output.plugin_version,
        output.granted_optional.join(", "),
    );
    Ok(())
}

fn run_build(args: BuildArgs) -> Result<(), plamenix_cli::BuildError> {
    let dir = args
        .path
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let opts = BuildOptions {
        output_dir: args.out,
        skip_wasm: args.skip_wasm,
        skip_ui: args.skip_ui,
        skip_install: args.skip_install,
    };
    let output = build_plugin(&dir, &opts)?;
    println!(
        "built {} (wasm: {}, ui: {})",
        output.plx_path.display(),
        if output.built_wasm { "yes" } else { "skipped" },
        if output.built_ui { "yes" } else { "skipped" },
    );
    Ok(())
}

fn run_new(args: NewArgs) -> Result<(), plamenix_cli::ScaffoldError> {
    let target = args.path.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .join(&args.plugin_id)
    });
    let mut opts = NewPluginOptions::from_id(args.plugin_id);
    if let Some(name) = args.name {
        opts.display_name = name;
    }
    opts.description = args.description;
    opts.author = args.author;
    opts.version = args.version;
    scaffold_new_plugin(&target, &opts)?;
    println!("scaffolded plugin at {}", target.display());
    Ok(())
}
