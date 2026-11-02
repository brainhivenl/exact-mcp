use std::collections::VecDeque;
use std::path::Path;

use reqwest::header::HeaderValue;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::*;
use crate::store::Tokens;

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    change: Option<Change>,
}

impl Reply {
    fn ok(body: &str) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            change: None,
        }
    }

    fn limited(value: &str) -> Self {
        Self {
            status: 429,
            headers: vec![("Retry-After", value.into())],
            body: "old-access private-financial-data".into(),
            change: None,
        }
    }
}

enum Change {
    Administration,
    ExpireToken,
}

struct Server {
    base: Url,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Vec<String>>,
}

impl Server {
    async fn new(directory: &Path, replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (stop, mut stopped) = oneshot::channel();
        let directory = directory.to_owned();
        let task = tokio::spawn(async move {
            let mut replies: VecDeque<_> = replies.into();
            let mut requests = Vec::new();
            loop {
                let (mut stream, _) = tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => accepted.unwrap(),
                };
                let mut request = Vec::new();
                let length = loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        let headers = String::from_utf8(request.clone()).unwrap();
                        break headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                    }
                };
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                request.extend(body);
                requests.push(String::from_utf8(request).unwrap());
                let reply = replies.pop_front().expect("Unexpected extra request");
                if let Some(change) = reply.change {
                    let store = Store::open(&directory).await.unwrap();
                    let mut session = store.require_session().await.unwrap();
                    match change {
                        Change::Administration => {
                            session.administration.as_mut().unwrap().code = 43
                        }
                        Change::ExpireToken => session.tokens.expires_at = 0,
                    }
                    store.save(&session).await.unwrap();
                }
                let mut response = format!(
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in reply.headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                response.push_str(&reply.body);
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            assert!(replies.is_empty(), "Not all expected requests were sent");
            requests
        });
        Self { base, stop, task }
    }

    async fn finish(self) -> Vec<String> {
        self.stop.send(()).unwrap();
        self.task.await.unwrap()
    }
}

async fn fixture(replies: Vec<Reply>) -> (TempDir, Server, ExactClient) {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let server = Server::new(directory.path(), replies).await;
    let store = Store::open(directory.path()).await.unwrap();
    store
        .save(&Session {
            base_url: server.base.to_string(),
            client_id: "client-id".into(),
            redirect_uri: "https://localhost:8400/callback".into(),
            tokens: Tokens {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                expires_at: oauth::now().unwrap() + 3600,
            },
            administration: Some(Administration {
                code: 42,
                description: "Company".into(),
            }),
        })
        .await
        .unwrap();
    drop(store);
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let mut client = ExactClient::new(directory.path().into(), http, Some("secret".into()))
        .await
        .unwrap();
    client.pin_administration().unwrap();
    (directory, server, client)
}

fn api_url(client: &ExactClient) -> Url {
    client.base_url.join("/api/v1/42/report/Rows").unwrap()
}

fn headers(values: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        headers.insert(*name, HeaderValue::from_str(value).unwrap());
    }
    headers
}

#[test]
fn retry_after_seconds_http_dates_and_invalid_values() {
    let now = UNIX_EPOCH + Duration::from_secs(784111777);
    for value in [
        "3",
        "Sun, 06 Nov 1994 08:49:40 GMT",
        "Sunday, 06-Nov-94 08:49:40 GMT",
        "Sun Nov  6 08:49:40 1994",
    ] {
        assert_eq!(
            retry_after_delay(value, now),
            Some(Duration::from_secs(3)),
            "{value}"
        );
    }
    assert_eq!(
        retry_after_delay("Sun, 06 Nov 1994 08:49:37 GMT", now),
        Some(Duration::ZERO)
    );
    for value in [
        "",
        "-1",
        "+1",
        "1.5",
        "nonsense",
        "9999999999999999999999999",
    ] {
        assert_eq!(retry_after_delay(value, now), None, "{value}");
    }
    assert_eq!(
        rate_limit_delay(&HeaderMap::new(), now, 0),
        Duration::from_secs(1)
    );
    assert_eq!(
        rate_limit_delay(&headers(&[("retry-after", "broken")]), now, 1),
        Duration::from_secs(2)
    );
}

#[test]
fn exact_reset_headers_are_milliseconds_and_only_exhausted_windows_apply() {
    let now = UNIX_EPOCH + Duration::from_secs(1000);
    for reset in ["x-ratelimit-reset", "x-ratelimit-minutely-reset"] {
        assert_eq!(
            rate_limit_delay(&headers(&[(reset, "1001500")]), now, 0),
            Duration::from_millis(1500)
        );
        assert_eq!(
            rate_limit_delay(&headers(&[(reset, "999000")]), now, 0),
            Duration::ZERO
        );
    }
    assert_eq!(
        rate_limit_delay(
            &headers(&[
                ("retry-after", "2"),
                ("x-ratelimit-reset", "999999999"),
                ("x-ratelimit-remaining", "5"),
                ("x-ratelimit-minutely-reset", "1003000"),
                ("x-ratelimit-minutely-remaining", "0"),
            ]),
            now,
            0
        ),
        Duration::from_secs(3)
    );
    assert_eq!(
        rate_limit_delay(
            &headers(&[
                ("retry-after", "5"),
                ("x-ratelimit-minutely-reset", "1003000"),
            ]),
            now,
            0
        ),
        Duration::from_secs(5)
    );
    for value in ["-1", "broken", "99999999999999999999999999999"] {
        assert_eq!(
            rate_limit_delay(&headers(&[("x-ratelimit-minutely-reset", value)]), now, 0),
            Duration::from_secs(1)
        );
    }
    assert!(
        rate_limit_delay(
            &headers(&[("x-ratelimit-reset", "18446744073709551615")]),
            now,
            0
        ) > MAX_RATE_LIMIT_WAIT
    );
}

#[tokio::test]
async fn json_xml_and_pdf_share_bounded_get_retries() {
    for kind in ["json", "xml", "pdf"] {
        let body = match kind {
            "json" => "{\"d\":[1,2]}",
            "xml" => "<MatchSets/>",
            _ => "%PDF-test",
        };
        let (_directory, server, client) =
            fixture(vec![Reply::limited("0"), Reply::ok(body)]).await;
        match kind {
            "json" => assert_eq!(
                client
                    .get_collection::<u32>(api_url(&client))
                    .await
                    .unwrap(),
                vec![1, 2]
            ),
            "xml" => assert_eq!(
                client.get_match_sets_page(42, None, 100).await.unwrap(),
                body.as_bytes()
            ),
            _ => assert_eq!(
                client
                    .get_invoice_pdf_bytes(42, uuid::Uuid::nil(), 100)
                    .await
                    .unwrap(),
                body.as_bytes()
            ),
        }
        let requests = server.finish().await;
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
        assert_eq!(requests[0], requests[1]);
    }
}

#[tokio::test]
async fn rate_limit_exhaustion_is_bounded_and_does_not_leak() {
    let (_directory, server, client) = fixture(vec![
        Reply::limited("0"),
        Reply::limited("0"),
        Reply::limited("0"),
    ])
    .await;
    let error = client
        .get_collection::<Value>(api_url(&client))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Exact API rate limit reached (HTTP 429); wait before trying again"
    );
    assert!(!format!("{error:#}").contains("old-access"));
    assert!(!format!("{error:#}").contains("private-financial-data"));
    assert_eq!(server.finish().await.len(), 3);
}

#[tokio::test]
async fn excessive_wait_hints_fail_without_sleeping_or_retrying() {
    for (name, value) in [
        ("Retry-After", "61"),
        ("Retry-After", "18446744073709551615"),
        ("Retry-After", "Fri, 01 Jan 2100 00:00:00 GMT"),
        ("X-RateLimit-Reset", "18446744073709551615"),
        ("X-RateLimit-Minutely-Reset", "4102444800000"),
    ] {
        let mut reply = Reply::limited("0");
        reply.headers = vec![(name, value.into())];
        let (_directory, server, client) = fixture(vec![reply]).await;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client.get_collection::<Value>(api_url(&client)),
        )
        .await
        .unwrap();
        assert!(result.unwrap_err().to_string().contains("HTTP 429"));
        assert_eq!(server.finish().await.len(), 1);
    }
}

#[tokio::test]
async fn arbitrary_errors_redirects_and_unauthorized_are_not_retried() {
    for status in [401, 403, 500, 302] {
        let mut reply = Reply::limited("0");
        reply.status = status;
        reply
            .headers
            .push(("Location", "http://127.0.0.1:1/stolen".into()));
        let (_directory, server, client) = fixture(vec![reply]).await;
        let error = client
            .get_collection::<Value>(api_url(&client))
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(!error.contains("old-access"));
        assert!(!error.contains("private-financial-data"));
        assert!(!error.contains("127.0.0.1"));
        assert!(error.contains(if status == 401 {
            "rejected authorization"
        } else {
            "HTTP"
        }));
        assert_eq!(server.finish().await.len(), 1);
    }
}

#[tokio::test]
async fn retry_revalidates_scope_and_refreshes_expired_tokens() {
    let mut reply = Reply::limited("0");
    reply.change = Some(Change::Administration);
    let (_directory, server, client) = fixture(vec![reply]).await;
    assert!(
        client
            .get_collection::<Value>(api_url(&client))
            .await
            .unwrap_err()
            .to_string()
            .contains("administration changed")
    );
    assert_eq!(server.finish().await.len(), 1);

    let mut reply = Reply::limited("0");
    reply.change = Some(Change::ExpireToken);
    let tokens = json!({"access_token":"new-access", "refresh_token":"new-refresh", "token_type":"bearer", "expires_in":600}).to_string();
    let (_directory, server, client) =
        fixture(vec![reply, Reply::ok(&tokens), Reply::ok("{\"d\":[1]}")]).await;
    assert_eq!(
        client
            .get_collection::<u32>(api_url(&client))
            .await
            .unwrap(),
        vec![1]
    );
    let requests = server.finish().await;
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("Bearer old-access"));
    assert!(requests[1].starts_with("POST /api/oauth2/token "));
    assert!(requests[2].contains("Bearer new-access"));
}

#[tokio::test]
async fn token_exchange_429_is_never_retried() {
    let mut reply = Reply::limited("0");
    reply.change = Some(Change::ExpireToken);
    let (_directory, server, client) = fixture(vec![reply, Reply::limited("0")]).await;
    let error = client
        .get_collection::<Value>(api_url(&client))
        .await
        .unwrap_err();
    assert!(!format!("{error:#}").contains("private-financial-data"));
    let requests = server.finish().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET "));
    assert!(requests[1].starts_with("POST /api/oauth2/token "));
}

#[tokio::test]
async fn visitor_streams_pages_and_preserves_window_behavior() {
    let first = "{\"d\":{\"results\":[1,2],\"__next\":\"?page=2\"}}";
    let second = "{\"d\":{\"results\":[3]}}";
    let (_directory, server, client) = fixture(vec![
        Reply::ok(first),
        Reply::limited("0"),
        Reply::ok(second),
    ])
    .await;
    let mut pages = Vec::new();
    client
        .try_for_each_page(api_url(&client), |rows: Vec<u32>| {
            pages.push(rows);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(pages, vec![vec![1, 2], vec![3]]);
    assert_eq!(server.finish().await.len(), 3);

    let (_directory, server, client) = fixture(vec![Reply::ok(first)]).await;
    assert_eq!(
        client
            .get_collection_window::<u32>(api_url(&client), 1, 1)
            .await
            .unwrap(),
        vec![2]
    );
    assert_eq!(server.finish().await.len(), 1);
}

#[tokio::test]
async fn visitor_and_collection_propagate_errors_without_partial_success() {
    let first = "{\"d\":{\"results\":[1],\"__next\":\"?page=2\"}}";
    let (_directory, server, client) = fixture(vec![Reply::ok(first)]).await;
    let error = client
        .try_for_each_page(api_url(&client), |_: Vec<u32>| {
            bail!("Visitor rejected page")
        })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Visitor rejected page");
    assert_eq!(server.finish().await.len(), 1);
    for visitor in [false, true] {
        let (_directory, server, client) =
            fixture(vec![Reply::ok(first), Reply::ok("invalid")]).await;
        let result = if visitor {
            client
                .try_for_each_page(api_url(&client), |_: Vec<u32>| Ok(()))
                .await
        } else {
            client
                .get_collection::<u32>(api_url(&client))
                .await
                .map(|_| ())
        };
        assert_eq!(
            result.unwrap_err().to_string(),
            "Invalid Exact API response"
        );
        assert_eq!(server.finish().await.len(), 2);
    }
}

#[tokio::test]
async fn visitor_rejects_pagination_loops_and_outside_resources() {
    for next in [
        "/api/v1/42/report/Rows",
        "/api/v1/43/report/Rows",
        "http://127.0.0.1:1/api/v1/42/report/Rows",
        "http://user:secret@127.0.0.1/api/v1/42/report/Rows",
        "/api/v1/42/report/Rows#fragment",
        "/docs/XMLDownload.aspx",
    ] {
        let body = json!({"d":{"results":[1],"__next":next}}).to_string();
        let (_directory, server, client) = fixture(vec![Reply::ok(&body)]).await;
        let error = client
            .try_for_each_page(api_url(&client), |_: Vec<u32>| Ok(()))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(if next == "/api/v1/42/report/Rows" {
                    "pagination loop"
                } else {
                    "outside this account's API resource"
                })
        );
        assert_eq!(server.finish().await.len(), 1);
    }
}

#[test]
fn cumulative_wait_budget_and_attempt_limit_are_enforced() {
    let now = UNIX_EPOCH + Duration::from_secs(1000);
    let forty = headers(&[("retry-after", "40")]);
    let twenty = headers(&[("retry-after", "20")]);
    let twenty_one = headers(&[("retry-after", "21")]);
    assert_eq!(
        bounded_rate_limit_delay(&forty, now, 0, Duration::ZERO),
        Some(Duration::from_secs(40))
    );
    assert_eq!(
        bounded_rate_limit_delay(&twenty, now, 1, Duration::from_secs(40)),
        Some(Duration::from_secs(20))
    );
    assert_eq!(
        bounded_rate_limit_delay(&twenty_one, now, 1, Duration::from_secs(40)),
        None
    );
    assert_eq!(
        bounded_rate_limit_delay(&headers(&[("retry-after", "0")]), now, 2, Duration::ZERO),
        None
    );
    assert_eq!(
        bounded_rate_limit_delay(
            &headers(&[("retry-after", "1")]),
            now,
            1,
            MAX_RATE_LIMIT_WAIT
        ),
        None
    );
}

#[tokio::test]
async fn cancelling_the_retry_sleep_never_sends_or_refreshes() {
    use std::future::Future;
    use std::task::Poll;

    let (directory, server, client) = fixture(Vec::new()).await;
    let store = Store::open(directory.path()).await.unwrap();
    let mut session = store.require_session().await.unwrap();
    session.tokens.expires_at = 0;
    store.save(&session).await.unwrap();
    drop(store);
    tokio::time::pause();
    let mut retry = Box::pin(client.send_get_after(
        api_url(&client),
        "application/json",
        "Exact API request failed",
        Some(Duration::from_secs(60)),
    ));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(retry.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(retry);
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(server.finish().await.is_empty());
}

#[tokio::test]
async fn scope_changed_during_retry_sleep_prevents_the_next_request() {
    use std::future::Future;
    use std::task::Poll;

    let (directory, server, client) = fixture(Vec::new()).await;
    let store = Store::open(directory.path()).await.unwrap();
    let mut session = store.require_session().await.unwrap();
    drop(store);
    tokio::time::pause();
    let mut retry = Box::pin(client.send_get_after(
        api_url(&client),
        "application/json",
        "Exact API request failed",
        Some(Duration::from_secs(60)),
    ));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(retry.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    session.administration.as_mut().unwrap().code = 43;
    std::fs::write(
        directory.path().join("session.json"),
        serde_json::to_vec(&session).unwrap(),
    )
    .unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    let error = retry.await.unwrap_err();
    assert!(error.to_string().contains("administration changed"));
    assert!(server.finish().await.is_empty());
}
