mod gen;
mod inspect;
mod mkv;
mod verify;
mod verify_manifest;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "binstruct", about = "Videofuser binstruct toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate a binstruct EBML file from an intermediate MKV.
    Gen {
        /// Intermediate MKV file (input).
        #[arg(long)]
        mkv: PathBuf,
        /// Torrent root directory (contains video/, audio/, info/).
        #[arg(long)]
        torrent_root: PathBuf,
        /// Publisher name to embed in the binstruct.
        #[arg(long)]
        publisher: String,
        /// Enable zstd compression of the output (sets ConfigFlags bit 0).
        #[arg(long, default_value_t = false)]
        compress: bool,
        /// Output path for the generated binstruct EBML file.
        #[arg(long)]
        output: PathBuf,
    },
    /// Print a human-readable dump of a binstruct EBML file.
    Inspect {
        /// Path to the binstruct EBML file.
        binstruct: PathBuf,
    },
    /// Verify integrity of a binstruct against the torrent files.
    Verify {
        /// Path to the binstruct EBML file.
        binstruct: PathBuf,
        /// Torrent root directory.
        #[arg(long)]
        torrent_root: PathBuf,
    },
    /// Verify coherence between the four manifest formats in an info/ directory.
    VerifyManifest {
        /// Path to the info/ directory containing the manifest files.
        info_dir: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Gen {
            mkv,
            torrent_root,
            publisher,
            compress,
            output,
        } => gen::run(gen::GenArgs {
            mkv,
            torrent_root,
            publisher,
            compress,
            output,
        }),
        Commands::Inspect { binstruct } => inspect::run(&binstruct),
        Commands::Verify {
            binstruct,
            torrent_root,
        } => verify::run(&binstruct, &torrent_root),
        Commands::VerifyManifest { info_dir } => verify_manifest::run(&info_dir),
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
