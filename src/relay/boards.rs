//! The board post (EVR-30) on Telegram and Slack through the lanes' [`Board`] interface: a create is
//! one message (neither platform enforces a nonce, so an uncertain create leaves the destination in
//! doubt, as the board rules want), an edit changes it in place (TELE-09, SLACK-09).

use std::sync::Arc;

use tokio::sync::watch;

use super::{slack, telegram};
use crate::config::Config;
use crate::lanes::BoxFut;
use crate::lanes::board::Board;
use crate::store::Store;

/// Board posts for `telegram:<chat>[/<topic>]` and `slack:<channel>` destinations.
pub(crate) struct ChatBoards {
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) config_file: std::path::PathBuf,
    pub(crate) store: Store,
}

impl ChatBoards {
    fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config.borrow())
    }
}

impl Board for ChatBoards {
    fn board_create<'a>(
        &'a self,
        dest: &'a str,
        text: &'a str,
        _nonce: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>> {
        Box::pin(async move {
            let config = self.config();
            if let Some(target) = dest.strip_prefix("telegram:") {
                let (chat, topic) = telegram::board_target(target)
                    .ok_or((false, "bad Telegram destination".to_string()))?;
                let api = telegram::Api::from_config(&config, &self.config_file)
                    .map_err(|e| (false, e))?;
                let scope = telegram::Scope::of(&config.telegram)
                    .ok_or((false, "set [telegram] owner first".to_string()))?;
                // The body is cut to fit one message, so it is one part.
                return match telegram::send(&api, &scope, chat, topic, None, text, false).await {
                    telegram::Sent::Confirmed(ids) => ids
                        .first()
                        .map(ToString::to_string)
                        .ok_or((true, "no message ID".to_string())),
                    telegram::Sent::Rejected(_, why) => Err((false, why)),
                    telegram::Sent::Uncertain(_, why) => Err((true, why)),
                };
            }
            if let Some(channel) = dest.strip_prefix("slack:") {
                let api =
                    slack::Api::from_config(&config, &self.config_file).map_err(|e| (false, e))?;
                return match slack::send(&api, &self.store, channel, None, text, &config.outgoing)
                    .await
                {
                    Ok(ts) => ts
                        .into_iter()
                        .next()
                        .ok_or((true, "no timestamp".to_string())),
                    Err((_, e)) => Err((e.uncertain, e.to_string())),
                };
            }
            Err((false, format!("unknown board destination {dest}")))
        })
    }

    fn board_edit<'a>(
        &'a self,
        dest: &'a str,
        message: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            let config = self.config();
            if let Some(target) = dest.strip_prefix("telegram:") {
                let (chat, _) = telegram::board_target(target).ok_or("bad Telegram destination")?;
                let id = message
                    .parse::<i64>()
                    .map_err(|_| "bad message ID".to_string())?;
                let api = telegram::Api::from_config(&config, &self.config_file)?;
                let scope =
                    telegram::Scope::of(&config.telegram).ok_or("set [telegram] owner first")?;
                return telegram::edit(&api, &scope, chat, id, text, false)
                    .await
                    .map_err(|e| e.to_string());
            }
            if let Some(channel) = dest.strip_prefix("slack:") {
                let api = slack::Api::from_config(&config, &self.config_file)?;
                return slack::edit(&api, &self.store, channel, message, text)
                    .await
                    .map_err(|e| e.to_string());
            }
            Err(format!("unknown board destination {dest}"))
        })
    }
}
