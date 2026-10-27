use anyhow::{Context as _, Result, bail};
use futures::{AsyncWriteExt as _, future::BoxFuture};
use smol::channel::Receiver;
use std::{
    collections::HashMap,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{NotifyConfig, expand_home};

/// Something worth telling a user who is not looking at the session.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    /// The agent asked for permission or input. `key` identifies the request
    /// until it is answered or withdrawn.
    InputRequested {
        key: String,
        agent: String,
        title: String,
        detail: String,
    },
    InputResolved {
        key: String,
    },
    TurnEnded {
        agent: String,
        title: String,
        stop_reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub title: String,
    pub message: String,
    /// ntfy priority, from 1 (min) to 5 (max).
    pub priority: u8,
    pub tags: &'static str,
}

pub type SendNotification =
    Arc<dyn Fn(Notification) -> BoxFuture<'static, Result<()>> + Send + Sync>;

const TITLE_LIMIT: usize = 60;

fn shorten(text: &str) -> String {
    let mut characters = text.chars();
    let short = characters.by_ref().take(TITLE_LIMIT).collect::<String>();
    if characters.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

fn stop_reason_label(stop_reason: &str) -> &'static str {
    match stop_reason {
        "end_turn" => "已完成",
        "cancelled" => "已取消",
        "max_tokens" => "达到 token 上限",
        "max_turn_requests" => "达到轮次上限",
        "refusal" => "agent 拒绝了请求",
        _ => "出错",
    }
}

pub fn notification_for(notice: &Notice) -> Option<Notification> {
    match notice {
        Notice::InputRequested {
            agent,
            title,
            detail,
            ..
        } => Some(Notification {
            title: format!("等待授权 · {}", shorten(title)),
            message: format!("{agent}：{detail}"),
            priority: 4,
            tags: "warning",
        }),
        Notice::InputResolved { .. } => None,
        Notice::TurnEnded {
            agent,
            title,
            stop_reason,
        } => {
            let label = stop_reason_label(stop_reason);
            Some(Notification {
                title: format!("{label} · {}", shorten(title)),
                message: format!("{agent} 的任务{label}"),
                priority: 3,
                tags: if stop_reason == "end_turn" {
                    "white_check_mark"
                } else {
                    "x"
                },
            })
        }
    }
}

/// Sends a notification for every notice. Input requests are only announced
/// when nobody answers them within `input_delay`, so that a user who is
/// watching the session is not notified about it.
pub async fn run(notices: Receiver<Notice>, input_delay: Duration, send: SendNotification) {
    let mut delayed: HashMap<String, (Instant, Notification)> = HashMap::new();
    loop {
        let next_deadline = delayed.values().map(|(deadline, _)| *deadline).min();
        let notice = match next_deadline {
            None => match notices.recv().await {
                Ok(notice) => Some(notice),
                Err(_) => return,
            },
            Some(deadline) => {
                let received = notices.recv();
                let timer = smol::Timer::at(deadline);
                futures::pin_mut!(received, timer);
                match futures::future::select(received, timer).await {
                    futures::future::Either::Left((Ok(notice), _)) => Some(notice),
                    futures::future::Either::Left((Err(_), _)) => return,
                    futures::future::Either::Right(_) => None,
                }
            }
        };

        if let Some(notice) = notice {
            match &notice {
                Notice::InputRequested { key, .. } => {
                    if let Some(notification) = notification_for(&notice) {
                        if input_delay.is_zero() {
                            deliver(&send, notification);
                        } else {
                            delayed
                                .insert(key.clone(), (Instant::now() + input_delay, notification));
                        }
                    }
                }
                Notice::InputResolved { key } => {
                    delayed.remove(key);
                }
                Notice::TurnEnded { .. } => {
                    if let Some(notification) = notification_for(&notice) {
                        deliver(&send, notification);
                    }
                }
            }
        }

        let now = Instant::now();
        let due = delayed
            .iter()
            .filter(|(_, (deadline, _))| *deadline <= now)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in due {
            if let Some((_, notification)) = delayed.remove(&key) {
                deliver(&send, notification);
            }
        }
    }
}

fn deliver(send: &SendNotification, notification: Notification) {
    let sending = send(notification);
    smol::spawn(async move {
        if let Err(error) = sending.await {
            log::warn!("sending a notification failed: {error:#}");
        }
    })
    .detach();
}

/// Publishes to an ntfy topic with curl, which avoids linking a TLS stack into
/// the daemon. The request is passed on stdin so that the access token and the
/// message do not appear in the process list.
pub fn ntfy(config: &NotifyConfig) -> Result<SendNotification> {
    let base = url::Url::parse(&config.ntfy_url)
        .with_context(|| format!("invalid ntfy_url {:?}", config.ntfy_url))?;
    let token = match &config.ntfy_token_file {
        Some(path) => {
            let path = expand_home(path);
            Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?
                    .trim()
                    .to_string(),
            )
        }
        None => None,
    };
    Ok(Arc::new(move |notification: Notification| {
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("title", &notification.title)
            .append_pair("priority", &notification.priority.to_string())
            .append_pair("tags", notification.tags);
        let mut curl_config = format!(
            "url = {}\ndata-binary = {}\n",
            curl_quote(url.as_str()),
            curl_quote(&notification.message)
        );
        if let Some(token) = &token {
            curl_config.push_str(&format!(
                "header = {}\n",
                curl_quote(&format!("Authorization: Bearer {token}"))
            ));
        }
        Box::pin(async move {
            let mut child = smol::process::Command::new("curl")
                .args(["--silent", "--show-error", "--fail", "--max-time", "15"])
                .args(["--config", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .context("running curl to publish to ntfy")?;
            let mut stdin = child.stdin.take().context("curl stdin was not captured")?;
            stdin.write_all(curl_config.as_bytes()).await?;
            stdin.close().await?;
            drop(stdin);
            let output = child.output().await?;
            if !output.status.success() {
                bail!(
                    "curl failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(())
        })
    }))
}

fn curl_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    fn recording_sender() -> (SendNotification, Receiver<Notification>) {
        let (sent, received) = smol::channel::unbounded();
        let send: SendNotification = Arc::new(move |notification| {
            sent.try_send(notification).expect("recording notification");
            Box::pin(async { Ok(()) })
        });
        (send, received)
    }

    fn input_requested(key: &str) -> Notice {
        Notice::InputRequested {
            key: key.to_string(),
            agent: "claude-acp".to_string(),
            title: "修复登录".to_string(),
            detail: "Edit src/login.rs".to_string(),
        }
    }

    #[test]
    fn announces_unanswered_requests_and_finished_turns() {
        let (send, sent) = recording_sender();
        let (notices, receiver) = smol::channel::unbounded();
        let sent = smol::block_on(async {
            let runner = smol::spawn(run(receiver, Duration::from_millis(100), send));
            notices.send(input_requested("answered")).await.ok();
            notices
                .send(Notice::InputResolved {
                    key: "answered".to_string(),
                })
                .await
                .ok();
            notices.send(input_requested("waiting")).await.ok();
            notices
                .send(Notice::TurnEnded {
                    agent: "claude-acp".to_string(),
                    title: "修复登录".to_string(),
                    stop_reason: "end_turn".to_string(),
                })
                .await
                .ok();
            // Wait for delivery rather than assuming the runner was scheduled
            // within a fixed delay on a busy build host.
            let received = async {
                vec![
                    sent.recv().await.expect("turn notification"),
                    sent.recv().await.expect("permission notification"),
                ]
            };
            let timeout = smol::Timer::at(Instant::now() + Duration::from_secs(5));
            futures::pin_mut!(received, timeout);
            let mut notifications = match futures::future::select(received, timeout).await {
                futures::future::Either::Left((notifications, _)) => notifications,
                futures::future::Either::Right(_) => panic!("timed out waiting for notifications"),
            };
            drop(notices);
            runner.await;
            while let Ok(notification) = sent.try_recv() {
                notifications.push(notification);
            }
            notifications
        });

        assert_eq!(
            sent,
            vec![
                Notification {
                    title: "已完成 · 修复登录".to_string(),
                    message: "claude-acp 的任务已完成".to_string(),
                    priority: 3,
                    tags: "white_check_mark",
                },
                Notification {
                    title: "等待授权 · 修复登录".to_string(),
                    message: "claude-acp：Edit src/login.rs".to_string(),
                    priority: 4,
                    tags: "warning",
                },
            ]
        );
    }

    #[test]
    fn quotes_curl_config_values() {
        assert_eq!(
            curl_quote("say \"hi\"\nC:\\path"),
            "\"say \\\"hi\\\"\\nC:\\\\path\""
        );
    }
}
