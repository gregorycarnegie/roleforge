use crate::{Result, card::card, storage::Database};
use regex::Regex;
use reqwest::blocking::{
    Client, RequestBuilder, Response,
    multipart::{Form, Part},
};
use serde_json::{Value, json};
use std::{collections::HashSet, env, sync::LazyLock, time::Duration};

pub fn token() -> String {
    env::var("TELEGRAM_BOT_TOKEN")
        .unwrap_or_default()
        .trim()
        .into()
}

fn settings() -> (String, String, String) {
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

fn call(
    token: &str,
    method: &str,
    timeout: u64,
    body: impl FnOnce(RequestBuilder) -> RequestBuilder,
) -> Result<Value> {
    // Every Telegram URL carries the bot token, and reqwest puts the URL it
    // failed on into the error. These errors are logged and persisted, so the
    // URL is dropped before the token can travel with it.
    let response: Value = body(
        Client::builder()
            .timeout(Duration::from_secs(timeout.max(1)))
            .build()?
            .post(format!("https://api.telegram.org/bot{token}/{method}")),
    )
    .send()
    .and_then(Response::error_for_status)
    .and_then(|response| response.json())
    .map_err(reqwest::Error::without_url)?;
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

fn request(token: &str, method: &str, payload: &Value, timeout: u64) -> Result<Value> {
    call(token, method, timeout, |post| post.json(payload))
}

fn credentials() -> Result<(String, String, String)> {
    let (token, chat_id, thread_id) = settings();
    if token.is_empty() || chat_id.is_empty() {
        return Err("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be configured".into());
    }
    Ok((token, chat_id, thread_id))
}

/// Standard emoji, not Telegram's animated custom ones: `custom_emoji`
/// entities are only accepted from bots with a username bought on Fragment.
fn tier(score: i64) -> &'static str {
    match score {
        80.. => "\u{1f525}",
        60..=79 => "\u{26a1}",
        _ => "\u{1f4cc}",
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
    let mut lines = vec![format!("{} Job Match ({score}): {title}", tier(score))];
    if let Some(company) = alert
        .get("company")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("\u{1f3e2} Company: {company}"));
    }
    if alert.get("shortlisted") == Some(&Value::Bool(true)) {
        lines.push("\u{2b50} Priority: shortlisted employer".into());
    } else if alert.get("company_control").and_then(Value::as_str) == Some("whitelist") {
        lines.push("\u{2b50} Priority: company whitelist".into());
    }
    if let Some(role) = alert
        .get("role_profile")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("\u{1f3af} Role Profile: {role}"));
    }
    if let Some(source) = alert
        .get("source")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("\u{1f4e1} Source: {source}"));
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
        lines.push(format!("\u{2705} Why: {}", reasons.join("; ")));
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

/// Telegram caps a caption at 1024 code points. The link is the last line and
/// the only actionable part of an alert, so the body gives way, never the URL.
fn caption(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let (body, link) = text.rsplit_once('\n').unwrap_or(("", text));
    let link_length = link.chars().count();
    // An ellipsis and the newline before the link cost two more characters.
    if link_length + 2 >= limit {
        return link.chars().take(limit).collect();
    }
    let kept: String = body.chars().take(limit - link_length - 2).collect();
    format!("{kept}\u{2026}\n{link}")
}

/// The first image the feed embedded in the posting, usually the company logo
/// or a banner. Telegram fetches the URL itself, so nothing is downloaded here.
static IMAGE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)https?://[^\s"'<>]+\.(?:jpe?g|png|webp|gif)"#).expect("valid image regex")
});

fn photo_url(alert: &Value) -> Option<&str> {
    IMAGE_URL
        .find(alert.get("description").and_then(Value::as_str)?)
        .map(|found| found.as_str())
}

fn dispatch(method: &str, payload: impl FnOnce(&str, &str) -> Value) -> Result<Value> {
    let (token, chat_id, thread_id) = credentials()?;
    request(&token, method, &payload(&chat_id, &thread_id), 20)
}

/// Uploads the drawn card, which has no URL for Telegram to fetch.
pub fn send_card(png: Vec<u8>, text: &str) -> Result<Value> {
    let (token, chat_id, thread_id) = credentials()?;
    let mut form = Form::new()
        .text("chat_id", chat_id)
        .text("caption", caption(text, 1024))
        .part(
            "photo",
            Part::bytes(png)
                .file_name("card.png")
                .mime_str("image/png")?,
        );
    if !thread_id.is_empty() {
        form = form.text("message_thread_id", thread_id);
    }
    // Uploads are slower than a text send, and the card is half a megabyte.
    call(&token, "sendPhoto", 60, |post| post.multipart(form))
}

pub fn send_message(message: &str, markup: Option<Value>) -> Result<Value> {
    dispatch("sendMessage", |chat_id, thread_id| {
        message_payload(chat_id, thread_id, message, markup)
    })
}

pub fn send_photo(url: &str, text: &str) -> Result<Value> {
    dispatch("sendPhoto", |chat_id, thread_id| {
        photo_payload(chat_id, thread_id, url, text)
    })
}

fn photo_payload(chat_id: &str, thread_id: &str, url: &str, text: &str) -> Value {
    let mut payload = message_payload(chat_id, thread_id, "", None);
    payload.as_object_mut().expect("object").remove("text");
    payload["photo"] = Value::String(url.to_owned());
    payload["caption"] = Value::String(caption(text, 1024));
    payload
}

fn message_payload(chat_id: &str, thread_id: &str, message: &str, markup: Option<Value>) -> Value {
    let mut payload = json!({"chat_id":chat_id,"text":message,"disable_web_page_preview":true});
    if !thread_id.is_empty() {
        payload["message_thread_id"] = Value::String(thread_id.into());
    }
    if let Some(markup) = markup {
        payload["reply_markup"] = markup;
    }
    payload
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

/// Each delivery is recorded before the next one is attempted: a crash
/// part-way through would otherwise resend everything already sent.
pub fn deliver_alerts(db: &mut Database, state: &mut Value, timestamp: &str) -> (usize, String) {
    let pending = state["pending_alerts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if pending.is_empty() {
        return (0, String::new());
    }
    let mut sent = 0;
    for alert in pending {
        let text = format_alert(&alert);
        // A dead or unfetchable image must not cost the alert, so the plain
        // message is the fallback.
        let score = alert
            .get("score")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let link = alert
            .get("link")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let delivery = match photo_url(&alert) {
            Some(url) => send_photo(url, &text),
            None => send_card(card(link, score), &text),
        }
        .or_else(|_| send_message(&text, None));
        match delivery {
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
                if let Err(error) = db.save_alert_state(state) {
                    return (sent, error.to_string());
                }
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
    let token = token();
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
    use rstest::rstest;

    #[rstest]
    #[case::valid("digest:abc:2", Some(("abc", "2")))]
    #[case::wrong_prefix("wrong:abc:2", None)]
    #[case::missing_page("digest:abc", None)]
    #[case::extra_field("digest:abc:2:3", None)]
    #[case::empty_session("digest::2", None)]
    #[case::empty_page("digest:abc:", None)]
    fn parses_callback(#[case] data: &str, #[case] expected: Option<(&str, &str)>) {
        assert_eq!(parse_digest_callback(data), expected);
    }

    #[rstest]
    #[case::multiple_pages(2, true)]
    #[case::single_page(1, false)]
    #[case::no_pages(0, false)]
    fn keyboard_needs_multiple_pages(#[case] total: usize, #[case] expected: bool) {
        assert_eq!(digest_keyboard("abc", 0, total).is_some(), expected);
    }

    #[rstest]
    #[case::first(0, "digest:abc:noop", "1/3", "digest:abc:1")]
    #[case::middle(1, "digest:abc:0", "2/3", "digest:abc:2")]
    #[case::last(2, "digest:abc:1", "3/3", "digest:abc:noop")]
    fn builds_keyboard(
        #[case] page: usize,
        #[case] previous: &str,
        #[case] label: &str,
        #[case] next: &str,
    ) {
        let keyboard = digest_keyboard("abc", page, 3).unwrap();
        let buttons = &keyboard["inline_keyboard"][0];
        assert_eq!(buttons[0]["callback_data"], previous);
        assert_eq!(buttons[1]["text"], label);
        assert_eq!(buttons[1]["callback_data"], "digest:abc:noop");
        assert_eq!(buttons[2]["callback_data"], next);
    }

    #[test]
    fn formats_a_full_alert() {
        let alert = json!({
            "score": 87,
            "title": "IT Support Engineer",
            "company": "Acme Ltd",
            "shortlisted": true,
            "company_control": "priority",
            "role_profile": "IT Support",
            "source": "wwr",
            "reasons": ["first reason", "second reason", "third reason"],
            "link": "https://example.com/jobs/1"
        });
        assert_eq!(
            format_alert(&alert),
            "\u{1f525} Job Match (87): IT Support Engineer\n\
             \u{1f3e2} Company: Acme Ltd\n\
             \u{2b50} Priority: shortlisted employer\n\
             \u{1f3af} Role Profile: IT Support\n\
             \u{1f4e1} Source: wwr\n\
             \u{2705} Why: first reason; second reason\n\
             https://example.com/jobs/1"
        );
    }

    #[test]
    fn alert_omits_absent_and_blank_fields() {
        assert_eq!(format_alert(&json!({})), "\u{1f4cc} Job Match (0): \n");
        assert_eq!(
            format_alert(&json!({
                "title": "Analyst",
                "company": "",
                "role_profile": "",
                "source": "",
                "reasons": [],
                "link": "https://example/1"
            })),
            "\u{1f4cc} Job Match (0): Analyst\nhttps://example/1"
        );
    }

    #[test]
    fn whitelist_priority_is_only_shown_when_not_shortlisted() {
        let base = json!({"title": "Analyst", "company_control": "whitelist", "link": "u"});
        assert!(format_alert(&base).contains("Priority: company whitelist"));

        let shortlisted = json!({
            "title": "Analyst", "shortlisted": true, "company_control": "whitelist", "link": "u"
        });
        let text = format_alert(&shortlisted);
        assert!(text.contains("Priority: shortlisted employer"));
        assert!(!text.contains("company whitelist"));

        let neither = json!({"title": "Analyst", "company_control": "none", "link": "u"});
        assert!(!format_alert(&neither).contains("Priority:"));
    }

    #[test]
    fn queue_alerts_skips_already_alerted_pending_and_linkless_matches() {
        let mut state = json!({
            "alerted_links": ["https://example/sent"],
            "pending_alerts": [{"link": "https://example/queued"}]
        });
        let queued = queue_alerts(
            &mut state,
            &[
                json!({"link": "https://example/sent"}),
                json!({"link": "https://example/queued"}),
                json!({"link": "", "title": "blank"}),
                json!({"title": "no link key"}),
                json!({"link": "https://example/new", "title": "New"}),
                json!({"link": "https://example/new", "title": "Repeat in this batch"}),
            ],
        );
        assert_eq!(queued, 2);
        let pending = state["pending_alerts"].as_array().unwrap();
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[1]["title"], "New");
        assert_eq!(pending[2]["title"], "Repeat in this batch");

        // Nothing to add leaves the queue untouched.
        assert_eq!(queue_alerts(&mut state, &[]), 0);
        assert_eq!(state["pending_alerts"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_caption_over_the_limit_gives_up_body_before_the_link() {
        let link = "https://example.com/jobs/1";
        let text = format!("{}\n{link}", "x".repeat(100));
        // Inside the limit nothing is touched.
        assert_eq!(caption(&text, 1024), text);

        // Exactly at the limit is not truncation; one character over is.
        let exact = format!(
            "{}
{link}",
            "y".repeat(1024 - link.chars().count() - 1)
        );
        assert_eq!(exact.chars().count(), 1024);
        assert_eq!(caption(&exact, 1024), exact);
        let over = caption(&format!("z{exact}"), 1024);
        assert_eq!(over.chars().count(), 1024);
        assert!(over.ends_with(link), "{over}");

        let trimmed = caption(&text, 60);
        assert_eq!(trimmed.chars().count(), 60);
        assert!(trimmed.ends_with(link), "{trimmed}");
        assert!(trimmed.contains('\u{2026}'), "{trimmed}");

        // Once only the link fits, only the link is sent.
        assert_eq!(caption(&text, 26), link);
        assert_eq!(caption(&text, 10), "https://ex");
        // A caption with no link at all is simply cut.
        assert_eq!(caption("abcdef", 3), "abc");
    }

    #[test]
    fn photo_url_takes_the_first_embedded_image() {
        // Shape a real We Work Remotely item arrives in, tags already stripped.
        let alert = json!({"description": "img src=\"https://we-work-remotely.imgix.net/logos/0171/logo.gif?ixlib=rails w=50\" / p Headquarters: /p"});
        assert_eq!(
            photo_url(&alert),
            Some("https://we-work-remotely.imgix.net/logos/0171/logo.gif")
        );
        assert_eq!(photo_url(&json!({"description": "no pictures here"})), None);
        assert_eq!(photo_url(&json!({})), None);
    }

    #[test]
    fn photo_payload_moves_the_text_into_a_caption() {
        let payload = photo_payload("chat", "7", "https://img/x.png", "line\nline");
        assert_eq!(
            payload,
            json!({
                "chat_id": "chat",
                "disable_web_page_preview": true,
                "message_thread_id": "7",
                "photo": "https://img/x.png",
                "caption": "line\nline"
            })
        );
        // Telegram rejects captions past 1024 characters.
        let long = photo_payload("chat", "", "https://img/x.png", &"x".repeat(2000));
        assert_eq!(long["caption"].as_str().expect("caption").len(), 1024);
    }

    #[test]
    fn deliver_alerts_is_a_no_op_with_an_empty_queue() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::open(dir.path().join("roleforge.sqlite3")).unwrap();
        let mut state = json!({"pending_alerts": [], "alerted_links": []});
        assert_eq!(deliver_alerts(&mut db, &mut state, "t"), (0, String::new()));
        assert!(state.get("last_delivery_utc").is_none());
    }

    #[test]
    fn settings_read_the_three_environment_variables() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded test, no worker threads are running.
        unsafe {
            env::set_var("TELEGRAM_BOT_TOKEN", "  token  ");
            env::set_var("TELEGRAM_CHAT_ID", " chat ");
            env::set_var("TELEGRAM_THREAD_ID", " 7 ");
        }
        assert_eq!(
            settings(),
            ("token".to_owned(), "chat".to_owned(), "7".to_owned())
        );

        unsafe {
            env::remove_var("TELEGRAM_BOT_TOKEN");
            env::remove_var("TELEGRAM_CHAT_ID");
            env::remove_var("TELEGRAM_THREAD_ID");
        }
        assert_eq!(settings(), (String::new(), String::new(), String::new()));

        // Credentials are required before anything is sent.
        assert_eq!(
            send_message("hello", None).unwrap_err().to_string(),
            "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be configured"
        );
        unsafe { env::set_var("TELEGRAM_CHAT_ID", "chat") };
        assert_eq!(
            send_message("hello", None).unwrap_err().to_string(),
            "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be configured"
        );
        unsafe {
            env::remove_var("TELEGRAM_CHAT_ID");
        }
    }

    #[test]
    fn message_payload_carries_the_thread_and_markup_only_when_present() {
        assert_eq!(
            message_payload("chat", "", "hello", None),
            json!({"chat_id": "chat", "text": "hello", "disable_web_page_preview": true})
        );
        assert_eq!(
            message_payload("chat", "7", "hello", Some(json!({"inline_keyboard": []}))),
            json!({
                "chat_id": "chat", "text": "hello", "disable_web_page_preview": true,
                "message_thread_id": "7", "reply_markup": {"inline_keyboard": []}
            })
        );
    }

    #[test]
    fn polling_needs_a_configured_token() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock keeps every environment-mutating test serialized.
        unsafe { env::remove_var("TELEGRAM_BOT_TOKEN") };
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("state.sqlite3")).unwrap();
        assert_eq!(
            process_updates(&db, 25).unwrap_err().to_string(),
            "TELEGRAM_BOT_TOKEN not configured"
        );
    }
}
