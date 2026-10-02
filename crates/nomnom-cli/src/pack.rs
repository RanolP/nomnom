//! `nomnom pack` — add, list, trust and pin the rule packs a run loads.
//!
//! Two of these subcommands print more than a confirmation, and `docs/lang.md`
//! is the reason for both.
//!
//! `add` prints the SHA it pinned to, because a pack is "pinned to a commit,
//! never to a branch": a user who typed `@main` has to see that `main` is not
//! what was recorded, or they will believe their lock tracks the branch.
//!
//! `trust` prints the pack's name, URL and pinned SHA *before* recording the
//! grant. Trust is "granted per pack, deliberately, once", and a grant whose
//! sentence does not name what is being trusted is not deliberate — it is a
//! name the user typed being echoed back at them.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Subcommand;
use nomnom_core::verdict::{KnownPack, PackRow, find_pack, pack_inventory};
use nomnom_pack::{Lock, LockedPack, Store, Tier, Trust};
use serde::Serialize;

#[derive(Debug, Subcommand)]
pub enum PackCommand {
    /// Fetch a pack, pin it to a commit, and record it in `.nomnom/packs.lock`.
    Add {
        /// `github.com/org/repo/subdir`, or any git URL, optionally `@<ref>`.
        url: String,
        #[arg(long)]
        json: bool,
    },
    /// Every pack this project resolves: name, tier, pinned SHA, trust state.
    List {
        /// Also list an explicit pack directory, as `--pack` would load it.
        #[arg(long = "pack", value_name = "DIR")]
        packs: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Allow a pack's rules to say `reclaimable`.
    Trust {
        name: String,
        /// The directory a `--pack` pack lives in, so one loaded that way can
        /// be named here too. Trust is recorded by name, not by path.
        #[arg(long = "pack", value_name = "DIR")]
        packs: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Revoke trust. The pack stays pinned; its rules cap at `review` again.
    Untrust {
        name: String,
        #[arg(long = "pack", value_name = "DIR")]
        packs: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Re-resolve the ref in the lock and move the pin to it.
    Update {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Drop a pack from the lock.
    Remove {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// Pack commands do not take a scan root, so the project is the working
/// directory. `Lock::load` treats a project that has never added a pack as an
/// empty lock rather than an error, so this is safe anywhere.
fn project_root() -> Result<PathBuf> {
    std::env::current_dir().context("cannot read the working directory to find .nomnom/packs.lock")
}

pub fn run(command: PackCommand) -> Result<ExitCode> {
    let root = project_root()?;
    match command {
        PackCommand::Add { url, json } => add(&root, &url, json),
        PackCommand::List { packs, json } => list(&root, &packs, json),
        PackCommand::Trust { name, packs, json } => set_trust(&root, &name, &packs, true, json),
        PackCommand::Untrust { name, packs, json } => set_trust(&root, &name, &packs, false, json),
        PackCommand::Update { name, json } => update(&root, &name, json),
        PackCommand::Remove { name, json } => remove(&root, &name, json),
    }
}

fn add(root: &Path, url: &str, json: bool) -> Result<ExitCode> {
    let store = Store::open()?;
    let mut lock = Lock::load(root)?;
    let added = nomnom_pack::add(&store, &mut lock, url)?;
    lock.save(root)?;

    if json {
        print_json(&AddOutput { added: &added, lock: &lock_path(root) })?;
        return Ok(ExitCode::SUCCESS);
    }

    println!("Added pack `{}`.", added.name);
    println!("  url:    {}", added.url.as_deref().unwrap_or("-"));
    println!("  pinned: {}", added.sha.as_deref().unwrap_or("-"));
    if let Some(subdir) = &added.subdir {
        println!("  subdir: {subdir}");
    }
    println!("  lock:   {}", lock_path(root));
    println!();
    println!("A pack is pinned to a commit, never to a branch: the SHA above is what loads.");
    if added.trusted {
        println!("Pack `{}` is trusted; its rules may say `reclaimable`.", added.name);
    } else {
        println!(
            "Pack `{}` is not trusted, so its `reclaimable` rules show as `review`.",
            added.name
        );
        println!("Run `nomnom pack trust {}` once you have read them.", added.name);
    }
    Ok(ExitCode::SUCCESS)
}

fn list(root: &Path, explicit: &[PathBuf], json: bool) -> Result<ExitCode> {
    let rows: Vec<Row> = pack_inventory(root, explicit)?.into_iter().map(Row::from).collect();

    if json {
        print_json(&ListOutput { lock: &lock_path(root), packs: &rows })?;
        return Ok(ExitCode::SUCCESS);
    }

    let width = |f: fn(&Row) -> &str| rows.iter().map(|r| f(r).len()).max().unwrap_or(0);
    let (name_w, tier_w, trust_w) =
        (width(|r| &r.name).max(4), width(|r| r.tier).max(4), width(|r| r.trust).max(5));
    println!("{:name_w$}  {:tier_w$}  {:trust_w$}  PINNED", "NAME", "TIER", "TRUST");
    for row in &rows {
        println!(
            "{:name_w$}  {:tier_w$}  {:trust_w$}  {}",
            row.name,
            row.tier,
            row.trust,
            row.sha.as_deref().unwrap_or("-")
        );
    }
    if rows.iter().any(|row| row.trust == UNTRUSTED) {
        println!();
        println!("An untrusted pack's `reclaimable` rules are capped at `review`.");
        println!("Grant it with `nomnom pack trust <name>`.");
    }
    Ok(ExitCode::SUCCESS)
}

fn set_trust(
    root: &Path,
    name: &str,
    explicit: &[PathBuf],
    trusted: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut lock = Lock::load(root)?;
    let known = Known::from(find_pack(root, name, explicit, &lock)?);

    if !json {
        if trusted {
            // Named in full before the grant is recorded, not after.
            println!("Trusting pack `{name}`:");
            println!("  url:    {}", known.url.as_deref().unwrap_or("(a local directory)"));
            println!("  pinned: {}", known.sha.as_deref().unwrap_or("-"));
            println!("  from:   {}", known.dir);
            println!();
            println!(
                "Its rules may then say `reclaimable`, which makes the paths they match \
                 deletion candidates."
            );
        } else {
            println!("Revoking trust for pack `{name}`.");
            println!("It stays pinned at {}; its rules cap at `review` again.", known.pinned());
        }
    }

    lock.set_trust(name, trusted);
    lock.save(root)?;

    if json {
        print_json(&TrustOutput { pack: &known, trusted, lock: &lock_path(root) })?;
    } else if trusted {
        println!("Trusted `{name}`.");
    } else {
        println!("Untrusted `{name}`.");
    }
    Ok(ExitCode::SUCCESS)
}

fn update(root: &Path, name: &str, json: bool) -> Result<ExitCode> {
    let store = Store::open()?;
    let mut lock = Lock::load(root)?;
    let before = lock.get(name).and_then(|pack| pack.sha.clone());
    let moved = nomnom_pack::update(&store, &mut lock, name)?;
    lock.save(root)?;

    if json {
        print_json(&UpdateOutput { pack: &moved, was: before.as_deref(), lock: &lock_path(root) })?;
        return Ok(ExitCode::SUCCESS);
    }
    let now = moved.sha.as_deref().unwrap_or("-");
    match before.as_deref() {
        Some(was) if was == now => println!("Pack `{name}` is already at {now}."),
        Some(was) => println!("Pack `{name}` moved from {was} to {now}."),
        None => println!("Pack `{name}` pinned at {now}."),
    }
    Ok(ExitCode::SUCCESS)
}

fn remove(root: &Path, name: &str, json: bool) -> Result<ExitCode> {
    let mut lock = Lock::load(root)?;
    if !lock.remove(name) {
        return Err(nomnom_pack::Error::NotLocked { name: name.to_string() }.into());
    }
    lock.save(root)?;
    if json {
        print_json(&RemoveOutput { removed: name, lock: &lock_path(root) })?;
    } else {
        println!("Removed `{name}` from {}.", lock_path(root));
        println!("Its cached checkout is left in place; nothing loads it now.");
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct Known {
    name: String,
    url: Option<String>,
    sha: Option<String>,
    dir: String,
}

impl From<KnownPack> for Known {
    fn from(pack: KnownPack) -> Self {
        Known { name: pack.name, url: pack.url, sha: pack.sha, dir: pack.dir.display().to_string() }
    }
}

impl Known {
    fn pinned(&self) -> &str {
        self.sha.as_deref().unwrap_or("its directory")
    }
}

const UNTRUSTED: &str = "untrusted";

#[derive(Serialize)]
struct Row {
    name: String,
    tier: &'static str,
    trust: &'static str,
    /// The pinned commit, or `null` for the built-in pack and for a pack that
    /// lives in a directory rather than in git.
    sha: Option<String>,
    url: Option<String>,
    dir: String,
}

impl From<PackRow> for Row {
    fn from(pack: PackRow) -> Row {
        Row {
            name: pack.name,
            tier: match pack.tier {
                None => "built-in",
                Some(Tier::User) => "user",
                Some(Tier::Project) => "project",
                Some(Tier::Explicit) => "explicit",
            },
            trust: match pack.trust {
                Trust::Builtin => "built-in",
                Trust::Trusted => "trusted",
                Trust::Untrusted => UNTRUSTED,
            },
            sha: pack.sha,
            url: pack.url,
            dir: pack.dir.map_or_else(|| "<built-in>".to_string(), |dir| dir.display().to_string()),
        }
    }
}

fn lock_path(root: &Path) -> String {
    Lock::path_in(root).display().to_string()
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[derive(Serialize)]
struct AddOutput<'a> {
    added: &'a LockedPack,
    lock: &'a str,
}

#[derive(Serialize)]
struct ListOutput<'a> {
    lock: &'a str,
    packs: &'a [Row],
}

#[derive(Serialize)]
struct TrustOutput<'a> {
    pack: &'a Known,
    trusted: bool,
    lock: &'a str,
}

#[derive(Serialize)]
struct UpdateOutput<'a> {
    pack: &'a LockedPack,
    was: Option<&'a str>,
    lock: &'a str,
}

#[derive(Serialize)]
struct RemoveOutput<'a> {
    removed: &'a str,
    lock: &'a str,
}
