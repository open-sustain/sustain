// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    num::NonZeroU32,
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gdk, glib};

use sustain_app_runtime::{
    ApplicationCommand, SmartPlaylistId, SmartPlaylistLimit, SmartPlaylistLimitSelection,
    SmartPlaylistMatchKind, SmartPlaylistRule, SmartPlaylistRuleSet,
};

use super::{
    SMART_PLAYLIST_EDITOR_HEIGHT, SMART_PLAYLIST_EDITOR_WIDTH, WINDOW_SHADOW_MARGIN,
    command_controller::SharedCommandController, sidebar_context::unique_default_name,
};

mod model;

use model::{
    EDITOR_FIELDS, EditorField, EditorOperator, LIMIT_SELECTIONS, MATCH_KINDS, RuleError,
    ValueInput, ValueKind, automatic_name_for_single_text_rule, decompose_rule,
    effective_value_kind, extract_rule, index_of_field, index_of_limit_selection,
    index_of_match_kind, index_of_operator, operators_for_field,
};

#[derive(Clone)]
enum ValueWidget {
    Text(gtk::Entry),
    Number(gtk::SpinButton),
    Rating(gtk::SpinButton),
    Date(gtk::Entry),
    Days(gtk::SpinButton),
    Bool(gtk::DropDown),
    None,
}

impl ValueWidget {
    fn root(&self) -> Option<gtk::Widget> {
        match self {
            Self::Text(entry) => Some(entry.clone().upcast()),
            Self::Number(spin) => Some(spin.clone().upcast()),
            Self::Rating(spin) => Some(spin.clone().upcast()),
            Self::Date(entry) => Some(entry.clone().upcast()),
            Self::Days(spin) => Some(spin.clone().upcast()),
            Self::Bool(dropdown) => Some(dropdown.clone().upcast()),
            Self::None => None,
        }
    }

    fn build(kind: ValueKind) -> Self {
        match kind {
            ValueKind::Text => {
                let entry = gtk::Entry::new();
                entry.set_hexpand(true);
                entry.set_placeholder_text(Some("value"));
                Self::Text(entry)
            }
            ValueKind::Number => {
                let spin = gtk::SpinButton::with_range(0.0, 9_999_999.0, 1.0);
                spin.set_value(0.0);
                spin.set_digits(0);
                Self::Number(spin)
            }
            ValueKind::Rating => {
                let spin = gtk::SpinButton::with_range(0.0, 5.0, 1.0);
                spin.set_value(0.0);
                spin.set_digits(0);
                Self::Rating(spin)
            }
            ValueKind::Date => {
                let entry = gtk::Entry::new();
                entry.set_placeholder_text(Some("YYYY-MM-DD"));
                entry.set_max_length(10);
                entry.set_width_chars(11);
                Self::Date(entry)
            }
            ValueKind::Days => {
                let spin = gtk::SpinButton::with_range(1.0, 9_999.0, 1.0);
                spin.set_value(7.0);
                spin.set_digits(0);
                Self::Days(spin)
            }
            ValueKind::Bool => Self::Bool(gtk::DropDown::from_strings(&["Yes", "No"])),
            ValueKind::None => Self::None,
        }
    }
}

#[derive(Clone)]
struct RuleRow {
    container: gtk::Box,
    current_field: Rc<Cell<EditorField>>,
    current_operator: Rc<Cell<EditorOperator>>,
    current_value: Rc<RefCell<ValueWidget>>,
}

impl RuleRow {
    fn from_initial(initial: Option<&SmartPlaylistRule>) -> Self {
        let (initial_field, initial_operator, initial_value) = match initial {
            Some(rule) => decompose_rule(rule),
            None => (
                EDITOR_FIELDS[0].0,
                operators_for_field(EDITOR_FIELDS[0].0)[0],
                ValueInput::None,
            ),
        };

        let container = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        container.add_css_class("smart-playlist-rule-row");

        let field_model = gtk::StringList::new(
            &EDITOR_FIELDS
                .iter()
                .map(|(_, label)| *label)
                .collect::<Vec<_>>(),
        );
        let field_combo = gtk::DropDown::new(Some(field_model), gtk::Expression::NONE);
        field_combo.set_selected(index_of_field(initial_field));

        let operator_combo = gtk::DropDown::new(
            Some(operator_model_for_field(initial_field)),
            gtk::Expression::NONE,
        );
        operator_combo.set_selected(index_of_operator(initial_field, initial_operator));

        let value_container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        value_container.set_hexpand(true);

        let days_suffix = gtk::Label::new(Some("days"));
        days_suffix.set_visible(false);

        let current_field = Rc::new(Cell::new(initial_field));
        let current_operator = Rc::new(Cell::new(initial_operator));
        let current_value = Rc::new(RefCell::new(ValueWidget::None));

        install_value_widget(
            &value_container,
            &days_suffix,
            &current_value,
            initial_field,
            initial_operator,
        );
        apply_initial_value(&current_value.borrow(), &initial_value);

        container.append(&field_combo);
        container.append(&operator_combo);
        container.append(&value_container);
        container.append(&days_suffix);

        let operator_combo_for_field = operator_combo.clone();
        let current_field_for_field = current_field.clone();
        let current_operator_for_field = current_operator.clone();
        let current_value_for_field = current_value.clone();
        let value_container_for_field = value_container.clone();
        let days_suffix_for_field = days_suffix.clone();
        field_combo.connect_selected_notify(move |dropdown| {
            let index = dropdown.selected() as usize;
            let Some((field, _)) = EDITOR_FIELDS.get(index).copied() else {
                return;
            };
            current_field_for_field.set(field);
            operator_combo_for_field.set_model(Some(&operator_model_for_field(field)));
            operator_combo_for_field.set_selected(0);
            let operator = operators_for_field(field)[0];
            current_operator_for_field.set(operator);
            install_value_widget(
                &value_container_for_field,
                &days_suffix_for_field,
                &current_value_for_field,
                field,
                operator,
            );
        });

        let current_field_for_op = current_field.clone();
        let current_operator_for_op = current_operator.clone();
        let current_value_for_op = current_value.clone();
        let value_container_for_op = value_container.clone();
        let days_suffix_for_op = days_suffix.clone();
        operator_combo.connect_selected_notify(move |dropdown| {
            let field = current_field_for_op.get();
            let operators = operators_for_field(field);
            let index = dropdown.selected() as usize;
            let Some(operator) = operators.get(index).copied() else {
                return;
            };
            current_operator_for_op.set(operator);
            install_value_widget(
                &value_container_for_op,
                &days_suffix_for_op,
                &current_value_for_op,
                field,
                operator,
            );
        });

        Self {
            container,
            current_field,
            current_operator,
            current_value,
        }
    }

    fn extract(&self) -> Result<SmartPlaylistRule, RuleError> {
        let input = value_input_from_widget(&self.current_value.borrow());
        extract_rule(
            self.current_field.get(),
            self.current_operator.get(),
            &input,
        )
    }
}

fn value_input_from_widget(value: &ValueWidget) -> ValueInput {
    match value {
        ValueWidget::Text(entry) => ValueInput::Text(entry.text().to_string()),
        ValueWidget::Number(spin) => ValueInput::Number(spin.value_as_int() as i64),
        ValueWidget::Rating(spin) => ValueInput::Rating(spin.value_as_int().max(0) as u32),
        ValueWidget::Date(entry) => ValueInput::Date(entry.text().to_string()),
        ValueWidget::Days(spin) => ValueInput::Days(spin.value_as_int().max(0) as u32),
        ValueWidget::Bool(dropdown) => ValueInput::Bool(dropdown.selected() == 0),
        ValueWidget::None => ValueInput::None,
    }
}

fn install_value_widget(
    container: &gtk::Box,
    days_suffix: &gtk::Label,
    current_value: &Rc<RefCell<ValueWidget>>,
    field: EditorField,
    operator: EditorOperator,
) {
    let kind = effective_value_kind(field, operator);
    let new_widget = ValueWidget::build(kind);

    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    if let Some(root) = new_widget.root() {
        container.append(&root);
    }
    days_suffix.set_visible(matches!(new_widget, ValueWidget::Days(_)));
    current_value.replace(new_widget);
}

fn operator_model_for_field(field: EditorField) -> gtk::StringList {
    let labels: Vec<&str> = operators_for_field(field)
        .iter()
        .map(|op| op.label())
        .collect();
    gtk::StringList::new(&labels)
}

fn apply_initial_value(widget: &ValueWidget, input: &ValueInput) {
    match (widget, input) {
        (ValueWidget::Text(entry), ValueInput::Text(text)) => entry.set_text(text),
        (ValueWidget::Number(spin), ValueInput::Number(number)) => spin.set_value(*number as f64),
        (ValueWidget::Rating(spin), ValueInput::Rating(stars)) => spin.set_value(*stars as f64),
        (ValueWidget::Date(entry), ValueInput::Date(text)) => entry.set_text(text),
        (ValueWidget::Days(spin), ValueInput::Days(days)) => spin.set_value(*days as f64),
        (ValueWidget::Bool(dropdown), ValueInput::Bool(value)) => {
            dropdown.set_selected(u32::from(!value));
        }
        _ => {}
    }
}

pub(crate) enum SmartPlaylistEditorMode {
    Create {
        name: String,
    },
    Edit {
        smart_playlist_id: SmartPlaylistId,
        name: String,
        rules: SmartPlaylistRuleSet,
    },
}

pub(crate) fn open_smart_playlist_editor(
    parent: &gtk::ApplicationWindow,
    command_controller: SharedCommandController,
    on_saved: Rc<dyn Fn()>,
    mode: SmartPlaylistEditorMode,
) {
    let window = gtk::Window::builder()
        .title("Smart Playlist")
        .decorated(false)
        .transient_for(parent)
        .modal(true)
        .default_width(SMART_PLAYLIST_EDITOR_WIDTH + WINDOW_SHADOW_MARGIN * 2)
        .default_height(SMART_PLAYLIST_EDITOR_HEIGHT + WINDOW_SHADOW_MARGIN * 2)
        .resizable(false)
        .build();
    window.add_css_class("app-window");

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("preferences-frame");
    frame.set_hexpand(true);
    frame.set_vexpand(true);
    frame.set_margin_top(WINDOW_SHADOW_MARGIN);
    frame.set_margin_end(WINDOW_SHADOW_MARGIN);
    frame.set_margin_bottom(WINDOW_SHADOW_MARGIN);
    frame.set_margin_start(WINDOW_SHADOW_MARGIN);
    frame.set_size_request(SMART_PLAYLIST_EDITOR_WIDTH, SMART_PLAYLIST_EDITOR_HEIGHT);

    let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    panel.add_css_class("preferences-panel");
    panel.set_hexpand(true);
    panel.set_vexpand(true);
    panel.set_overflow(gtk::Overflow::Hidden);

    let close_row = close_row_for(&window);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(16);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let initial_rules: Option<&SmartPlaylistRuleSet> = match &mode {
        SmartPlaylistEditorMode::Create { .. } => None,
        SmartPlaylistEditorMode::Edit { rules, .. } => Some(rules),
    };

    let match_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let match_prefix = gtk::Label::new(Some("Match"));
    let match_combo = gtk::DropDown::new(
        Some(gtk::StringList::new(
            &MATCH_KINDS
                .iter()
                .map(|(_, label)| *label)
                .collect::<Vec<_>>(),
        )),
        gtk::Expression::NONE,
    );
    match_combo.set_selected(
        initial_rules
            .map(|rules| index_of_match_kind(rules.match_kind))
            .unwrap_or(0),
    );
    let match_suffix = gtk::Label::new(Some("of the following rules:"));
    match_row.append(&match_prefix);
    match_row.append(&match_combo);
    match_row.append(&match_suffix);
    content.append(&match_row);

    let rules_container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    rules_container.add_css_class("smart-playlist-rules");
    content.append(&rules_container);

    let rule_rows: Rc<RefCell<Vec<RuleRow>>> = Rc::new(RefCell::new(Vec::new()));
    match initial_rules {
        Some(rule_set) if !rule_set.rules.is_empty() => {
            for rule in &rule_set.rules {
                append_rule_row_with(&rules_container, &rule_rows, Some(rule));
            }
        }
        _ => append_rule_row(&rules_container, &rule_rows),
    }

    let limit_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    limit_row.add_css_class("smart-playlist-limit-row");
    let limit_check = gtk::CheckButton::with_label("Limit to");
    let limit_count = gtk::SpinButton::with_range(1.0, 10_000.0, 1.0);
    limit_count.set_value(25.0);
    limit_count.set_digits(0);
    limit_count.set_sensitive(false);
    let limit_unit = gtk::Label::new(Some("songs, selected by"));
    let limit_selection_combo = gtk::DropDown::new(
        Some(gtk::StringList::new(
            &LIMIT_SELECTIONS
                .iter()
                .map(|(_, label)| *label)
                .collect::<Vec<_>>(),
        )),
        gtk::Expression::NONE,
    );
    limit_selection_combo.set_selected(0);
    limit_selection_combo.set_sensitive(false);

    if let Some(SmartPlaylistLimit { count, selection }) =
        initial_rules.and_then(|rules| rules.limit)
    {
        limit_check.set_active(true);
        limit_count.set_value(f64::from(count.get()));
        limit_selection_combo.set_selected(index_of_limit_selection(selection));
        limit_count.set_sensitive(true);
        limit_selection_combo.set_sensitive(true);
    }

    let limit_count_for_toggle = limit_count.clone();
    let limit_selection_for_toggle = limit_selection_combo.clone();
    limit_check.connect_toggled(move |check| {
        let active = check.is_active();
        limit_count_for_toggle.set_sensitive(active);
        limit_selection_for_toggle.set_sensitive(active);
    });

    limit_row.append(&limit_check);
    limit_row.append(&limit_count);
    limit_row.append(&limit_unit);
    limit_row.append(&limit_selection_combo);
    content.append(&limit_row);

    let error_label = gtk::Label::new(None);
    error_label.add_css_class("smart-playlist-error");
    error_label.set_xalign(0.0);
    error_label.set_wrap(true);
    error_label.set_visible(false);
    content.append(&error_label);

    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    content.append(&spacer);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);

    let cancel_button = gtk::Button::with_label("Cancel");
    let ok_button = gtk::Button::with_label("OK");
    ok_button.add_css_class("suggested-action");

    let window_for_cancel = window.clone();
    cancel_button.connect_clicked(move |_| {
        window_for_cancel.close();
    });

    let window_for_ok = window.clone();
    let command_controller_for_ok = command_controller.clone();
    let rule_rows_for_ok = rule_rows.clone();
    let match_combo_for_ok = match_combo.clone();
    let limit_check_for_ok = limit_check.clone();
    let limit_count_for_ok = limit_count.clone();
    let limit_selection_for_ok = limit_selection_combo.clone();
    let on_saved_for_ok = on_saved.clone();
    let error_label_for_ok = error_label.clone();
    ok_button.connect_clicked(move |_| {
        let extraction = extract_rule_set(
            &rule_rows_for_ok.borrow(),
            &match_combo_for_ok,
            &limit_check_for_ok,
            &limit_count_for_ok,
            &limit_selection_for_ok,
        );
        match extraction {
            Ok(rule_set) => {
                let command = match &mode {
                    SmartPlaylistEditorMode::Create { name } => {
                        let name = smart_playlist_name_for_create(
                            &command_controller_for_ok,
                            name,
                            &rule_set,
                        );
                        ApplicationCommand::CreateSmartPlaylist {
                            name,
                            parent_folder_id: None,
                            rules: rule_set,
                        }
                    }
                    SmartPlaylistEditorMode::Edit {
                        smart_playlist_id,
                        name,
                        ..
                    } => ApplicationCommand::UpdateSmartPlaylist {
                        smart_playlist_id: *smart_playlist_id,
                        name: name.clone(),
                        rules: rule_set,
                    },
                };
                let dispatched = command_controller_for_ok.dispatch_succeeded(command);
                if dispatched {
                    on_saved_for_ok();
                    window_for_ok.close();
                }
            }
            Err(error) => {
                error_label_for_ok.set_text(&error.message());
                error_label_for_ok.set_visible(true);
            }
        }
    });

    buttons.append(&cancel_button);
    buttons.append(&ok_button);
    content.append(&buttons);

    panel.append(&close_row);
    panel.append(&content);
    frame.append(&panel);
    window.set_child(Some(&frame));

    let key_controller = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    window.present();
    ok_button.grab_focus();
}

fn smart_playlist_name_for_create(
    command_controller: &SharedCommandController,
    fallback_name: &str,
    rule_set: &SmartPlaylistRuleSet,
) -> String {
    let preferred = automatic_name_for_single_text_rule(rule_set)
        .unwrap_or_else(|| fallback_name.trim().to_owned());
    let existing_names: Vec<String> = command_controller
        .runtime()
        .borrow()
        .smart_playlists()
        .iter()
        .map(|smart| smart.name.clone())
        .collect();
    unique_default_name(existing_names, &preferred)
}

fn append_rule_row(rules_container: &gtk::Box, rule_rows: &Rc<RefCell<Vec<RuleRow>>>) {
    append_rule_row_with(rules_container, rule_rows, None);
}

fn append_rule_row_with(
    rules_container: &gtk::Box,
    rule_rows: &Rc<RefCell<Vec<RuleRow>>>,
    initial: Option<&SmartPlaylistRule>,
) {
    let row = RuleRow::from_initial(initial);
    let row_widget = row.container.clone();
    let row_buttons = rule_row_buttons();
    row_widget.append(&row_buttons.container);

    rules_container.append(&row_widget);
    rule_rows.borrow_mut().push(row);

    connect_row_buttons(
        row_buttons,
        row_widget,
        rule_rows.clone(),
        rules_container.clone(),
    );
}

fn close_row_for(window: &gtk::Window) -> gtk::Box {
    let close_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    close_row.set_margin_top(8);
    close_row.set_margin_end(8);
    close_row.set_margin_start(8);

    let close_icon = gtk::Image::from_icon_name("window-close-symbolic");
    close_icon.set_pixel_size(14);

    let close_button = gtk::Button::new();
    close_button.add_css_class("flat");
    close_button.add_css_class("preference-close-button");
    close_button.set_child(Some(&close_icon));
    close_button.set_tooltip_text(Some("Close"));
    close_button.set_halign(gtk::Align::End);
    close_button.set_valign(gtk::Align::Center);
    close_button.set_hexpand(true);

    let window_for_close = window.clone();
    close_button.connect_clicked(move |_| {
        window_for_close.close();
    });
    close_row.append(&close_button);
    close_row
}

#[derive(Clone)]
struct RuleRowButtons {
    container: gtk::Box,
    remove_button: gtk::Button,
    add_button: gtk::Button,
}

fn rule_row_buttons() -> RuleRowButtons {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);

    let remove_button = gtk::Button::from_icon_name("list-remove-symbolic");
    remove_button.add_css_class("flat");
    remove_button.set_tooltip_text(Some("Remove rule"));

    let add_button = gtk::Button::from_icon_name("list-add-symbolic");
    add_button.add_css_class("flat");
    add_button.set_tooltip_text(Some("Add rule"));

    container.append(&remove_button);
    container.append(&add_button);

    RuleRowButtons {
        container,
        remove_button,
        add_button,
    }
}

fn connect_row_buttons(
    buttons: RuleRowButtons,
    row_widget: gtk::Box,
    rule_rows: Rc<RefCell<Vec<RuleRow>>>,
    rules_container: gtk::Box,
) {
    let rule_rows_for_remove = rule_rows.clone();
    let rules_container_for_remove = rules_container.clone();
    let row_widget_for_remove = row_widget.clone();
    buttons.remove_button.connect_clicked(move |_| {
        let mut rows = rule_rows_for_remove.borrow_mut();
        if rows.len() <= 1 {
            return;
        }
        if let Some(index) = rows
            .iter()
            .position(|row| row.container == row_widget_for_remove)
        {
            rows.remove(index);
            rules_container_for_remove.remove(&row_widget_for_remove);
        }
    });

    let rule_rows_for_add = rule_rows.clone();
    let rules_container_for_add = rules_container.clone();
    buttons.add_button.connect_clicked(move |_| {
        append_rule_row(&rules_container_for_add, &rule_rows_for_add);
    });
}

fn extract_rule_set(
    rule_rows: &[RuleRow],
    match_combo: &gtk::DropDown,
    limit_check: &gtk::CheckButton,
    limit_count: &gtk::SpinButton,
    limit_selection_combo: &gtk::DropDown,
) -> Result<SmartPlaylistRuleSet, RuleError> {
    let rules: Vec<_> = rule_rows
        .iter()
        .map(RuleRow::extract)
        .collect::<Result<_, _>>()?;

    let limit = if limit_check.is_active() {
        let count_value = limit_count.value_as_int().max(1) as u32;
        let count = NonZeroU32::new(count_value).expect("count clamped to >= 1");
        Some(SmartPlaylistLimit {
            count,
            selection: limit_selection_from_combo(limit_selection_combo),
        })
    } else {
        None
    };

    Ok(SmartPlaylistRuleSet {
        match_kind: match_kind_from_combo(match_combo),
        rules,
        limit,
    })
}

fn match_kind_from_combo(combo: &gtk::DropDown) -> SmartPlaylistMatchKind {
    let index = combo.selected() as usize;
    MATCH_KINDS
        .get(index)
        .map(|(kind, _)| *kind)
        .unwrap_or(SmartPlaylistMatchKind::All)
}

fn limit_selection_from_combo(combo: &gtk::DropDown) -> SmartPlaylistLimitSelection {
    let index = combo.selected() as usize;
    LIMIT_SELECTIONS
        .get(index)
        .map(|(selection, _)| *selection)
        .unwrap_or(SmartPlaylistLimitSelection::Random)
}
