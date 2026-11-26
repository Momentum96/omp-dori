//! `[update]` (IR-21, IR-22: the daily release notice).

use std::time::Duration;

use super::{ConfigError, LOG_DESTINATION, Language, Section};
use crate::jobs::event::{EventKind, Template};
use crate::relay::http::Base;

/// The default notice in Korean; `{current}`, `{latest}` and `{url}` are filled in.
const KOREAN: &str = "새로운 버전이 나왔습니다! 보스. 콘솔 창에 `cataly update` 명령어를 입력해주세요.\n\
                      {current} -> {latest} {url}";
/// The default notice in English.
const ENGLISH: &str = "A new version is out, boss. Run `cataly update` in a terminal.\n\
                       {current} -> {latest} {url}";
/// After the hub first runs a newer version, in Korean and English; `{previous}`, `{latest}` and
/// `{sessions}` (the sessions resumed, a sentence or nothing).
const UPDATED_KOREAN: &str = "{latest} 버전으로 업데이트했습니다. 보스, 감사합니다!\n\
                              {previous} -> {latest}\n{sessions}";
const UPDATED_ENGLISH: &str =
    "Updated to {latest}. Thanks, boss!\n{previous} -> {latest}\n{sessions}";
/// After the hub first runs an older version (a rollback).
const ROLLED_BACK_KOREAN: &str =
    "{latest} 버전으로 되돌렸습니다.\n{previous} -> {latest}\n{sessions}";
const ROLLED_BACK_ENGLISH: &str = "Rolled back to {latest}.\n{previous} -> {latest}\n{sessions}";

/// `[update]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateSettings {
    /// The daily check for a newer stable release; off means no request is ever made (IR req. 38).
    pub(crate) check: bool,
    /// Loopback-only test override of GitHub's API.
    pub(crate) api_base: Option<Base>,
    /// The first check after the hub starts (IR req. 36: 10 minutes).
    pub(crate) first_after: Duration,
    /// The owner's notice wording, checked at load; `None` is the default in `[hub] language`.
    pub(crate) template: Option<String>,
    /// The owner's wording after an update, checked at load; `None` is the default.
    pub(crate) updated_template: Option<String>,
    /// Named destinations; `None` posts to the owner's home room.
    pub(crate) destinations: Option<Vec<String>>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check: true,
            api_base: None,
            first_after: Duration::from_mins(10),
            template: None,
            updated_template: None,
            destinations: None,
        }
    }
}

impl UpdateSettings {
    /// Where release metadata comes from.
    pub(crate) fn api(&self) -> Result<Base, String> {
        match &self.api_base {
            Some(b) => Ok(b.clone()),
            None => Base::parse(crate::sys::release::GITHUB_API),
        }
    }

    /// The notice wording: the owner's, else the default in the owner's language.
    pub(crate) fn template(&self, language: Language) -> Result<Template, String> {
        let text = self.template.as_deref().unwrap_or(match language {
            Language::Korean => KOREAN,
            Language::English => ENGLISH,
        });
        Template::parse(text, EventKind::UpdateAvailable)
    }

    /// The message after the hub first runs a new version: the owner's update wording, else the
    /// default in the owner's language; a rollback always has its default.
    pub(crate) fn updated(
        &self,
        language: Language,
        rolled_back: bool,
    ) -> Result<Template, String> {
        let (kind, text) = match (rolled_back, language, self.updated_template.as_deref()) {
            (true, Language::Korean, _) => (EventKind::UpdateRolledBack, ROLLED_BACK_KOREAN),
            (true, Language::English, _) => (EventKind::UpdateRolledBack, ROLLED_BACK_ENGLISH),
            (false, _, Some(own)) => (EventKind::UpdateDone, own),
            (false, Language::Korean, None) => (EventKind::UpdateDone, UPDATED_KOREAN),
            (false, Language::English, None) => (EventKind::UpdateDone, UPDATED_ENGLISH),
        };
        Template::parse(text, kind)
    }
}

pub(super) fn parse(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    update: &mut UpdateSettings,
) {
    if let Some(mut u) = root.table(errors, "update") {
        update.check = u.bool(errors, "check").unwrap_or(true);
        if let Some(base) = u.string(errors, "api_base") {
            match Base::parse(base) {
                Ok(b) if b.is_loopback() => update.api_base = Some(b),
                Ok(_) => {
                    errors.push(u.error("api_base", "the test override must point at localhost"));
                }
                Err(e) => errors.push(u.error("api_base", &e)),
            }
        }
        if let Some(d) = u.duration(
            errors,
            "first_after",
            Duration::from_secs(1),
            Duration::from_hours(24),
        ) {
            update.first_after = d;
        }
        if let Some(text) = u.string(errors, "template") {
            match Template::parse(text, EventKind::UpdateAvailable) {
                Ok(_) => update.template = Some(text.to_string()),
                Err(e) => errors.push(u.error("template", &e)),
            }
        }
        if let Some(text) = u.string(errors, "updated_template") {
            match Template::parse(text, EventKind::UpdateDone) {
                Ok(_) => update.updated_template = Some(text.to_string()),
                Err(e) => errors.push(u.error("updated_template", &e)),
            }
        }
        if let Some(names) = u.strings(errors, "destinations") {
            // Names are checked against `[destinations]` once that table is read.
            update.destinations =
                Some(names.into_iter().filter(|n| n != LOG_DESTINATION).collect());
        }
        u.finish(errors);
    }
}

#[cfg(test)]
mod tests {
    use crate::config;

    #[test]
    fn update_settings() {
        let c = config::parse("").unwrap();
        assert!(c.update.check);
        assert_eq!(c.update.first_after.as_secs(), 600);
        let c = config::parse(
            "[update]\ncheck = false\nfirst_after = \"2s\"\napi_base = \"http://127.0.0.1:9\"\n",
        )
        .unwrap();
        assert!(!c.update.check);
        assert!(config::parse("[update]\napi_base = \"https://api.github.com\"\n").is_err());
    }
}
