//! The Compare dialog: build a project of saved runs, pick a baseline, and make the
//! report.
//!
//! The project is a small JSON file in `FW Benchmark/projects` - the same format the
//! Windows build writes - and the report an HTML page beside it, opened in the browser.
//! All the arithmetic is in `fw_helper_client::compare`; this is only the editor.

use crate::monitor::{open_file, utc_offset};
use adw::prelude::*;
use fw_helper_client::bench;
use fw_helper_client::compare::{self, CompareProject, CompareRun, MAX_RUNS};
use gtk::glib;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

struct Editor {
    project: RefCell<CompareProject>,
    runs_group: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
    name_row: adw::EntryRow,
    add_list: gtk::ListBox,
    add_targets: RefCell<Vec<PathBuf>>,
    status: gtk::Label,
    make: gtk::Button,
}

/// Show the dialog over `parent`. `first`, if given, starts a new project with it.
pub fn present(parent: &gtk::Widget, first: Option<PathBuf>) {
    let name_row = adw::EntryRow::builder().title("Project name").build();
    let open_model = gtk::StringList::new(&[]);
    let projects = compare::list_projects();
    open_model.append("New project");
    for (name, _) in &projects {
        open_model.append(name);
    }
    let open_row = adw::ComboRow::builder()
        .title("Project")
        .model(&open_model)
        .build();
    let project_group = adw::PreferencesGroup::new();
    project_group.add(&open_row);
    project_group.add(&name_row);

    let add_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let add_popover = gtk::Popover::builder()
        .child(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(400)
                .min_content_width(340)
                .child(&add_list)
                .build(),
        )
        .build();
    let add_button = gtk::MenuButton::builder()
        .label("Add Run")
        .popover(&add_popover)
        .css_classes(["flat"])
        .build();
    let runs_group = adw::PreferencesGroup::builder()
        .title("Runs")
        .description(
            "Each run is compared against the baseline. Name runs by what differs - \
             \u{201c}25 W\u{201d}, \u{201c}quiet fan\u{201d} - since that is what the charts label them with.",
        )
        .header_suffix(&add_button)
        .build();

    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["stat-label"])
        .build();
    let make = gtk::Button::builder()
        .label("Save and Open Report")
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    content.append(&project_group);
    content.append(&runs_group);
    content.append(&status);
    content.append(&make);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&content)
            .build(),
    ));
    let dialog = adw::Dialog::builder()
        .title("Compare Runs")
        .content_width(560)
        .content_height(640)
        .child(&toolbar)
        .build();

    let ed = Rc::new(Editor {
        project: RefCell::new(CompareProject::default()),
        runs_group,
        rows: RefCell::new(Vec::new()),
        name_row,
        add_list,
        add_targets: RefCell::new(Vec::new()),
        status,
        make,
    });

    if let Some(csv) = first {
        let mut p = ed.project.borrow_mut();
        p.runs.push(run_for(&csv));
        p.name = p.runs[0].name.clone();
    }
    ed.name_row.set_text(&ed.project.borrow().name.clone());
    ed.rebuild();

    let e = Rc::clone(&ed);
    ed.name_row.connect_changed(move |row| {
        e.project.borrow_mut().name = row.text().to_string();
        e.sync();
    });

    let e = Rc::clone(&ed);
    open_row.connect_selected_notify(move |row| {
        let i = row.selected() as usize;
        let loaded = match i {
            0 => Some(CompareProject::default()),
            _ => projects.get(i - 1).and_then(|(_, path)| {
                std::fs::read_to_string(path)
                    .ok()
                    .and_then(|t| CompareProject::from_json(&t).ok())
            }),
        };
        if let Some(p) = loaded {
            let name = p.name.clone();
            *e.project.borrow_mut() = p;
            e.name_row.set_text(&name);
            e.rebuild();
        }
    });

    let e = Rc::clone(&ed);
    add_popover.connect_show(move |_| e.rebuild_add_list());
    let e = Rc::clone(&ed);
    let pop = add_popover.clone();
    ed.add_list.connect_row_activated(move |_, row| {
        let target = e.add_targets.borrow().get(row.index() as usize).cloned();
        pop.popdown();
        if let Some(csv) = target {
            let run = run_for(&csv);
            let mut p = e.project.borrow_mut();
            if p.name.trim().is_empty() {
                p.name = run.name.clone();
                drop(p);
                e.name_row.set_text(&run.name);
                p = e.project.borrow_mut();
            }
            p.runs.push(run);
            drop(p);
            e.rebuild();
        }
    });

    let e = Rc::clone(&ed);
    let d = dialog.clone();
    ed.make.connect_clicked(move |_| {
        let p = e.project.borrow().clone();
        match compare::save_and_report(&p, &utc_offset) {
            Ok(html) => {
                open_file(&html, d.root().and_downcast::<gtk::Window>().as_ref());
                e.status
                    .set_label(&format!("Saved. Report: {}", html.display()));
            }
            Err(err) => e
                .status
                .set_label(&format!("Could not save the project: {err}")),
        }
    });

    dialog.present(Some(parent));
}

/// A run for a saved recording, named after its label.
fn run_for(csv: &std::path::Path) -> CompareRun {
    let label = bench::saved_sessions()
        .into_iter()
        .find(|m| m.path == csv)
        .map(|m| m.label.replace('-', " "))
        .unwrap_or_default();
    CompareRun {
        file: CompareProject::store(&bench::bench_dir(), csv),
        name: label,
    }
}

impl Editor {
    /// Rebuild the run rows from the project.
    fn rebuild(self: &Rc<Self>) {
        for row in self.rows.borrow_mut().drain(..) {
            self.runs_group.remove(&row);
        }
        let runs = self.project.borrow().runs.clone();
        let baseline = self.project.borrow().baseline_index();
        let mut group_leader: Option<gtk::CheckButton> = None;
        for (i, run) in runs.iter().enumerate() {
            let row = adw::EntryRow::builder()
                .title(glib::markup_escape_text(&run.file).as_str())
                .text(&run.name)
                .build();
            let check = gtk::CheckButton::builder()
                .tooltip_text("Baseline: the run the others are compared against")
                .valign(gtk::Align::Center)
                .active(i == baseline)
                .build();
            if let Some(leader) = &group_leader {
                check.set_group(Some(leader));
            } else {
                group_leader = Some(check.clone());
            }
            row.add_prefix(&check);
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove this run from the project")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            row.add_suffix(&remove);

            let e = Rc::clone(self);
            row.connect_changed(move |r| {
                if let Some(run) = e.project.borrow_mut().runs.get_mut(i) {
                    run.name = r.text().to_string();
                }
            });
            let e = Rc::clone(self);
            check.connect_toggled(move |c| {
                if c.is_active() {
                    e.project.borrow_mut().baseline = i;
                }
            });
            let e = Rc::clone(self);
            remove.connect_clicked(move |_| {
                {
                    let mut p = e.project.borrow_mut();
                    if i < p.runs.len() {
                        p.runs.remove(i);
                    }
                    if p.baseline >= i && p.baseline > 0 {
                        p.baseline -= 1;
                    }
                }
                e.rebuild();
            });

            self.runs_group.add(&row);
            self.rows.borrow_mut().push(row.upcast());
        }
        if runs.is_empty() {
            let row = adw::ActionRow::builder()
                .title("No runs yet")
                .subtitle("Add Run lists the recordings saved in FW Benchmark")
                .build();
            self.runs_group.add(&row);
            self.rows.borrow_mut().push(row.upcast());
        }
        self.sync();
    }

    fn sync(&self) {
        let p = self.project.borrow();
        let ready = p.runs.len() >= 2 && !p.name.trim().is_empty();
        self.make.set_sensitive(ready);
        if !ready {
            self.status.set_label(if p.runs.len() < 2 {
                "Add at least two runs."
            } else {
                "Give the project a name."
            });
        } else {
            self.status.set_label("");
        }
    }

    fn rebuild_add_list(&self) {
        while let Some(c) = self.add_list.first_child() {
            self.add_list.remove(&c);
        }
        let dir = bench::bench_dir();
        let have: Vec<String> = self
            .project
            .borrow()
            .runs
            .iter()
            .map(|r| r.file.clone())
            .collect();
        let full = self.project.borrow().runs.len() >= MAX_RUNS;
        let mut targets = Vec::new();
        for m in bench::saved_sessions() {
            if have.contains(&CompareProject::store(&dir, &m.path)) {
                continue;
            }
            let when = glib::DateTime::from_unix_local(m.started_unix as i64)
                .and_then(|d| d.format("%a %-d %b, %H:%M"))
                .map(|s| s.to_string())
                .unwrap_or_default();
            let settings = bench::load_notes(&m.path)
                .and_then(|n| n.game_settings)
                .map(|g| format!(" \u{b7} {g}"))
                .unwrap_or_default();
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&m.label).as_str())
                .subtitle(glib::markup_escape_text(&format!("{when}{settings}")).as_str())
                .activatable(!full)
                .build();
            self.add_list.append(&row);
            targets.push(m.path);
        }
        if targets.is_empty() {
            self.add_list.append(
                &adw::ActionRow::builder()
                    .title("No more saved runs")
                    .subtitle("Record a run and stop it to save one")
                    .build(),
            );
        }
        *self.add_targets.borrow_mut() = targets;
    }
}
