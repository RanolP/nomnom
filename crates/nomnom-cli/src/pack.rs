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

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Subcommand;
use nomnom_core::scan::VolumeRoot;
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
    /// Every pack this drive resolves: name, tier, pinned SHA, trust state.
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

/// The lock a run reads lives on the drive it scans, at
/// `<drive>\.nomnom\packs.lock` — the same file the GUI's Packs screen edits —
/// so a grant made here is the one `suggest` and `clean` on that drive obey.
/// Without `--drive`, the drive is the one holding the working directory.
fn drive_lock_root(drive: Option<VolumeRoot>) -> Result<VolumeRoot> {
    if let Some(drive) = drive {
        return Ok(drive);
    }
    let cwd =
        std::env::current_dir().context("cannot read the working directory to find its drive")?;
    let root: PathBuf = cwd
        .components()
        .take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
        .collect();
    VolumeRoot::new(&root).with_context(|| {
        format!("the working directory {} is not on a drive; pass --drive", cwd.display())
    })
}

pub fn run(drive: Option<VolumeRoot>, command: PackCommand) -> Result<ExitCode> {
    let root = drive_lock_root(drive)?;
    dispatch(root.as_path(), command, &mut std::io::stdout().lock())
}

pub(crate) fn dispatch(root: &Path, command: PackCommand, out: &mut dyn Write) -> Result<ExitCode> {
    match command {
        PackCommand::Add { url, json } => add(root, &url, json, out),
        PackCommand::List { packs, json } => list(root, &packs, json, out),
        PackCommand::Trust { name, packs, json } => set_trust(root, &name, &packs, true, json, out),
        PackCommand::Untrust { name, packs, json } => {
            set_trust(root, &name, &packs, false, json, out)
        }
        PackCommand::Update { name, json } => update(root, &name, json, out),
        PackCommand::Remove { name, json } => remove(root, &name, json, out),
    }
}

fn add(root: &Path, url: &str, json: bool, out: &mut dyn Write) -> Result<ExitCode> {
    let store = Store::open()?;
    let mut lock = Lock::load(root)?;
    let added = nomnom_pack::add(&store, &mut lock, url)?;
    lock.save(root)?;

    if json {
        print_json(out, &AddOutput { added: &added, lock: &lock_path(root) })?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(out, "Added pack `{}`.", added.name)?;
    writeln!(out, "  url:    {}", added.url.as_deref().unwrap_or("-"))?;
    writeln!(out, "  pinned: {}", added.sha.as_deref().unwrap_or("-"))?;
    if let Some(subdir) = &added.subdir {
        writeln!(out, "  subdir: {subdir}")?;
    }
    writeln!(out, "  lock:   {}", lock_path(root))?;
    writeln!(out)?;
    writeln!(out, "A pack is pinned to a commit, never to a branch: the SHA above is what loads.")?;
    if added.trusted {
        writeln!(out, "Pack `{}` is trusted; its rules may say `reclaimable`.", added.name)?;
    } else {
        writeln!(
            out,
            "Pack `{}` is not trusted, so its `reclaimable` rules show as `review`.",
            added.name
        )?;
        writeln!(out, "Run `nomnom pack trust {}` once you have read them.", added.name)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn list(root: &Path, explicit: &[PathBuf], json: bool, out: &mut dyn Write) -> Result<ExitCode> {
    let rows: Vec<Row> = pack_inventory(root, explicit)?.into_iter().map(Row::from).collect();

    if json {
        print_json(out, &ListOutput { lock: &lock_path(root), packs: &rows })?;
        return Ok(ExitCode::SUCCESS);
    }

    let width = |f: fn(&Row) -> &str| rows.iter().map(|r| f(r).len()).max().unwrap_or(0);
    let (name_w, tier_w, trust_w) =
        (width(|r| &r.name).max(4), width(|r| r.tier).max(4), width(|r| r.trust).max(5));
    writeln!(out, "{:name_w$}  {:tier_w$}  {:trust_w$}  PINNED", "NAME", "TIER", "TRUST")?;
    for row in &rows {
        writeln!(
            out,
            "{:name_w$}  {:tier_w$}  {:trust_w$}  {}",
            row.name,
            row.tier,
            row.trust,
            row.sha.as_deref().unwrap_or("-")
        )?;
    }
    if rows.iter().any(|row| row.trust == UNTRUSTED) {
        writeln!(out)?;
        writeln!(out, "An untrusted pack's `reclaimable` rules are capped at `review`.")?;
        writeln!(out, "Grant it with `nomnom pack trust <name>`.")?;
    }
    Ok(ExitCode::SUCCESS)
}

fn set_trust(
    root: &Path,
    name: &str,
    explicit: &[PathBuf],
    trusted: bool,
    json: bool,
    out: &mut dyn Write,
) -> Result<ExitCode> {
    let mut lock = Lock::load(root)?;
    let known = Known::from(find_pack(root, name, explicit, &lock)?);

    if !json {
        if trusted {
            // Named in full before the grant is recorded, not after.
            writeln!(out, "Trusting pack `{name}`:")?;
            writeln!(out, "  url:    {}", known.url.as_deref().unwrap_or("(a local directory)"))?;
            writeln!(out, "  pinned: {}", known.sha.as_deref().unwrap_or("-"))?;
            writeln!(out, "  from:   {}", known.dir)?;
            writeln!(out)?;
            writeln!(
                out,
                "Its rules may then say `reclaimable`, which makes the paths they match \
                 deletion candidates."
            )?;
        } else {
            writeln!(out, "Revoking trust for pack `{name}`.")?;
            writeln!(
                out,
                "It stays pinned at {}; its rules cap at `review` again.",
                known.pinned()
            )?;
        }
    }

    lock.set_trust(name, trusted);
    lock.save(root)?;

    if json {
        print_json(out, &TrustOutput { pack: &known, trusted, lock: &lock_path(root) })?;
    } else if trusted {
        writeln!(out, "Trusted `{name}`.")?;
    } else {
        writeln!(out, "Untrusted `{name}`.")?;
    }
    Ok(ExitCode::SUCCESS)
}

fn update(root: &Path, name: &str, json: bool, out: &mut dyn Write) -> Result<ExitCode> {
    let store = Store::open()?;
    let mut lock = Lock::load(root)?;
    let before = lock.get(name).and_then(|pack| pack.sha.clone());
    let moved = nomnom_pack::update(&store, &mut lock, name)?;
    lock.save(root)?;

    if json {
        print_json(
            out,
            &UpdateOutput { pack: &moved, was: before.as_deref(), lock: &lock_path(root) },
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let now = moved.sha.as_deref().unwrap_or("-");
    match before.as_deref() {
        Some(was) if was == now => writeln!(out, "Pack `{name}` is already at {now}.")?,
        Some(was) => writeln!(out, "Pack `{name}` moved from {was} to {now}.")?,
        None => writeln!(out, "Pack `{name}` pinned at {now}.")?,
    }
    Ok(ExitCode::SUCCESS)
}

fn remove(root: &Path, name: &str, json: bool, out: &mut dyn Write) -> Result<ExitCode> {
    let mut lock = Lock::load(root)?;
    if !lock.remove(name) {
        return Err(nomnom_pack::Error::NotLocked { name: name.to_string() }.into());
    }
    lock.save(root)?;
    if json {
        print_json(out, &RemoveOutput { removed: name, lock: &lock_path(root) })?;
    } else {
        writeln!(out, "Removed `{name}` from {}.", lock_path(root))?;
        writeln!(out, "Its cached checkout is left in place; nothing loads it now.")?;
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

fn print_json<T: Serialize>(out: &mut dyn Write, value: &T) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
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

/// Run against a temp directory's lock rather than a real drive's. The git
/// tests drive a real repository through the fixture helpers `nomnom-pack`'s
/// own acquire tests use, with no network.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack_fixtures as fixtures;
    use crate::test_support::{local_pack, pack_err, pack_ok};

    fn list_json(root: &Path, packs: Vec<PathBuf>) -> serde_json::Value {
        serde_json::from_str(&pack_ok(root, PackCommand::List { packs, json: true }))
            .expect("valid JSON")
    }

    /// The regression: `--drive` being ignored in favour of the working
    /// directory, or the lock landing anywhere but the drive's own
    /// `.nomnom\packs.lock` that `suggest` on that drive reads.
    #[cfg(windows)]
    #[test]
    fn an_explicit_drive_wins_and_names_its_lock() {
        let drive = VolumeRoot::new("d:").unwrap();
        assert_eq!(drive_lock_root(Some(drive.clone())).unwrap(), drive);
        assert_eq!(lock_path(drive.as_path()), "D:\\.nomnom\\packs.lock");
    }

    /// `pack list` is how a user answers "what rules is this run loading?". The
    /// regression: a listing that omits the built-in pack or loses a pack's
    /// tier or trust state, which are the two facts that explain a verdict
    /// they did not expect.
    #[test]
    fn pack_list_names_every_resolved_pack_with_its_tier_and_trust_in_both_forms() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let explicit = local_pack(root, "vendor", "reclaimable");
        fixtures::write_pack(
            &root.join(".nomnom").join("packs").join("house"),
            "house-style",
            &fixtures::rule("house-rule", "review"),
        );

        let text = pack_ok(root, PackCommand::List { packs: vec![explicit.clone()], json: false });
        for expected in ["NAME", "TIER", "TRUST", "PINNED", "built-in", "house-style", "project"] {
            assert!(text.contains(expected), "`{expected}` missing from:\n{text}");
        }
        assert!(text.contains("untrusted"), "{text}");

        let json = list_json(root, vec![explicit]);
        let packs = json["packs"].as_array().expect("packs array");
        let rows: Vec<(&str, &str, &str)> = packs
            .iter()
            .map(|row| {
                (
                    row["name"].as_str().expect("name"),
                    row["tier"].as_str().expect("tier"),
                    row["trust"].as_str().expect("trust"),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("builtin.ableton", "built-in", "built-in"),
                ("builtin.after-effects", "built-in", "built-in"),
                ("builtin.bun", "built-in", "built-in"),
                ("builtin.cargo", "built-in", "built-in"),
                ("builtin.chrome", "built-in", "built-in"),
                ("builtin.cmake", "built-in", "built-in"),
                ("builtin.cocoapods", "built-in", "built-in"),
                ("builtin.cpython", "built-in", "built-in"),
                ("builtin.dart", "built-in", "built-in"),
                ("builtin.dotnet", "built-in", "built-in"),
                ("builtin.downloads", "built-in", "built-in"),
                ("builtin.edge", "built-in", "built-in"),
                ("builtin.firefox", "built-in", "built-in"),
                ("builtin.go", "built-in", "built-in"),
                ("builtin.gradle", "built-in", "built-in"),
                ("builtin.maven", "built-in", "built-in"),
                ("builtin.mypy", "built-in", "built-in"),
                ("builtin.next", "built-in", "built-in"),
                ("builtin.npm", "built-in", "built-in"),
                ("builtin.nuget", "built-in", "built-in"),
                ("builtin.pip", "built-in", "built-in"),
                ("builtin.pnpm", "built-in", "built-in"),
                ("builtin.pytest", "built-in", "built-in"),
                ("builtin.ruff", "built-in", "built-in"),
                ("builtin.tox", "built-in", "built-in"),
                ("builtin.uv", "built-in", "built-in"),
                ("builtin.venv", "built-in", "built-in"),
                ("builtin.vscode", "built-in", "built-in"),
                ("builtin.windows-update", "built-in", "built-in"),
                ("builtin.yarn", "built-in", "built-in"),
                ("house-style", "project", "untrusted"),
                ("vendor", "explicit", "untrusted"),
            ],
            "resolution order is built-in, user, project, --pack"
        );
        // Every row carries the pin slot even when it is empty, so a consumer
        // never has to tell "no key" from "not pinned".
        assert!(packs.iter().all(|row| row.get("sha").is_some()), "{packs:?}");
    }

    /// `docs/lang.md`: a pack is "pinned to a commit, never to a branch". The
    /// regression: `add` acknowledging the branch name the user typed, which
    /// leaves them believing the lock tracks that branch.
    #[test]
    fn pack_add_prints_the_commit_sha_it_pinned_to_not_the_ref_that_was_asked_for() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let repo = root.join("packs.git");
        fixtures::init_repo(&repo);
        fixtures::write_pack(
            &repo.join("rust"),
            "rust",
            &fixtures::rule("cargo-target", "reclaimable"),
        );
        let sha = fixtures::commit(&repo, "the pack");
        let url = format!("{}/rust@main", fixtures::file_url(&repo));

        let text = pack_ok(root, PackCommand::Add { url, json: false });

        assert!(text.contains(&sha), "the resolved SHA is missing:\n{text}");
        assert!(text.contains("never to a branch"), "{text}");
        assert!(text.contains("not trusted"), "a freshly added pack is untrusted:\n{text}");

        let json = list_json(root, Vec::new());
        let rust = json["packs"]
            .as_array()
            .expect("packs")
            .iter()
            .find(|row| row["name"] == "rust")
            .expect("the added pack is listed");
        assert_eq!(rust["sha"].as_str(), Some(sha.as_str()));
        assert_eq!(rust["tier"].as_str(), Some("user"));

        // `update` is the only operation allowed to move the pin, and `remove`
        // takes the row back out again.
        let updated = pack_ok(root, PackCommand::Update { name: "rust".into(), json: false });
        assert!(updated.contains(&sha), "{updated}");
        let removed = pack_ok(root, PackCommand::Remove { name: "rust".into(), json: true });
        assert!(removed.contains("\"removed\": \"rust\""), "{removed}");
        let after = list_json(root, Vec::new());
        let left: Vec<&str> = after["packs"]
            .as_array()
            .expect("packs")
            .iter()
            .map(|row| row["tier"].as_str().expect("tier"))
            .collect();
        assert!(
            left.iter().all(|tier| *tier == "built-in"),
            "only the built-in packs are left: {left:?}"
        );
    }

    /// The regression: flattening a `nomnom-pack` error to "could not add
    /// pack". An auth failure and a missing repository both exit 128 from
    /// `git`, so the command and its stderr are what separate them, and `main`
    /// prints `{error:#}` — which only helps if the detail survives the chain.
    #[test]
    fn a_failed_git_fetch_surfaces_the_command_and_its_stderr() {
        let project = tempfile::tempdir().unwrap();
        let missing = fixtures::file_url(&project.path().join("no-such-repository"));

        let error = pack_err(project.path(), PackCommand::Add { url: missing, json: false });

        assert!(error.contains("ls-remote"), "the git command is missing:\n{error}");
        assert!(error.contains("repository"), "git's own stderr is missing:\n{error}");
        assert!(error.contains("command:"), "{error}");
        assert!(error.contains("stderr:"), "{error}");
    }

    /// Trust is keyed by a pack's own name, so a typo has no pack to attach
    /// to. The regression: writing a trust row for a name nothing resolves to,
    /// which reports success and leaves the real pack still capped.
    #[test]
    fn trusting_a_name_no_pack_resolves_to_fails_and_lists_what_does() {
        let project = tempfile::tempdir().unwrap();
        let pack = local_pack(project.path(), "vendor", "reclaimable");

        let error = pack_err(
            project.path(),
            PackCommand::Trust { name: "vender".into(), packs: vec![pack], json: false },
        );

        assert!(error.contains("no pack named `vender`"), "{error}");
        assert!(error.contains("vendor"), "the error must name what does resolve:\n{error}");
        assert!(!Lock::path_in(project.path()).exists(), "a lock was written");
    }
}
