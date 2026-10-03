//! Packs: the rule packs the scan root resolves, and the lock that pins them.
//!
//! Every operation runs against `<scan root>/.nomnom/packs.lock`, off the UI
//! thread (add, update and trust can reach the network), and re-judges the
//! catalog afterwards because a pack change alters which rules load or how far
//! they are trusted.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use nomnom_core::action::plain;
use nomnom_core::verdict::{KnownPack, PackRow, find_pack, pack_inventory};
use nomnom_pack::{Lock, Store, Tier, Trust};

use crate::pack_icon;
use crate::session::{Phase, Session};

/// A row with the icon it draws, decoded once per load rather than per frame.
type IconedRow = (PackRow, Option<Arc<Image>>);

pub struct PacksScreen {
    session: Entity<Session>,
    /// The root the inventory was loaded for, so a root change reloads it.
    loaded_for: Option<PathBuf>,
    rows: Option<Result<Vec<IconedRow>, String>>,
    url: Entity<InputState>,
    /// A trust grant waiting for the user to read what it names.
    pending_trust: Option<KnownPack>,
    status: Option<Result<String, String>>,
}

impl PacksScreen {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |this, _, cx| this.reload_if_root_changed(cx)).detach();
        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("github.com/org/repo/subdir@ref"));
        let mut this =
            Self { session, loaded_for: None, rows: None, url, pending_trust: None, status: None };
        this.reload_if_root_changed(cx);
        this
    }

    fn root(&self, cx: &App) -> Option<PathBuf> {
        self.session.read(cx).root.clone()
    }

    fn explicit(&self, cx: &App) -> Vec<PathBuf> {
        self.session.read(cx).explicit_packs.clone()
    }

    /// The CLI's repeatable `--pack DIR`. The list lives on the session, so
    /// Suggest and Clean judge with the same packs this table shows.
    fn add_pack_dir(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Load this pack directory".into()),
        });
        cx.spawn(async move |this, cx| {
            let chosen = match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => {
                    let message = format!("cannot open the folder picker: {error}");
                    eprintln!("nomnom-gui: {message}");
                    let _ = this.update(cx, |this, cx| {
                        this.status = Some(Err(message));
                        cx.notify();
                    });
                    None
                }
            };
            if let Some(dir) = chosen {
                let _ = this.update(cx, |this, cx| {
                    this.set_explicit(|dirs| dirs.push(dir), cx);
                });
            }
        })
        .detach();
    }

    fn set_explicit(&mut self, change: impl FnOnce(&mut Vec<PathBuf>), cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            change(&mut session.explicit_packs);
            session.reassess(cx);
        });
        self.pending_trust = None;
        self.reload(cx);
    }

    fn render_explicit(&self, busy: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let dirs = self.explicit(cx);
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().text_sm().font_weight(FontWeight::SEMIBOLD).child(
                        "Pack directories — loaded last, a later one overriding an earlier one",
                    ))
                    .child(
                        Button::new("add-pack-dir")
                            .small()
                            .outline()
                            .label("Add pack directory…")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.add_pack_dir(cx))),
                    ),
            )
            .when(dirs.is_empty(), |col| {
                col.child(div().text_xs().text_color(muted).child("None loaded."))
            })
            .children(dirs.into_iter().enumerate().map(|(ix, dir)| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(plain(&dir)))
                    .child(
                        Button::new(("remove-pack-dir", ix))
                            .small()
                            .ghost()
                            .label("Remove")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_explicit(|dirs| drop(dirs.remove(ix)), cx)
                            })),
                    )
            }))
    }

    fn reload_if_root_changed(&mut self, cx: &mut Context<Self>) {
        let root = self.root(cx);
        if root != self.loaded_for {
            self.loaded_for = root;
            self.pending_trust = None;
            self.status = None;
            self.reload(cx);
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.loaded_for.clone() else {
            self.rows = None;
            cx.notify();
            return;
        };
        let explicit = self.explicit(cx);
        cx.spawn(async move |this, cx| {
            let pack_root = root.clone();
            let rows = cx
                .background_executor()
                .spawn(async move { pack_inventory(&pack_root, &explicit) })
                .await
                .map(|rows| {
                    rows.into_iter()
                        .map(|row| {
                            let image = pack_icon::image(row.icon.as_ref());
                            (row, image)
                        })
                        .collect()
                })
                .map_err(|error| {
                    let message = format!("cannot list packs for {}: {error}", root.display());
                    eprintln!("nomnom-gui: {message}");
                    message
                });
            let _ = this.update(cx, |this, cx| {
                // A reply for a root the user has since left is stale.
                if this.loaded_for.as_deref() == Some(root.as_path()) {
                    this.rows = Some(rows);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Run one lock mutation in the background under the Packs phase, then
    /// reload the table and re-judge an analyzed catalog.
    fn run(
        &mut self,
        what: String,
        op: impl FnOnce(&Path) -> Result<String, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.loaded_for.clone() else { return };
        let session = self.session.clone();
        if !session.update(cx, |session, cx| session.begin(Phase::Packs, cx)) {
            return;
        }
        self.status = None;
        cx.spawn(async move |this, cx| {
            let op_root = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move { op(&op_root) })
                .await
                .map_err(|error| format!("{what} failed for {}: {error}", root.display()));
            if let Err(message) = &result {
                eprintln!("nomnom-gui: {message}");
            }
            let _ = this.update(cx, |this, cx| {
                this.status = Some(result);
                this.reload(cx);
            });
            session.update(cx, |session, cx| {
                session.end(cx);
                session.reassess(cx);
            });
        })
        .detach();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().trim().to_string();
        if url.is_empty() {
            return;
        }
        self.url.update(cx, |input, cx| input.set_value("", window, cx));
        self.run(
            format!("adding {url}"),
            move |root| {
                let store = Store::open().map_err(|e| e.to_string())?;
                let mut lock = Lock::load(root).map_err(|e| e.to_string())?;
                let added = nomnom_pack::add(&store, &mut lock, &url).map_err(|e| e.to_string())?;
                lock.save(root).map_err(|e| e.to_string())?;
                Ok(format!(
                    "Added `{}`, pinned at {}. It is untrusted until you trust it.",
                    added.name,
                    added.sha.as_deref().unwrap_or("-")
                ))
            },
            cx,
        );
    }

    fn update_pack(&mut self, name: String, cx: &mut Context<Self>) {
        self.run(
            format!("updating `{name}`"),
            move |root| {
                let store = Store::open().map_err(|e| e.to_string())?;
                let mut lock = Lock::load(root).map_err(|e| e.to_string())?;
                let before = lock.get(&name).and_then(|pack| pack.sha.clone());
                let moved =
                    nomnom_pack::update(&store, &mut lock, &name).map_err(|e| e.to_string())?;
                lock.save(root).map_err(|e| e.to_string())?;
                let now = moved.sha.as_deref().unwrap_or("-");
                Ok(match before.as_deref() {
                    Some(was) if was == now => format!("`{name}` is already at {now}."),
                    Some(was) => format!("`{name}` moved from {was} to {now}."),
                    None => format!("`{name}` pinned at {now}."),
                })
            },
            cx,
        );
    }

    fn set_trust(&mut self, name: String, trusted: bool, cx: &mut Context<Self>) {
        self.pending_trust = None;
        let what = format!("{} `{name}`", if trusted { "trusting" } else { "untrusting" });
        self.run(
            what,
            move |root| {
                let mut lock = Lock::load(root).map_err(|e| e.to_string())?;
                lock.set_trust(&name, trusted);
                lock.save(root).map_err(|e| e.to_string())?;
                Ok(if trusted {
                    format!("Trusted `{name}`; its rules may now say `reclaimable`.")
                } else {
                    format!("Untrusted `{name}`; its rules cap at `review` again.")
                })
            },
            cx,
        );
    }

    fn remove(&mut self, name: String, cx: &mut Context<Self>) {
        self.run(
            format!("removing `{name}`"),
            move |root| {
                let mut lock = Lock::load(root).map_err(|e| e.to_string())?;
                if !lock.remove(&name) {
                    return Err(nomnom_pack::Error::NotLocked { name }.to_string());
                }
                lock.save(root).map_err(|e| e.to_string())?;
                Ok(format!("Removed `{name}`; its cached checkout is left in place."))
            },
            cx,
        );
    }

    /// Trust is granted only after the pack is named in full, as the CLI
    /// does: resolving it may fetch, so it runs in the background and the
    /// grant waits in `pending_trust` for a second click.
    fn ask_trust(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(root) = self.loaded_for.clone() else { return };
        let explicit = self.explicit(cx);
        let session = self.session.clone();
        if !session.update(cx, |session, cx| session.begin(Phase::Packs, cx)) {
            return;
        }
        self.status = None;
        cx.spawn(async move |this, cx| {
            let lookup_root = root.clone();
            let lookup_name = name.clone();
            let found = cx
                .background_executor()
                .spawn(async move {
                    let lock = Lock::load(&lookup_root).map_err(|e| e.to_string())?;
                    find_pack(&lookup_root, &lookup_name, &explicit, &lock)
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match found {
                    Ok(known) => this.pending_trust = Some(known),
                    Err(error) => {
                        let message = format!("cannot look up pack `{name}`: {error}");
                        eprintln!("nomnom-gui: {message}");
                        this.status = Some(Err(message));
                    }
                }
                cx.notify();
            });
            session.update(cx, |session, cx| session.end(cx));
        })
        .detach();
    }

    fn render_pending_trust(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let known = self.pending_trust.clone()?;
        let name = known.name.clone();
        Some(
            v_flex()
                .gap_1()
                .p_3()
                .border_1()
                .border_color(cx.theme().warning)
                .rounded_md()
                .text_sm()
                .child(
                    div().font_weight(FontWeight::SEMIBOLD).child(format!("Trust pack `{name}`?")),
                )
                .child(format!("url:    {}", known.url.as_deref().unwrap_or("(a local directory)")))
                .child(format!("pinned: {}", known.sha.as_deref().unwrap_or("-")))
                .child(format!("from:   {}", known.dir.display()))
                .child(div().text_color(cx.theme().muted_foreground).child(
                    "Its rules may then say `reclaimable`, which makes the paths they match \
                         deletion candidates.",
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("grant-trust").primary().small().label("Trust").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.set_trust(name.clone(), true, cx)
                                }),
                            ),
                        )
                        .child(
                            Button::new("cancel-trust").ghost().small().label("Cancel").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.pending_trust = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }
}

fn tier_name(tier: Option<Tier>) -> &'static str {
    match tier {
        None => "built-in",
        Some(Tier::User) => "user",
        Some(Tier::Project) => "project",
        Some(Tier::Explicit) => "explicit",
    }
}

fn trust_tag(trust: Trust) -> Tag {
    match trust {
        Trust::Builtin => Tag::secondary().small().child("built-in"),
        Trust::Trusted => Tag::success().small().child("trusted"),
        Trust::Untrusted => Tag::warning().small().child("untrusted"),
    }
}

impl Render for PacksScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(root) = self.loaded_for.clone() else {
            return v_flex()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("Scan a drive first: its packs are pinned in its .nomnom\\packs.lock.")
                .into_any_element();
        };
        let busy = self.session.read(cx).busy;
        let working = busy == Some(Phase::Packs);

        let add_row = h_flex()
            .gap_2()
            .child(div().flex_1().child(Input::new(&self.url)))
            .child(
                Button::new("add-pack")
                    .primary()
                    .label("Add")
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
            )
            .when(working, |row| row.child(Spinner::new().small()).child(Phase::Packs.label()));

        let table = match &self.rows {
            None => div().child(Spinner::new()).into_any_element(),
            Some(Err(message)) => {
                Alert::error("pack-list-error", message.clone()).into_any_element()
            }
            Some(Ok(rows)) => {
                let body = rows.iter().enumerate().map(|(ix, (row, image))| {
                    let name = row.name.clone();
                    let locked = row.url.is_some();
                    let actions = h_flex()
                        .gap_1()
                        .when(locked, |cell| {
                            let name = name.clone();
                            cell.child(
                                Button::new(("update", ix))
                                    .small()
                                    .outline()
                                    .label("Update")
                                    .disabled(busy.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.update_pack(name.clone(), cx)
                                    })),
                            )
                        })
                        .when(row.trust == Trust::Untrusted, |cell| {
                            let name = name.clone();
                            cell.child(
                                Button::new(("trust", ix))
                                    .small()
                                    .outline()
                                    .label("Trust…")
                                    .disabled(busy.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ask_trust(name.clone(), cx)
                                    })),
                            )
                        })
                        .when(row.trust == Trust::Trusted, |cell| {
                            let name = name.clone();
                            cell.child(
                                Button::new(("untrust", ix))
                                    .small()
                                    .outline()
                                    .label("Untrust")
                                    .disabled(busy.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_trust(name.clone(), false, cx)
                                    })),
                            )
                        })
                        .when(locked, |cell| {
                            let name = name.clone();
                            cell.child(
                                Button::new(("remove", ix))
                                    .small()
                                    .danger()
                                    .label("Remove")
                                    .disabled(busy.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove(name.clone(), cx)
                                    })),
                            )
                        });
                    TableRow::new()
                        .child(
                            TableCell::new().child(
                                h_flex()
                                    .gap_2()
                                    .child(pack_icon::tile(image.clone(), cx))
                                    .child(row.name.clone()),
                            ),
                        )
                        .child(TableCell::new().child(tier_name(row.tier)))
                        .child(TableCell::new().child(trust_tag(row.trust)))
                        .child(
                            TableCell::new().child(row.url.clone().unwrap_or_else(|| "-".into())),
                        )
                        .child(
                            TableCell::new().child(
                                row.sha
                                    .as_deref()
                                    .map_or("-".to_string(), |sha| sha.chars().take(12).collect()),
                            ),
                        )
                        .child(TableCell::new().child(actions))
                });
                // The CLI's `pack list` prints the same lines under its table.
                let broken: Vec<String> = rows
                    .iter()
                    .filter_map(|(row, _)| {
                        let why = row.icon.as_ref()?.svg.as_ref().err()?;
                        Some(format!("Pack `{}` shows no icon: {why}", row.name))
                    })
                    .collect();
                let table = Table::new()
                    .child(
                        TableHeader::new().child(
                            TableRow::new()
                                .child(TableHead::new().child("Name"))
                                .child(TableHead::new().child("Tier"))
                                .child(TableHead::new().child("Trust"))
                                .child(TableHead::new().child("URL"))
                                .child(TableHead::new().child("SHA"))
                                .child(TableHead::new().child("")),
                        ),
                    )
                    .child(TableBody::new().children(body));
                v_flex()
                    .gap_2()
                    .when(!broken.is_empty(), |col| {
                        col.child(Alert::warning("pack-icon-warning", broken.join("\n")))
                    })
                    .child(table)
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .gap_3()
            .p_4()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Lock: {}", Lock::path_in(&root).display())),
            )
            .child(add_row)
            .child(self.render_explicit(busy.is_some(), cx))
            .children(self.render_pending_trust(cx))
            .children(self.status.as_ref().map(|status| match status {
                Ok(message) => Alert::success("pack-status", message.clone()).into_any_element(),
                Err(message) => Alert::error("pack-status", message.clone()).into_any_element(),
            }))
            .child(table)
            .into_any_element()
    }
}
