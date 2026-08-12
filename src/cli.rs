//! Clap types only. Field doc comments are the --help text.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "greentree",
    version,
    about = "Test the tree, not every commit.",
    long_about = "greentree content-addresses your dirty working tree, caches check verdicts \
                  by tree hash, and only lets verified trees become commits. Exit codes and \
                  --json output are stable contracts; see docs/SPEC.md."
)]
pub struct Cli {
    /// Machine-readable JSON on stdout (agents: use this).
    #[arg(long, global = true)]
    pub json: bool,

    /// Run as if started in this directory.
    #[arg(long, short = 'C', global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Detect the project, write greentree.yaml, and warm the snapshot index.
    Init,

    /// Snapshot the tree and run checks; instant on cache hit.
    Test {
        /// A single check name; default runs every configured check.
        check: Option<String>,
        /// Re-run even when a cached verdict exists for this tree.
        #[arg(long)]
        no_cache: bool,
    },

    /// Show the current tree, its verdicts, and whether publish would succeed.
    Status,

    /// Create a commit from the current tree — refused unless verified.
    Publish {
        /// Also push (force-with-lease). Default is local commit only.
        #[arg(long)]
        push: bool,
        /// Commit message (a Greentree-Change-Id trailer is appended).
        #[arg(long, short)]
        message: Option<String>,
    },

    /// Run required checks (cache-aware), then publish if green. Idempotent.
    Gate {
        /// Also push (force-with-lease). Default is local commit only.
        #[arg(long)]
        push: bool,
        /// Commit message (a Greentree-Change-Id trailer is appended).
        #[arg(long, short)]
        message: Option<String>,
    },

    /// Run watch-marked checks whenever the tree settles; kill runs on edit.
    Watch {
        /// Process one settled cycle, then exit (for scripting/tests).
        #[arg(long)]
        once: bool,
    },

    /// Prune snapshot anchors and trim check logs.
    Gc {
        /// Snapshot anchors to keep (newest first).
        #[arg(long, default_value_t = 50)]
        keep: usize,
        /// Prune anchors older than this even inside the keep window.
        #[arg(long, default_value = "14d")]
        ttl: String,
        /// Byte budget for check logs, in MB.
        #[arg(long, default_value_t = 256)]
        log_budget_mb: u64,
    },
}
