use zed_extension_api::{self as zed, Result};

struct LeanToExtension;

impl zed::Extension for LeanToExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        _worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        todo!()
    }
}

zed::register_extension!(LeanToExtension);
