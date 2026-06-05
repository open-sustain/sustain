// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::rc::Rc;

use sustain_app_runtime::{NotificationCategory, NotificationSeverity, runtime_error_text};
use sustain_artwork::ArtworkReadError;

use super::{ApplicationCommand, ApplicationRuntimeError, SharedRuntime};

#[derive(Clone)]
pub(crate) struct UiCommandController {
    runtime: SharedRuntime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandBatchResult {
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
    pub(crate) first_error: Option<ApplicationRuntimeError>,
}

impl UiCommandController {
    pub(crate) fn new(runtime: SharedRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> SharedRuntime {
        self.runtime.clone()
    }

    pub(crate) fn dispatch(
        &self,
        command: ApplicationCommand,
    ) -> Result<(), ApplicationRuntimeError> {
        let result = self.dispatch_unreported(command);
        if let Err(error) = &result {
            self.report_command_error(error);
        }
        result
    }

    pub(crate) fn dispatch_unreported(
        &self,
        command: ApplicationCommand,
    ) -> Result<(), ApplicationRuntimeError> {
        self.runtime.borrow_mut().handle_command(command)
    }

    pub(crate) fn dispatch_succeeded(&self, command: ApplicationCommand) -> bool {
        self.dispatch(command).is_ok()
    }

    pub(crate) fn report_artwork_selection_error(&self, error: &ArtworkReadError) {
        self.report_command_message(
            NotificationSeverity::Warning,
            format!("Could not use the selected artwork: {error}."),
        );
    }

    pub(crate) fn report_command_warning(&self, body: impl Into<String>) {
        self.report_command_message(NotificationSeverity::Warning, body.into());
    }

    pub(crate) fn dispatch_batch(
        &self,
        commands: impl IntoIterator<Item = ApplicationCommand>,
    ) -> CommandBatchResult {
        let mut result = CommandBatchResult {
            succeeded: 0,
            failed: 0,
            first_error: None,
        };

        for command in commands {
            match self.runtime.borrow_mut().handle_command(command) {
                Ok(()) => {
                    result.succeeded += 1;
                }
                Err(error) => {
                    result.failed += 1;
                    if result.first_error.is_none() {
                        result.first_error = Some(error);
                    }
                }
            }
        }

        match (result.succeeded, result.failed, result.first_error.as_ref()) {
            (_, 0, _) => {}
            (0, _, Some(error)) => self.report_command_error(error),
            (_, _, Some(_error)) => self.report_command_message(
                NotificationSeverity::Warning,
                "Some selected tracks could not be updated.".to_owned(),
            ),
            (_, _, None) => {}
        }

        result
    }

    pub(crate) fn report_command_error(&self, error: &ApplicationRuntimeError) {
        if matches!(
            error,
            ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(_)
        ) {
            // The runtime owns a persistent, typed warning for this
            // condition so it remains visible until a managed-root probe
            // succeeds. Do not stack a transient command error above it.
            return;
        }
        self.report_command_message(NotificationSeverity::Error, runtime_error_text(error));
    }

    fn report_command_message(&self, severity: NotificationSeverity, body: String) {
        self.runtime.borrow_mut().push_ephemeral_notification(
            NotificationCategory::Command,
            severity,
            body,
        );
    }
}

pub(crate) type SharedCommandController = Rc<UiCommandController>;
