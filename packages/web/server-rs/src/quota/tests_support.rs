//! Shared fakes for quota tests (mirrors the JS test seams: stubbed `fetch`,
//! mocked `readAuthFile`, fixed `Date.now`, temp `OMPCHAMBER_DATA_DIR`).
//!
//! 中文说明：quota 测试共享的 fake 设施工具，与 JS 测试 seam 一一对应：
//! 可脚本化的 `fetch` stub、可 mock 的 `readAuthFile`/`writeAuthFile`、
//! 固定的 `Date.now`、指向临时目录的 `OMPCHAMBER_DATA_DIR`，以及假的
//! Keychain 与 sqlite 查询。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpFetch, HttpRequest, HttpResponse};
use serde_json::Value;

/// 测试用的固定时钟（epoch 毫秒）；未显式指定时间时 `TestEnv::deps`
/// 注入该值，保证缓存/重置计算可复现。
pub const FIXED_NOW: u64 = 1_780_000_000_000;

/// 永不该被调用的 HTTP fake：任何请求都回 200 `{}`，供不碰网络的测试占位。
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

/// 构造 200 响应：body 为 `body` 序列化的 JSON，`Content-Type` 为
/// `application/json`。
pub fn json_response(body: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

/// 按指定状态码、响应头（`&str` 二元组列表）与原始 body 构造响应，
/// 供脚本化错误路径（401/429 等）使用。
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
///
/// 中文说明：脚本化传输 fake：按顺序弹出预置响应队列（耗尽后回 200
/// `null` JSON），同时把每个请求完整记录下来供断言。
#[derive(Clone, Default)]
pub struct FakeHttp {
    /// 收到的全部请求（方法、URL、header、body），按到达顺序。
    pub requests: Arc<Mutex<Vec<HttpRequest>>>,
    /// 预置响应队列；每次请求弹出队首。
    pub responses: Arc<Mutex<Vec<HttpResponse>>>,
    /// 收到的请求 URL 列表（`requests` 的轻量投影，便于断言）。
    pub urls: Arc<Mutex<Vec<String>>>,
}

/// [`FakeHttp`] 的构造与 [`HttpFetch`] 适配。
impl FakeHttp {
    /// 以按序消费的响应队列创建 fake。
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
            urls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 把 fake 包装成 [`HttpFetch`] 供 [`QuotaDeps`] 注入；内部克隆共享
    /// 状态，记录请求并弹出下一个预置响应。
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

/// 内部辅助 trait：为响应队列提供"空则现场生成、否则弹出队首"的语义。
trait IfEmptySwap {
    /// 队列为空时调用 `make` 现场构造一个响应，否则移除并返回队首元素。
    fn if_empty_swap(&mut self, make: impl FnOnce() -> HttpResponse) -> HttpResponse;
}

/// [`IfEmptySwap`] 在响应向量上的实现。
impl IfEmptySwap for Vec<HttpResponse> {
    /// 空队列调用 `make()` 兜底，非空则 `remove(0)` 弹出队首。
    fn if_empty_swap(&mut self, make: impl FnOnce() -> HttpResponse) -> HttpResponse {
        if self.is_empty() {
            make()
        } else {
            self.remove(0)
        }
    }
}

/// 每个测试一份的隔离环境：临时 home 目录（Drop 时自动清理）、内存中的
/// auth.json/环境变量/Keychain/sqlite 状态，以及记录 auth 写入历史的缓冲。
pub struct TestEnv {
    /// 内存中的 auth.json 内容；`Value::Null` 表示"文件不存在"语义
    /// （读取时映射为空对象）。
    pub auth: Arc<Mutex<Value>>,
    /// 内存中的环境变量表（`OMPCHAMBER_DATA_DIR` 默认指向临时 home 下的
    /// `data` 目录）。
    pub env_vars: Arc<Mutex<HashMap<String, String>>>,
    /// 每个测试独享的临时 home 目录，由 [`TempDirGuard`] 负责清理。
    pub home: std::path::PathBuf,
    /// 假 Keychain 读取结果（`None` 模拟无凭据/非 macOS）。
    pub keychain: Arc<Mutex<Option<String>>>,
    /// 假 sqlite 查询表：键为 `db路径\0key`，值为查询返回的 `value`。
    pub sqlite: Arc<Mutex<HashMap<String, String>>>,
    /// `write_auth` 的调用历史（按序记录每次写回的完整 JSON）。
    pub written_auth: Arc<Mutex<Vec<Value>>>,
    /// RAII 守卫：持有它即持有临时目录的清理责任（字段本身不可访问）。
    _guard: TempDirGuard,
}

/// RAII 临时目录守卫：包装目录路径，Drop 时递归删除该目录。
pub struct TempDirGuard(pub std::path::PathBuf);
/// Drop 时递归删除守卫持有的临时目录，失败静默忽略。
impl Drop for TempDirGuard {
    /// 删除临时目录（`let _` 吞掉错误：测试收尾不因清理失败而 panic）。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// [`TestEnv`] 的构造、状态注入与 [`QuotaDeps`] 装配。
impl TestEnv {
    /// 创建独享临时 home 目录的测试环境，默认把 `OMPCHAMBER_DATA_DIR`
    /// 指向 `<home>/data`。
    pub fn new() -> Self {
        // 进程内递增计数器，保证并发测试的临时目录名互不冲突。
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

    /// 覆盖一个环境变量（等价于 `process.env[name] = value`）。
    pub fn set_env(&self, name: &str, value: &str) {
        self.env_vars
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
    }

    /// 覆盖内存中的 auth.json 内容。
    pub fn set_auth(&self, auth: Value) {
        *self.auth.lock().unwrap() = auth;
    }

    /// 覆盖假 Keychain 的读取结果。
    pub fn set_keychain(&self, raw: Option<String>) {
        *self.keychain.lock().unwrap() = raw;
    }

    /// 用固定时钟 [`FIXED_NOW`] 与指定 HTTP fake 装配 [`QuotaDeps`]。
    pub fn deps(&self, http: HttpFetch) -> QuotaDeps {
        self.deps_with_now(http, FIXED_NOW)
    }

    /// 以自定义时钟毫秒值装配 [`QuotaDeps`]：read_auth 把 Null 映射为
    /// 空对象，write_auth 同时记录历史并更新当前值，sqlite 查询按
    /// `db\0key` 查表。
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
