# nomnom

Find what is eating your disk, say what each path is and why, and reclaim it with a journal you can undo. The rule language is described in [docs/lang.md](docs/lang.md).

## CLI

`nomnom` scans whole drives only: every command that scans takes a drive root such as `C:\`, `C:` or `D:/`, and refuses a folder with a nonzero exit.

```sh
cargo run -p nomnom-cli -- drives               # fixed drives: label, filesystem, used, free, total
cargo run -p nomnom-cli -- scan C:\             # the tree, biggest first
cargo run -p nomnom-cli -- types D:             # bytes, share and file count per extension
cargo run -p nomnom-cli -- largest D: -n 1000   # the largest files: size, modified, path
cargo run -p nomnom-cli -- suggest D:           # what each path is, and why
cargo run -p nomnom-cli -- clean D:             # the dry-run plan; add --apply to act
cargo run -p nomnom-cli -- undo                 # the journals there are to undo
cargo run -p nomnom-cli -- undo <journal>       # reverse an apply with the journal it printed
cargo run -p nomnom-cli -- pack list --drive D: # the rule packs a drive's runs load
```

The CLI and the GUI offer the same features, one subcommand per GUI screen.

`--json` gives machine-readable output on every command. A scan reads the NTFS Master File Table, which is much faster than walking the drive: when the shell is not already Administrator, nomnom asks through a UAC prompt, and if you decline it walks the drive instead and says so. In a terminal, a progress line shows the percent done, the entry count and the elapsed time. `--backend walk` never prompts; `--backend mft` fails rather than falling back. The pack lock a drive's runs obey lives at `<drive>\.nomnom\packs.lock`, the same file the GUI's Packs screen edits; `pack` commands use the working directory's drive unless `--drive` names another.

## GUI

`nomnom-gui` is a desktop front-end over the same pipeline as the `nomnom` CLI, laid out after WizTree, and it offers the same features as the CLI. The sidebar holds the drive views (Drives, Tree, File types, Largest files) and a Cleanup group (Suggest, Clean, Packs, Undo).

```sh
cargo run -p nomnom-gui
```

nomnom scans whole drives only, like the CLI. The app opens on **Drives**, one card per fixed drive; clicking a card scans that drive and opens the **Tree**: a size-sorted tree table above a treemap, where clicking a rectangle selects its entry in the tree. A progress bar shows how far the scan has got, with the entry count and elapsed time.

Every scan of an NTFS drive asks for Administrator access through a UAC prompt, because reading the Master File Table is much faster than walking the drive and also reports on-disk sizes. Decline the prompt, or let the elevated scan fail, and nomnom walks the drive instead and says so in a banner. When the app already runs as Administrator, no prompt appears. The header's **Backend** switch matches the CLI's `--backend`: **Walk** never prompts, and **MFT** fails rather than falling back.

Suggest and Clean judge the drive only when you press **Analyze**, since hashing duplicate candidates on a whole drive takes minutes. Clean always shows the dry run first, and Apply asks for confirmation before anything moves.

A release build (`cargo build -p nomnom-gui --release`) compiles GPUI's shaders with the Windows SDK's `fxc.exe`. When Windows SDK 10.0.26100.0 is not installed, point `GPUI_FXC_PATH` at the `fxc.exe` of an SDK you do have, for example:

```powershell
$env:GPUI_FXC_PATH = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.22000.0\x64\fxc.exe"
```

Debug builds need nothing extra.
