use crate::Result;
use crate::storage::Database;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::env;
use std::time::Duration;

pub fn settings() -> (String, String, String) {
    (
        env::var("TELEGRAM_BOT_TOKEN")
            .unwrap_or_default()
            .trim()
            .into(),
        env::var("TELEGRAM_CHAT_ID")
            .unwrap_or_default()
            .trim()
            .into(),
        env::var("TELEGRAM_THREAD_ID")
            .unwrap_or_default()
            .trim()
            .into(),
    )
}

fn request(token: &str, method: &str, payload: &Value, timeout: u64) -> Result<Value> {
    let response: Value = Client::builder()
        .timeout(Duration::from_secs(timeout.max(1)))
        .build()?
        .post(format!("https://api.telegram.org/bot{token}/{method}"))
        .json(payload)
        .send()?
        .error_for_status()?
        .json()?;
    if response.get("ok") == Some(&Value::Bool(true)) {
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(response
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("Telegram API error")
            .to_owned()
            .into())
    }
}

pub fn format_alert(alert: &Value) -> String {
    let score = alert
        .get("score")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let title = alert
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut lines = vec![format!("Job Match ({score}): {title}")];
    if let Some(company) = alert
        .get("company")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("Company: {company}"));
    }
    if alert.get("shortlisted") == Some(&Value::Bool(true)) {
        lines.push("Priority: shortlisted employer".into());
    } else if alert.get("company_control").and_then(Value::as_str) == Some("whitelist") {
        lines.push("Priority: company whitelist".into());
    }
    if let Some(role) = alert
        .get("role_profile")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("Role Profile: {role}"));
    }
    if let Some(source) = alert
        .get("source")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("Source: {source}"));
    }
    let reasons = alert
        .get("reasons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(2)
        .collect::<Vec<_>>();
    if !reasons.is_empty() {
        lines.push(format!("Why: {}", reasons.join("; ")));
    }
    lines.push(
        alert
            .get("link")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
    );
    lines.join("\n")
}

pub fn send_message(message: &str, markup: Option<Value>) -> Result<Value> {
    let (token, chat_id, thread_id) = settings();
    if token.is_empty() || chat_id.is_empty() {
        return Err("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be configured".into());
    }
    let mut payload = json!({"chat_id":chat_id,"text":message,"disable_web_page_preview":true});
    if !thread_id.is_empty() {
        payload["message_thread_id"] = Value::String(thread_id);
    }
    if let Some(markup) = markup {
        payload["reply_markup"] = markup;
    }
    request(&token, "sendMessage", &payload, 20)
}

fn edit_message(
    token: &str,
    chat_id: &str,
    message_id: i64,
    message: &str,
    markup: Option<Value>,
) -> Result<Value> {
    let mut payload = json!({"chat_id":chat_id,"message_id":message_id,"text":message,"disable_web_page_preview":true});
    if let Some(markup) = markup {
        payload["reply_markup"] = markup;
    }
    request(token, "editMessageText", &payload, 20)
}

pub fn queue_alerts(state: &mut Value, matches: &[Value]) -> usize {
    let alerted: HashSet<String> = state["alerted_links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let pending_links: HashSet<String> = state["pending_alerts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("link")?.as_str())
        .map(str::to_owned)
        .collect();
    let pending = state["pending_alerts"]
        .as_array_mut()
        .expect("database returns an array");
    let mut count = 0;
    for item in matches {
        let link = item.get("link").and_then(Value::as_str).unwrap_or_default();
        if link.is_empty() || alerted.contains(link) || pending_links.contains(link) {
            continue;
        }
        pending.push(item.clone());
        count += 1;
    }
    count
}

pub fn deliver_alerts(state: &mut Value, timestamp: &str) -> (usize, String) {
    let pending = state["pending_alerts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if pending.is_empty() {
        return (0, String::new());
    }
    let mut sent = 0;
    for alert in pending {
        match send_message(&format_alert(&alert), None) {
            Ok(_) => {
                let link = alert
                    .get("link")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                state["pending_alerts"]
                    .as_array_mut()
                    .expect("array")
                    .retain(|item| item.get("link").and_then(Value::as_str) != Some(&link));
                let alerted = state["alerted_links"].as_array_mut().expect("array");
                alerted.push(Value::String(link));
                if alerted.len() > 5000 {
                    alerted.drain(..alerted.len() - 5000);
                }
                sent += 1;
                state["last_delivery_utc"] = Value::String(timestamp.into());
                state["last_delivery_error"] = Value::String(String::new());
            }
            Err(error) => {
                let message = error.to_string();
                state["last_delivery_error"] = Value::String(message.clone());
                return (sent, message);
            }
        }
    }
    (sent, String::new())
}

pub fn digest_keyboard(session: &str, page: usize, total: usize) -> Option<Value> {
    if total <= 1 {
        return None;
    }
    Some(json!({"inline_keyboard":[[
        {"text":"Previous","callback_data":format!("digest:{session}:{}",if page == 0 { "noop".into() } else { (page-1).to_string() })},
        {"text":format!("{}/{}",page+1,total),"callback_data":format!("digest:{session}:noop")},
        {"text":"Next","callback_data":format!("digest:{session}:{}",if page+1 >= total { "noop".into() } else { (page+1).to_string() })}
    ]] }))
}

pub fn parse_digest_callback(value: &str) -> Option<(&str, &str)> {
    let mut parts = value.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("digest"), Some(session), Some(page), None)
            if !session.is_empty() && !page.is_empty() =>
        {
            Some((session, page))
        }
        _ => None,
    }
}

pub fn process_updates(db: &Database, timeout: u64) -> Result<usize> {
    let (token, _, _) = settings();
    if token.is_empty() {
        return Err("TELEGRAM_BOT_TOKEN not configured".into());
    }
    let offset = db.telegram_offset()?;
    let result = request(
        &token,
        "getUpdates",
        &json!({"offset":offset,"limit":20,"timeout":timeout,"allowed_updates":["callback_query"]}),
        timeout + 5,
    )?;
    let updates = result.as_array().cloned().unwrap_or_default();
    let mut next_offset = offset;
    for update in &updates {
        next_offset = next_offset.max(
            update
                .get("update_id")
                .and_then(Value::as_i64)
                .unwrap_or_default()
                + 1,
        );
        if let Some(callback) = update.get("callback_query") {
            let id = callback
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let data = callback
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut acknowledgement = "Invalid digest action";
            if let Some((session, page)) = parse_digest_callback(data) {
                if page == "noop" {
                    acknowledgement = "This page is already selected";
                } else if let Ok(page_index) = page.parse::<usize>() {
                    if let Some(pages) = db.digest_session(session)? {
                        if let (Some(text), Some(message)) =
                            (pages.get(page_index), callback.get("message"))
                        {
                            let chat_id = message
                                .get("chat")
                                .and_then(|chat| chat.get("id"))
                                .and_then(Value::as_i64)
                                .unwrap_or_default()
                                .to_string();
                            let message_id = message
                                .get("message_id")
                                .and_then(Value::as_i64)
                                .unwrap_or_default();
                            edit_message(
                                &token,
                                &chat_id,
                                message_id,
                                text,
                                digest_keyboard(session, page_index, pages.len()),
                            )?;
                            acknowledgement = "";
                        }
                    } else {
                        acknowledgement = "This digest has expired";
                    }
                }
            }
            if !id.is_empty() {
                let _ = request(
                    &token,
                    "answerCallbackQuery",
                    &json!({"callback_query_id":id,"text":acknowledgement}),
                    20,
                );
            }
        }
    }
    db.save_telegram_offset(next_offset)?;
    Ok(updates.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_callback() {
        assert_eq!(parse_digest_callback("digest:abc:2"), Some(("abc", "2")));
        assert_eq!(parse_digest_callback("wrong:abc:2"), None);
    }
    #[test]
    fn builds_keyboard() {
        assert!(digest_keyboard("abc", 0, 2).is_some());
        assert!(digest_keyboard("abc", 0, 1).is_none());
    }
}
