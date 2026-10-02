# nomnom

Find what is eating your disk, say what each path is and why, and reclaim it with a journal you can undo. The rule language is described in [docs/lang.md](docs/lang.md).

## GUI

`nomnom-gui` is a desktop front-end over the same pipeline as the `nomnom` CLI: Scan, Suggest, Clean, Packs and Undo, each a screen in the sidebar.

```sh
cargo run -p nomnom-gui
```

Pass a folder as the first argument (or set `NOMNOM_GUI_ROOT`) to open and scan it at launch; otherwise choose one with **Choose folder…**. Clean always shows the dry run first, and Apply asks for confirmation before anything moves.

A release build (`cargo build -p nomnom-gui --release`) compiles GPUI's shaders with the Windows SDK's `fxc.exe`. When Windows SDK 10.0.26100.0 is not installed, point `GPUI_FXC_PATH` at the `fxc.exe` of an SDK you do have, for example:

```powershell
$env:GPUI_FXC_PATH = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.22000.0\x64\fxc.exe"
```

Debug builds need nothing extra.
