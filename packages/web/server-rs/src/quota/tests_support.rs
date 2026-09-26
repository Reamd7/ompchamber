//! Shared fakes for quota tests (mirrors the JS test seams: stubbed `fetch`,
//! mocked `readAuthFile`, fixed `Date.now`, temp `OMPCHAMBER_DATA_DIR`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpFetch, HttpRequest, HttpResponse};
use serde_json::Value;

pub const FIXED_NOW: u64 = 1_780_000_000_000;

pub fn unused_http() -> HttpFetch {
    Arc::new(|_request: HttpRequest| {
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
        })
    })
}

pub fn json_response(body: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

pub fn status_response(status: u16, headers: Vec<(&str, &str)>, body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.to_vec(),
    }
}

/// Scripted transport: pops queued responses in order (or 200 `{}`), logging
/// every request for assertions.
#[derive(Clone, Default)]
pub struct FakeHttp {
    pub requests: Arc<Mutex<Vec<HttpRequest>>>,
    pub responses: Arc<Mutex<Vec<HttpResponse>>>,
    pub urls: Arc<Mutex<Vec<String>>>,
}

impl FakeHttp {
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
            urls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn transport(&self) -> HttpFetch {
        let this = self.clone();
        Arc::new(move |request: HttpRequest| {
            let fake = this.clone();
            Box::pin(async move {
                fake.requests.lock().unwrap().push(request.clone());
                fake.urls.lock().unwrap().push(request.url.clone());
                let response = fake
                    .responses
                    .lock()
                    .unwrap()
                    .if_empty_swap(|| json_response(Value::Null));
                Ok(response)
            })
        })
    }
}

trait IfEmptySwap {
    fn if_empty_swap(&mut self, make: impl FnOnce() -> HttpResponse) -> HttpResponse;
}

impl IfEmptySwap for Vec<HttpResponse> {
    fn if_empty_swap(&mut self, make: impl FnOnce() -> HttpResponse) -> HttpResponse {
        if self.is_empty() {
            make()
        } else {
            self.remove(0)
        }
    }
}

pub struct TestEnv {
    pub auth: Arc<Mutex<Value>>,
    pub env_vars: Arc<Mutex<HashMap<String, String>>>,
    pub home: std::path::PathBuf,
    pub keychain: Arc<Mutex<Option<String>>>,
    pub sqlite: Arc<Mutex<HashMap<String, String>>>,
    pub written_auth: Arc<Mutex<Vec<Value>>>,
    _guard: TempDirGuard,
}

pub struct TempDirGuard(pub std::path::PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl TestEnv {
    pub fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            + COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ompchamber-quota-test-{unique}"));
        std::fs::create_dir_all(&home).unwrap();
        let guard = TempDirGuard(home.clone());
        let env_vars: HashMap<String, String> = [(
            "OMPCHAMBER_DATA_DIR".to_string(),
            home.join("data").to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect();
        Self {
            auth: Arc::new(Mutex::new(Value::Null)),
            env_vars: Arc::new(Mutex::new(env_vars)),
            home,
            keychain: Arc::new(Mutex::new(None)),
            sqlite: Arc::new(Mutex::new(HashMap::new())),
            written_auth: Arc::new(Mutex::new(Vec::new())),
            _guard: guard,
        }
    }

    pub fn set_env(&self, name: &str, value: &str) {
        self.env_vars
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
    }

    pub fn set_auth(&self, auth: Value) {
        *self.auth.lock().unwrap() = auth;
    }

    pub fn set_keychain(&self, raw: Option<String>) {
        *self.keychain.lock().unwrap() = raw;
    }

    pub fn deps(&self, http: HttpFetch) -> QuotaDeps {
        self.deps_with_now(http, FIXED_NOW)
    }

    pub fn deps_with_now(&self, http: HttpFetch, now: u64) -> QuotaDeps {
        let auth = self.auth.clone();
        let auth_for_write = self.auth.clone();
        let env_vars = self.env_vars.clone();
        let home = self.home.clone();
        let keychain = self.keychain.clone();
        let sqlite = self.sqlite.clone();
        let written_auth = self.written_auth.clone();
        QuotaDeps {
            http,
            now: Arc::new(move || now),
            env: Arc::new(move |name| env_vars.lock().unwrap().get(name).cloned()),
            home_dir: Arc::new(move || Some(home.clone())),
            read_auth: Arc::new(move || {
                let value = auth.lock().unwrap().clone();
                match value {
                    Value::Null => Ok(Value::Object(serde_json::Map::new())),
                    other => Ok(other),
                }
            }),
            write_auth: Arc::new(move |new_auth| {
                written_auth.lock().unwrap().push(new_auth.clone());
                *auth_for_write.lock().unwrap() = new_auth.clone();
                Ok(())
            }),
            keychain: Arc::new(move || keychain.lock().unwrap().clone()),
            sqlite_value: Arc::new(move |db, query| {
                // Fake `sqlite3 -json`: answer by the quoted ItemTable key.
                let quoted = query.split("key = '").nth(1)?.split('\'').next()?;
                let lookup = format!("{}\0{}", db.to_string_lossy(), quoted);
                sqlite.lock().unwrap().get(&lookup).cloned()
            }),
        }
    }
}
