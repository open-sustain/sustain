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
use sustain_i18n::gettext;

use super::{
    SMART_PLAYLIST_EDITOR_HEIGHT, SMART_PLAYLIST_EDITOR_WIDTH, WINDOW_SHADOW_MARGIN,
    command_controller::SharedCommandController, sidebar_context::unique_default_name,
};

mod model;

use model::{
    EDITOR_FIELDS, EditorField, EditorOperator, LIMIT_SELECTIONS, MATCH_KINDS, RuleError,
    StoredUntilEdited, ValueInput, ValueKind, automatic_name_for_single_text_rule, decompose_rule,
    effective_value_kind, extract_rule, index_of_field, index_of_limit_selection,
    index_of_match_kind, index_of_operator, limit_selection_label, match_kind_label,
    operators_for_field,
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
    /// Call `on_edited` whenever the control's value changes.
    fn connect_edited(&self, on_edited: impl Fn() + 'static) {
        match self {
            Self::Text(entry) | Self::Date(entry) => {
                entry.connect_changed(move |_| on_edited());
            }
            Self::Number(spin) | Self::Rating(spin) | Self::Days(spin) => {
                connect_spin_edited(spin, on_edited);
            }
            Self::Bool(dropdown) => {
                dropdown.connect_selected_notify(move |_| on_edited());
            }
            Self::None => {}
        }
    }

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
                entry.set_placeholder_text(Some(&gettext("value")));
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
            ValueKind::Bool => Self::Bool(gtk::DropDown::from_strings(&[
                &gettext("Yes"),
                &gettext("No"),
            ])),
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
    /// The stored rule the row opened with, for a row of an existing
    /// smart playlist.
    opened: Option<StoredUntilEdited<SmartPlaylistRule>>,
}

impl RuleRow {
    fn from_initial(initial: Option<&SmartPlaylistRule>) -> Self {
        let (initial_field, initial_operator, initial_value) = match initial {
            Some(rule) => decompose_rule(rule),
            None => (
                EDITOR_FIELDS[0],
                operators_for_field(EDITOR_FIELDS[0])[0],
                ValueInput::None,
            ),
        };

        let container = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        container.add_css_class("smart-playlist-rule-row");

        let field_model: gtk::StringList =
            EDITOR_FIELDS.iter().map(|field| field.label()).collect();
        let field_combo = gtk::DropDown::new(Some(field_model), gtk::Expression::NONE);
        field_combo.set_selected(index_of_field(initial_field));

        let operator_combo = gtk::DropDown::new(
            Some(operator_model_for_field(initial_field)),
            gtk::Expression::NONE,
        );
        operator_combo.set_selected(index_of_operator(initial_field, initial_operator));

        let value_container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        value_container.set_hexpand(true);

        // Translators: follows the day count of "is in the last [7] days".
        let days_suffix = gtk::Label::new(Some(&gettext("days")));
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
        let opened = initial.map(|rule| StoredUntilEdited::new(rule.clone()));
        // Connected once the initial value is shown, so only the user's
        // edits count. Changing the field or operator replaces the value
        // control, and is an edit of the rule itself.
        if let Some(opened) = &opened {
            let opened = opened.clone();
            current_value
                .borrow()
                .connect_edited(move || opened.edited());
        }

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
        let opened_for_field = opened.clone();
        field_combo.connect_selected_notify(move |dropdown| {
            if let Some(opened) = &opened_for_field {
                opened.edited();
            }
            let index = dropdown.selected() as usize;
            let Some(field) = EDITOR_FIELDS.get(index).copied() else {
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
        let opened_for_op = opened.clone();
        operator_combo.connect_selected_notify(move |dropdown| {
            if let Some(opened) = &opened_for_op {
                opened.edited();
            }
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
            opened,
        }
    }

    fn extract(&self) -> Result<SmartPlaylistRule, RuleError> {
        let rebuild = || {
            extract_rule(
                self.current_field.get(),
                self.current_operator.get(),
                &value_input_from_widget(&self.current_value.borrow()),
            )
        };
        match &self.opened {
            Some(opened) => opened.value_or_rebuilt(rebuild),
            None => rebuild(),
        }
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
    operators_for_field(field)
        .iter()
        .map(|op| op.label())
        .collect()
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
    parent: &impl IsA<gtk::Window>,
    command_controller: SharedCommandController,
    on_saved: Rc<dyn Fn()>,
    mode: SmartPlaylistEditorMode,
) {
    let window = gtk::Window::builder()
        .title(gettext("Smart Playlist"))
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
    let match_prefix = gtk::Label::new(Some(&gettext("Match")));
    let match_combo = gtk::DropDown::new(
        Some(
            MATCH_KINDS
                .iter()
                .map(|kind| match_kind_label(*kind))
                .collect::<gtk::StringList>(),
        ),
        gtk::Expression::NONE,
    );
    match_combo.set_selected(
        initial_rules
            .map(|rules| index_of_match_kind(rules.match_kind))
            .unwrap_or(0),
    );
    let match_suffix = gtk::Label::new(Some(&gettext("of the following rules:")));
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
    let limit_check = gtk::CheckButton::with_label(&gettext("Limit to"));
    let limit_count = gtk::SpinButton::with_range(1.0, 10_000.0, 1.0);
    limit_count.set_value(25.0);
    limit_count.set_digits(0);
    limit_count.set_sensitive(false);
    let limit_unit = gtk::Label::new(Some(&gettext("songs, selected by")));
    let limit_selection_combo = gtk::DropDown::new(
        Some(
            LIMIT_SELECTIONS
                .iter()
                .map(|selection| limit_selection_label(*selection))
                .collect::<gtk::StringList>(),
        ),
        gtk::Expression::NONE,
    );
    limit_selection_combo.set_selected(0);
    limit_selection_combo.set_sensitive(false);

    let mut opened_limit_count = None;
    if let Some(SmartPlaylistLimit { count, selection }) =
        initial_rules.and_then(|rules| rules.limit)
    {
        limit_check.set_active(true);
        opened_limit_count = Some(show_opened_limit_count(&limit_count, count));
        limit_selection_combo.set_selected(index_of_limit_selection(selection));
        limit_count.set_sensitive(true);
        limit_selection_combo.set_sensitive(true);
    }

    let limit_count_for_toggle = limit_count.downgrade();
    let limit_selection_for_toggle = limit_selection_combo.downgrade();
    limit_check.connect_toggled(move |check| {
        let active = check.is_active();
        if let Some(limit_count) = limit_count_for_toggle.upgrade() {
            limit_count.set_sensitive(active);
        }
        if let Some(limit_selection) = limit_selection_for_toggle.upgrade() {
            limit_selection.set_sensitive(active);
        }
    });

    limit_row.append(&limit_check);
    limit_row.append(&limit_count);
    limit_row.append(&limit_unit);
    limit_row.append(&limit_selection_combo);
    content.append(&limit_row);

    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    content.append(&spacer);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);

    let cancel_button = gtk::Button::with_label(&gettext("Cancel"));
    let ok_button = gtk::Button::with_label(&gettext("OK"));
    ok_button.add_css_class("suggested-action");

    let window_for_cancel = window.downgrade();
    cancel_button.connect_clicked(move |_| {
        if let Some(window) = window_for_cancel.upgrade() {
            window.close();
        }
    });

    // The OK handler owns the rule rows: it is the one that reads them, and
    // it lives outside them, so holding them closes no cycle. Every widget
    // it reads it reaches weakly, as the dialog's other handlers do (#370).
    let window_for_ok = window.downgrade();
    let command_controller_for_ok = command_controller.clone();
    let match_combo_for_ok = match_combo.downgrade();
    let limit_check_for_ok = limit_check.downgrade();
    let limit_count_for_ok = limit_count.downgrade();
    let limit_selection_for_ok = limit_selection_combo.downgrade();
    let opened_limit_count_for_ok = opened_limit_count;
    let on_saved_for_ok = on_saved.clone();
    ok_button.connect_clicked(move |_| {
        let (Some(match_combo), Some(limit_check), Some(limit_count), Some(limit_selection)) = (
            match_combo_for_ok.upgrade(),
            limit_check_for_ok.upgrade(),
            limit_count_for_ok.upgrade(),
            limit_selection_for_ok.upgrade(),
        ) else {
            return;
        };
        let extraction = extract_rule_set(
            &rule_rows.borrow(),
            &match_combo,
            &limit_check,
            &limit_count,
            opened_limit_count_for_ok.as_ref(),
            &limit_selection,
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
                    if let Some(window) = window_for_ok.upgrade() {
                        window.close();
                    }
                }
            }
            // The editor stays open on the rules so the user can fix them.
            Err(error) => command_controller_for_ok.report_command_warning(error.message()),
        }
    });

    buttons.append(&cancel_button);
    buttons.append(&ok_button);
    content.append(&buttons);

    panel.append(&close_row);
    panel.append(&content);
    frame.append(&panel);
    window.set_child(Some(&frame));

    // The controller belongs to the window and reaches it through itself,
    // not a capture (#370).
    let key_controller = gtk::EventControllerKey::new();
    key_controller.connect_key_pressed(move |controller, key, _keycode, _state| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(window) = controller.widget().and_downcast::<gtk::Window>() {
            window.close();
        }
        glib::Propagation::Stop
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

/// The rule rows as a row's own buttons see them: the rows own those
/// buttons, so the buttons reach the rows, and the container holding
/// them, weakly (#370).
type WeakRuleRows = std::rc::Weak<RefCell<Vec<RuleRow>>>;

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
        &row_buttons,
        &row_widget,
        Rc::downgrade(rule_rows),
        rules_container,
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
    close_button.set_tooltip_text(Some(&gettext("Close")));
    close_button.set_halign(gtk::Align::End);
    close_button.set_valign(gtk::Align::Center);
    close_button.set_hexpand(true);

    let window_for_close = window.downgrade();
    close_button.connect_clicked(move |_| {
        if let Some(window) = window_for_close.upgrade() {
            window.close();
        }
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
    remove_button.set_tooltip_text(Some(&gettext("Remove rule")));

    let add_button = gtk::Button::from_icon_name("list-add-symbolic");
    add_button.add_css_class("flat");
    add_button.set_tooltip_text(Some(&gettext("Add rule")));

    container.append(&remove_button);
    container.append(&add_button);

    RuleRowButtons {
        container,
        remove_button,
        add_button,
    }
}

fn connect_row_buttons(
    buttons: &RuleRowButtons,
    row_widget: &gtk::Box,
    rule_rows: WeakRuleRows,
    rules_container: &gtk::Box,
) {
    let rule_rows_for_remove = rule_rows.clone();
    let rules_container_for_remove = rules_container.downgrade();
    let row_widget_for_remove = row_widget.downgrade();
    buttons.remove_button.connect_clicked(move |_| {
        let (Some(rule_rows), Some(rules_container), Some(row_widget)) = (
            rule_rows_for_remove.upgrade(),
            rules_container_for_remove.upgrade(),
            row_widget_for_remove.upgrade(),
        ) else {
            return;
        };
        let mut rows = rule_rows.borrow_mut();
        if rows.len() <= 1 {
            return;
        }
        if let Some(index) = rows.iter().position(|row| row.container == row_widget) {
            rows.remove(index);
            rules_container.remove(&row_widget);
        }
    });

    let rules_container_for_add = rules_container.downgrade();
    buttons.add_button.connect_clicked(move |_| {
        if let (Some(rule_rows), Some(rules_container)) =
            (rule_rows.upgrade(), rules_container_for_add.upgrade())
        {
            append_rule_row(&rules_container, &rule_rows);
        }
    });
}

/// Show a stored limit count in `limit_count`, keeping the stored count
/// until the user edits the control.
fn show_opened_limit_count(
    limit_count: &gtk::SpinButton,
    count: NonZeroU32,
) -> StoredUntilEdited<NonZeroU32> {
    limit_count.set_value(f64::from(count.get()));
    let opened = StoredUntilEdited::new(count);
    let opened_for_edit = opened.clone();
    connect_spin_edited(limit_count, move || opened_for_edit.edited());
    opened
}

/// Call `on_edited` whenever the user edits `spin`. Its text is watched,
/// not its value: typing a number and typing it back leaves the value
/// where it was, yet the user has chosen what the control shows. Every
/// change of value rewrites the text, so arrow presses count too.
fn connect_spin_edited(spin: &gtk::SpinButton, on_edited: impl Fn() + 'static) {
    spin.connect_changed(move |_| on_edited());
}

fn extract_rule_set(
    rule_rows: &[RuleRow],
    match_combo: &gtk::DropDown,
    limit_check: &gtk::CheckButton,
    limit_count: &gtk::SpinButton,
    opened_limit_count: Option<&StoredUntilEdited<NonZeroU32>>,
    limit_selection_combo: &gtk::DropDown,
) -> Result<SmartPlaylistRuleSet, RuleError> {
    let rules: Vec<_> = rule_rows
        .iter()
        .map(RuleRow::extract)
        .collect::<Result<_, _>>()?;

    let limit = if limit_check.is_active() {
        let rebuild = || {
            let count_value = limit_count.value_as_int().max(1) as u32;
            Ok::<_, RuleError>(NonZeroU32::new(count_value).expect("count clamped to >= 1"))
        };
        let count = match opened_limit_count {
            Some(opened) => opened.value_or_rebuilt(rebuild)?,
            None => rebuild()?,
        };
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
        .copied()
        .unwrap_or(SmartPlaylistMatchKind::All)
}

fn limit_selection_from_combo(combo: &gtk::DropDown) -> SmartPlaylistLimitSelection {
    let index = combo.selected() as usize;
    LIMIT_SELECTIONS
        .get(index)
        .copied()
        .unwrap_or(SmartPlaylistLimitSelection::Random)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use sustain_app_runtime::ApplicationRuntime;

    use super::*;
    use crate::command_controller::UiCommandController;

    fn button_labelled(root: &gtk::Widget, label_text: &str) -> Option<gtk::Button> {
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(button) = widget.downcast_ref::<gtk::Button>()
                && button.label().as_deref() == Some(label_text)
            {
                return Some(button.clone());
            }
            if let Some(found) = button_labelled(&widget, label_text) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }

    fn number_rule(value: i64) -> SmartPlaylistRule {
        SmartPlaylistRule::Number {
            field: sustain_app_runtime::SmartPlaylistNumberField::Year,
            operator: sustain_app_runtime::SmartPlaylistNumberOperator::GreaterThan,
            value,
        }
    }

    fn number_spin(row: &RuleRow) -> gtk::SpinButton {
        match &*row.current_value.borrow() {
            ValueWidget::Number(spin) => Some(spin.clone()),
            _ => None,
        }
        .expect("a number rule shows a spin button")
    }

    #[test]
    fn an_opened_rule_its_control_cannot_show_is_saved_as_stored() {
        crate::test_support::with_gtk(|| {
            // The spin button starts at zero: it cannot show -1.
            let row = RuleRow::from_initial(Some(&number_rule(-1)));
            assert_eq!(number_spin(&row).value_as_int(), 0);

            assert_eq!(row.extract(), Ok(number_rule(-1)));
        });
    }

    #[test]
    fn an_opened_rule_the_user_changes_is_saved_as_shown() {
        crate::test_support::with_gtk(|| {
            let row = RuleRow::from_initial(Some(&number_rule(-1)));
            number_spin(&row).set_value(1990.0);

            assert_eq!(row.extract(), Ok(number_rule(1990)));
        });
    }

    #[test]
    fn an_opened_rule_edited_back_to_what_it_showed_is_saved_as_shown() {
        crate::test_support::with_gtk(|| {
            let row = RuleRow::from_initial(Some(&number_rule(-1)));
            number_spin(&row).set_value(1.0);
            number_spin(&row).set_value(0.0);

            assert_eq!(row.extract(), Ok(number_rule(0)));
        });
    }

    #[test]
    fn an_opened_limit_its_control_cannot_show_is_saved_as_stored() {
        crate::test_support::with_gtk(|| {
            let limit_check = gtk::CheckButton::new();
            limit_check.set_active(true);
            let limit_count = gtk::SpinButton::with_range(1.0, 10_000.0, 1.0);
            let stored = NonZeroU32::new(10_001).expect("positive count");
            let opened = show_opened_limit_count(&limit_count, stored);
            let selection = gtk::DropDown::from_strings(&["random"]);
            let rows = [RuleRow::from_initial(Some(&number_rule(2000)))];
            let extract = |count: &gtk::SpinButton| {
                extract_rule_set(
                    &rows,
                    &gtk::DropDown::from_strings(&["all"]),
                    &limit_check,
                    count,
                    Some(&opened),
                    &selection,
                )
                .expect("a valid rule set")
                .limit
                .expect("a limit")
                .count
            };

            assert_eq!(extract(&limit_count), stored);
            // Its last digit deleted and typed back: the user chose the
            // 10,000 it shows, though its value never changed.
            limit_count.set_text("1000");
            limit_count.set_text("10000");
            limit_count.update();
            assert_eq!(extract(&limit_count).get(), 10_000);
        });
    }

    #[test]
    fn the_closed_editor_is_released() {
        crate::test_support::with_gtk(|| {
            let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let command_controller: SharedCommandController =
                Rc::new(UiCommandController::new(runtime));
            let parent = gtk::Window::new();
            open_smart_playlist_editor(
                &parent,
                command_controller,
                Rc::new(|| {}),
                SmartPlaylistEditorMode::Create {
                    name: "Smart".to_owned(),
                },
            );
            // A row the user added holds the same handlers as the first.
            let editor = gtk::Window::list_toplevels()
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
                .find(|window| window.transient_for().as_ref() == Some(&parent))
                .expect("the editor window is open");
            let add_buttons = |editor: &gtk::Window| {
                crate::test_support::descendants(editor.upcast_ref())
                    .into_iter()
                    .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
                    .filter(|button| button.tooltip_text().as_deref() == Some("Add rule"))
                    .collect::<Vec<_>>()
            };
            add_buttons(&editor)
                .first()
                .expect("the row's add button")
                .emit_clicked();
            assert_eq!(add_buttons(&editor).len(), 2, "the click added a rule row");
            drop(editor);

            crate::test_support::assert_closes_and_disposes("Smart Playlist");
            parent.destroy();
        });
    }

    #[test]
    fn an_incomplete_rule_is_reported_through_the_notification_lane() {
        crate::test_support::with_gtk(|| {
            let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let command_controller: SharedCommandController =
                Rc::new(UiCommandController::new(runtime.clone()));
            let parent = gtk::Window::new();
            open_smart_playlist_editor(
                &parent,
                command_controller,
                Rc::new(|| {}),
                SmartPlaylistEditorMode::Create {
                    name: "Smart".to_owned(),
                },
            );
            let editor = gtk::Window::list_toplevels()
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
                .find(|window| window.transient_for().as_ref() == Some(&parent))
                .expect("the editor window is open");

            // The new editor's only rule is a text rule with no value.
            button_labelled(editor.upcast_ref(), "OK")
                .expect("the OK button")
                .emit_clicked();

            let shown = runtime
                .borrow()
                .notifications()
                .current_ephemeral()
                .map(|notification| notification.body.clone());
            assert_eq!(shown.as_deref(), Some("A text rule needs a value."));
            assert!(editor.is_visible(), "the editor stays open on the rules");
            editor.destroy();
        });
    }
}
