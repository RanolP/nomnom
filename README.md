# nomnom

Find what is eating your disk, say what each path is and why, and reclaim it by deleting what you approve. Deletion is permanent: there is no recycle bin and no undo. The rule language is described in [docs/lang.md](docs/lang.md).

## CLI

`nomnom` scans whole drives only: every command that scans takes a drive root such as `C:\`, `C:` or `D:/`, and refuses a folder with a nonzero exit.

```sh
cargo run -p nomnom-cli -- drives               # fixed drives: label, filesystem, used, free, total
cargo run -p nomnom-cli -- scan C:\             # the tree, biggest first
cargo run -p nomnom-cli -- suggest D:           # what each path is, and why
cargo run -p nomnom-cli -- clean D:             # list the cleanup candidates grouped by rule; approves nothing
cargo run -p nomnom-cli -- clean D: --rule "Cargo target/"        # dry-run plan of every match of that rule, minus exclusions
cargo run -p nomnom-cli -- clean D: --rule "Cargo target/" --apply  # delete them permanently
cargo run -p nomnom-cli -- clean D: --exclude D:\work\active      # keep a path and its subtree out of every plan, across scans
cargo run -p nomnom-cli -- clean D: --exclusions                  # list the drive's exclusions (no scan)
cargo run -p nomnom-cli -- clean D: D:\proj\node_modules --apply  # permanently delete only the named paths
cargo run -p nomnom-cli -- pack list --drive D: # the rule packs a drive's runs load
```

`clean` is opt-in, and you approve rules rather than files. `nomnom clean <DRIVE>` lists the candidates (every `reclaimable` verdict, plus every `review` one with `--include-review`) grouped by the rule that matched them, as `pack [Title]`, and plans nothing. `--rule <RULE>` (repeatable; the title, or `pack [Title]` when two packs share it) approves a rule, which plans every one of its matches except the excluded ones. `nomnom clean <DRIVE> <PATH>...` plans single candidates; any path that is not a listed candidate fails the command with its name. `--apply` with no rule and no path is an error. Approvals last one run. Exclusions persist: `--exclude <PATH>` keeps a path and everything under it out of every plan on that drive, `--unexclude <PATH>` takes it back, and `--exclusions` lists them. The list lives at `<drive>\.nomnom\exclusions.toml`, and an exclusion can only shrink a plan, never add to it.

The CLI and the GUI offer the same features: `drives`, `scan`, `suggest`, `clean --rule … --exclude … --apply` and `pack` are the GUI's Drives, Tree, rule list (approve a rule, open it to see and exclude its matches, undo exclusions in its Exclusions panel), Reclaim button and Packs.

`--json` gives machine-readable output on every command. A scan reads the NTFS Master File Table, which is much faster than walking the drive: when the shell is not already Administrator, nomnom asks through a UAC prompt, and if you decline it walks the drive instead and says so. In a terminal, a progress line shows the percent done, the entry count and the elapsed time. The pack lock a drive's runs obey lives at `<drive>\.nomnom\packs.lock`, the same file the GUI's Packs screen edits; `pack` commands use the working directory's drive unless `--drive` names another.

## GUI

`nomnom-gui` is a desktop front-end over the same pipeline as the `nomnom` CLI, laid out after WizTree, and it offers the same features as the CLI. Its screens stack: **Drives** is the root, picking a drive pushes its **Tree**, and the header's **Back** button pops. The header's **Packs** button opens the rule packs.

```sh
cargo run -p nomnom-gui
```

nomnom scans whole drives only, like the CLI. The app opens on **Drives**, one card per fixed drive; clicking a card scans that drive and opens the **Tree**: a size-sorted tree table above a treemap, where clicking a rectangle selects its entry in the tree. A progress bar shows how far the scan has got, with the entry count and elapsed time.

Every scan of an NTFS drive asks for Administrator access through a UAC prompt, because reading the Master File Table is much faster than walking the drive and also reports on-disk sizes. Decline the prompt, or let the elevated scan fail, and nomnom walks the drive instead and says so in a banner. When the app already runs as Administrator, no prompt appears.

When a scan finishes, nomnom judges every path in the background while the tree and treemap stay usable. The Tree's bottom bar holds **Files to delete** on the left, with the count of paths you checked, which opens the dry-run list where every candidate starts unchecked and you check the paths to delete (include review verdicts to widen the list), and **Reclaim** on the right, which shows **Analyzing…** until the judging is done, then the size of what you checked, and, after a confirmation, permanently deletes only that, showing a progress bar and a log line per path (deleted, failed with its error, or skipped) that stays until you dismiss it. A new assessment, a rescan, a pack change or an apply clears the checks.

A release build (`cargo build -p nomnom-gui --release`) compiles GPUI's shaders with the Windows SDK's `fxc.exe`. When Windows SDK 10.0.26100.0 is not installed, point `GPUI_FXC_PATH` at the `fxc.exe` of an SDK you do have, for example:

```powershell
$env:GPUI_FXC_PATH = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.22000.0\x64\fxc.exe"
```

Debug builds need nothing extra.
