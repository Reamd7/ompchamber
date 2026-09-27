//! Tests for the opencode_routes port: unit coverage of the shared.js
//! subset (markdown, JSONC layers) plus oneshot route tests over a temp
//! `OpenCodeEnv` and a mock omp-host engine.
//!
//! 中文概述：opencode_routes 移植的测试集——shared.js 子集的单元覆盖
//! （markdown frontmatter、JSONC 分层安全）+ 基于临时 OpenCodeEnv 与
//! mock omp-host 引擎的 oneshot 路由测试。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::routing::{any, get};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::config::{EngineConfig, ServerConfig};
use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::hub::EventHub;

use super::commands;
use super::config_layers as layers;
use super::mcp;
use super::providers;
use super::{OpenCodeEnv, router_with_env};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// 临时目录计数器：为并行测试生成互不冲突的目录名。
static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 在系统临时目录下创建唯一空目录（label + 进程 id + 自增序号）；
/// 若已存在则先递归删除，返回新目录路径。
fn temp_dir_unique(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ompchamber-ocr-{label}-{}-{}",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// 构造指向独立临时 HOME 的 [`OpenCodeEnv`]：config/data/home 三个目录
/// 均落在临时根之下，避免污染真实用户环境。
fn temp_env(label: &str) -> (OpenCodeEnv, PathBuf) {
    let root = temp_dir_unique(label);
    let env = OpenCodeEnv {
        config_dir: root.join(".config").join("opencode"),
        data_dir: root.join(".local").join("share").join("opencode"),
        home: root.join("home"),
        custom_config: None,
    };
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::create_dir_all(&env.data_dir).unwrap();
    std::fs::create_dir_all(&env.home).unwrap();
    (env, root)
}

/// 在 [`temp_env`] 基础上把 custom 层配置文件指到给定路径（模拟 omp custom 配置）。
fn env_with_custom(label: &str, custom: &Path) -> (OpenCodeEnv, PathBuf) {
    let (mut env, root) = temp_env(label);
    env.custom_config = Some(custom.to_path_buf());
    (env, root)
}

/// 写文件辅助：自动创建父目录，任一步失败即 panic。
fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// 读入并解析 JSON 文件，任一步失败即 panic。
fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// 构造测试用 [`RouterContext`]：端口 0、data 目录指向给定路径，
/// engine 默认指向 127.0.0.1:9（不可达，测试按需覆盖为 mock 引擎）。
fn test_context(data_dir: &Path) -> RouterContext {
    let config = ServerConfig {
        port: 0,
        host: None,
        lan: false,
        ui_password: None,
        api_only: false,
        data_dir: data_dir.to_path_buf(),
        dist_dir: data_dir.join("dist"),
        tunnel: Default::default(),
        engine: EngineConfig::Managed {
            hostname: "127.0.0.1".to_string(),
        },
    };
    RouterContext {
        config: Arc::new(config),
        engine: EngineState::external("http://127.0.0.1:9".to_string(), None),
        hub: EventHub::new(),
    }
}

/// 用给定 env 与 context 组装被测 Router（配合 oneshot 调用）。
fn test_app(env: OpenCodeEnv, ctx: RouterContext) -> Router {
    router_with_env(ctx, env)
}

/// 构造 HTTP 请求：带 body 时附上 Content-Type 与 Content-Length 头。
fn request_json(method: &str, uri: &str, body: Option<(&str, &str)>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((content, content_type)) = body {
        builder = builder
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, content.len());
        builder.body(Body::from(content.to_string())).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
}

/// oneshot 发送请求并收集（状态码, 解析后的 JSON body, 原始文本）三元组；
/// body 不是合法 JSON 时 JSON 位置为 `Value::Null`。
async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value, String) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let _content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).into_owned();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, json, text)
}

/// Mock omp-host: an axum server whose `/agent-dir` (and MCP callback)
/// behavior the test controls.
///
/// 中文：在本地起一个 axum 服务扮演 omp-host——`/agent-dir` 的返回值与
/// MCP 回调请求记录均可由测试动态控制。
struct MockEngine {
    /// mock 服务的 base URL（127.0.0.1 随机端口）。
    base_url: String,
    /// `/agent-dir` 端点的当前返回值；`None` 表示引擎未上报目录。
    agent_dir: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// 收到的 MCP 回调请求记录（标签, 完整路径含查询串）。
    requests: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// 服务任务句柄（随测试运行时回收，标记允许未读）。
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

/// mock 引擎的启动与状态控制。
impl MockEngine {
    /// 启动 mock 引擎：随机端口监听，注册 `/agent-dir` 与
    /// `/mcp/{name}/auth/callback` 两个端点；`agent_dir` 为初始上报目录。
    fn start(agent_dir: Option<String>) -> Self {
        let agent_dir = std::sync::Arc::new(std::sync::Mutex::new(agent_dir));
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let app_dir = agent_dir.clone();
        let app_requests = requests.clone();
        let app = Router::new()
            .route(
                "/agent-dir",
                get(move || {
                    let dir = app_dir.lock().unwrap().clone();
                    async move { axum::Json(json!({ "agentDir": dir })) }
                }),
            )
            .route(
                "/mcp/{name}/auth/callback",
                any(
                    move |axum::extract::Path(name): axum::extract::Path<String>,
                          uri: axum::http::Uri| {
                        let requests = app_requests.clone();
                        async move {
                            let path = format!(
                                "/mcp/{name}/auth/callback{}",
                                uri.query().map(|q| format!("?{q}")).unwrap_or_default()
                            );
                            requests
                                .lock()
                                .unwrap()
                                .push(("callback".to_string(), path));
                            axum::Json(json!({ "success": true }))
                        }
                    },
                ),
            );
        let handle = tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            agent_dir,
            requests,
            handle,
        }
    }

    /// 更新 `/agent-dir` 的返回值，模拟引擎切换 profile。
    fn set_agent_dir(&self, dir: Option<String>) {
        *self.agent_dir.lock().unwrap() = dir;
    }

    /// 生成指向本 mock 引擎的 [`RouterContext`]（复用 [`test_context`] 的目录布局）。
    fn context_with_engine(&self, data_dir: &Path) -> RouterContext {
        let mut ctx = test_context(data_dir);
        ctx.engine = EngineState::external(self.base_url.clone(), None);
        ctx
    }
}

/// 构造 engine 必然连接失败的 context：绑定端口后立即释放监听，
/// 之后连接一律被拒，用于测试引擎不可达时的回退路径。
fn unreachable_engine_context(data_dir: &Path) -> RouterContext {
    // Bind, learn the port, drop the listener: connections are refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut ctx = test_context(data_dir);
    ctx.engine = EngineState::external(format!("http://127.0.0.1:{port}"), None);
    ctx
}

// ---------------------------------------------------------------------------
// shared.js subset — parseMdFile / writeMdFile
// ---------------------------------------------------------------------------

/// 标准 agent markdown 样例：frontmatter 三字段 + 一段正文。
const STANDARD_MD: &str = "---\ndescription: My build agent\nmodel: anthropic/claude-sonnet-4\nmode: primary\n---\n\nThis is the prompt body.\n";

/// 验证标准 frontmatter（开闭 `---`）正确切分出字段与正文。
#[test]
fn parse_md_file_handles_standard_frontmatter() {
    let dir = temp_dir_unique("md");
    let file = dir.join("standard.md");
    write_file(&file, STANDARD_MD);
    let md = super::md_file::parse_md_file(&file).unwrap();
    assert_eq!(
        md.frontmatter.get("description"),
        Some(&json!("My build agent"))
    );
    assert_eq!(md.body, "This is the prompt body.");
}

/// 验证文件在 frontmatter 结束符处截断（无闭合换行）仍可解析且正文为空。
#[test]
fn parse_md_file_accepts_eof_closed_frontmatter() {
    let dir = temp_dir_unique("md");
    let file = dir.join("eof.md");
    write_file(
        &file,
        "---\ndescription: My build agent\nmodel: anthropic/claude-sonnet-4\n---",
    );
    let md = super::md_file::parse_md_file(&file).unwrap();
    assert_eq!(
        md.frontmatter.get("model"),
        Some(&json!("anthropic/claude-sonnet-4"))
    );
    assert_eq!(md.body, "");
}

/// 验证 CRLF 换行与 UTF-8 BOM 前缀均不影响解析。
#[test]
fn parse_md_file_accepts_crlf_and_bom() {
    let dir = temp_dir_unique("md");
    let crlf = dir.join("crlf.md");
    write_file(&crlf, &STANDARD_MD.replace('\n', "\r\n"));
    let md = super::md_file::parse_md_file(&crlf).unwrap();
    assert_eq!(md.frontmatter.get("mode"), Some(&json!("primary")));

    let bom = dir.join("bom.md");
    write_file(&bom, &format!("\u{feff}{STANDARD_MD}"));
    let md = super::md_file::parse_md_file(&bom).unwrap();
    assert_eq!(
        md.frontmatter.get("description"),
        Some(&json!("My build agent"))
    );
}

/// 验证宽松回退：普通标量值含冒号加空格时整段按字面字符串读取。
#[test]
fn parse_md_file_lenient_colon_fallback() {
    let dir = temp_dir_unique("md");
    let file = dir.join("colon.md");
    write_file(
        &file,
        "---\ndescription: Build agent: creates builds\nmodel: x\n---\n\nBody\n",
    );
    let md = super::md_file::parse_md_file(&file).unwrap();
    assert_eq!(
        md.frontmatter.get("description"),
        Some(&json!("Build agent: creates builds"))
    );
    assert_eq!(md.body, "Body");
}

/// 验证无 frontmatter 的纯正文文件解析为空映射加原文正文。
#[test]
fn parse_md_file_plain_body_without_frontmatter() {
    let dir = temp_dir_unique("md");
    let file = dir.join("plain.md");
    write_file(&file, "Just a prompt body.");
    let md = super::md_file::parse_md_file(&file).unwrap();
    assert!(md.frontmatter.is_empty());
    assert_eq!(md.body, "Just a prompt body.");
}

/// 验证写回只产生一个 frontmatter 块，且修改后的字段与正文可无损重读。
#[test]
fn write_md_file_round_trips_single_block() {
    let dir = temp_dir_unique("md");
    let file = dir.join("roundtrip.md");
    write_file(&file, STANDARD_MD);
    let mut md = super::md_file::parse_md_file(&file).unwrap();
    md.frontmatter.insert("model".into(), json!("openai/gpt-5"));
    super::md_file::write_md_file(&file, &md.frontmatter, &md.body).unwrap();

    let content = std::fs::read_to_string(&file).unwrap();
    // Exactly one frontmatter block: opener + closer lines (the JS test
    // counts `^---` matches, which only guards against a duplicated block).
    assert_eq!(content.lines().filter(|line| *line == "---").count(), 2);
    let reparsed = super::md_file::parse_md_file(&file).unwrap();
    assert_eq!(
        reparsed.frontmatter.get("model"),
        Some(&json!("openai/gpt-5"))
    );
    assert_eq!(reparsed.body, "This is the prompt body.");
}

// ---------------------------------------------------------------------------
// shared.js subset — JSONC layer safety
// ---------------------------------------------------------------------------

/// 合法 JSONC 样例：含注释、尾随逗号与嵌套配置节。
const VALID_JSONC: &str = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  // keep me\n  \"plugin\": [\"opencode-see-image\"],\n  \"mcp\": {\n    \"openproject\": {\n      \"type\": \"remote\",\n      \"url\": \"https://openproject.example.com/mcp\",\n      \"enabled\": true,\n    }\n  },\n  \"provider\": {\n    \"ollama-cloud\": {\n      \"npm\": \"@ai-sdk/openai-compatible\",\n      \"name\": \"Ollama Cloud\"\n    }\n  }\n}\n";

/// 非法 JSONC 样例：裸键名——安全加载器必须拒绝的部分解析形态。
const PARTIAL_PARSE_JSONC: &str = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  plugin: [\"opencode-see-image\"],\n  mcp: {\n    openproject: {\n      type: \"remote\"\n    }\n  }\n}\n";

/// 验证 JSONC 读取兼容注释与尾随逗号并保留语义。
#[test]
fn read_config_file_parses_jsonc_with_comments_and_trailing_commas() {
    let dir = temp_dir_unique("jsonc");
    let file = dir.join("opencode.jsonc");
    write_file(&file, VALID_JSONC);
    let config = layers::read_config_file(&file).unwrap();
    assert_eq!(config.get("plugin"), Some(&json!(["opencode-see-image"])));
    assert_eq!(
        config
            .get("mcp")
            .unwrap()
            .get("openproject")
            .unwrap()
            .get("type"),
        Some(&json!("remote"))
    );
}

/// 验证缺失、空白与纯注释的配置文件均读取为空对象而非报错。
#[test]
fn read_config_file_empty_and_comment_only_files_read_as_empty() {
    let dir = temp_dir_unique("jsonc");
    assert!(
        layers::read_config_file(&dir.join("missing.json"))
            .unwrap()
            .is_empty()
    );
    let empty = dir.join("empty.json");
    write_file(&empty, "   \n");
    assert!(layers::read_config_file(&empty).unwrap().is_empty());
    let comments = dir.join("comments.json");
    write_file(&comments, "// placeholder\n/* still empty */\n");
    assert!(layers::read_config_file(&comments).unwrap().is_empty());
}

/// 验证部分解析形态与非对象根一律拒绝，并带统一的“cannot be loaded safely”错误。
#[test]
fn read_config_file_rejects_partial_parse_and_non_object_roots() {
    let dir = temp_dir_unique("jsonc");
    let partial = dir.join("partial.json");
    write_file(&partial, PARTIAL_PARSE_JSONC);
    let error = layers::read_config_file(&partial).unwrap_err();
    assert!(error.contains("cannot be loaded safely"), "{error}");

    let array = dir.join("array.json");
    write_file(&array, "[\"plugin\"]\n");
    assert!(
        layers::read_config_file(&array)
            .unwrap_err()
            .contains("cannot be loaded safely")
    );
}

/// 验证写配置前先安全加载：不可解析文件拒绝写入且原样保留（不产生备份）；
/// 合法文件写入成功且旧内容落盘为备份。
#[test]
fn write_config_refuses_unparseable_files_and_keeps_valid_ones() {
    let dir = temp_dir_unique("jsonc");
    let file = dir.join("config.json");
    write_file(&file, PARTIAL_PARSE_JSONC);
    let mut config = serde_json::Map::new();
    config.insert("$schema".into(), json!("https://opencode.ai/config.json"));
    let error = layers::write_config(&config, &file).unwrap_err();
    assert!(error.contains("cannot be loaded safely"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), PARTIAL_PARSE_JSONC);
    assert!(!layers::backup_path(&file).exists());

    write_file(&file, VALID_JSONC);
    let mut config = layers::read_config_file(&file).unwrap();
    if let Some(Value::Object(mcp_section)) = config.get_mut("mcp") {
        if let Some(Value::Object(entry)) = mcp_section.get_mut("openproject") {
            entry.insert("enabled".into(), json!(false));
        }
    }
    layers::write_config(&config, &file).unwrap();
    let rewritten = read_json(&file);
    assert_eq!(
        rewritten.get("plugin"),
        Some(&json!(["opencode-see-image"]))
    );
    assert_eq!(
        rewritten
            .get("mcp")
            .unwrap()
            .get("openproject")
            .unwrap()
            .get("enabled"),
        Some(&json!(false))
    );
    assert_eq!(
        std::fs::read_to_string(layers::backup_path(&file)).unwrap(),
        VALID_JSONC
    );
}

/// 验证项目层不可解析时分层读取降级为空并记录 layer error，
/// MCP 变更只写 custom 层且不动项目文件。
#[test]
fn layers_keep_custom_readable_when_project_layer_is_unparseable() {
    let dir = temp_dir_unique("jsonc");
    let custom = dir.join("custom.jsonc");
    write_file(&custom, VALID_JSONC);
    let project_dir = dir.join("project");
    let project_file = project_dir.join(".opencode").join("opencode.jsonc");
    write_file(&project_file, PARTIAL_PARSE_JSONC);
    let (env, _root) = env_with_custom("jsonc", &custom);

    let config_layers = layers::read_config_layers(&env, Some(&project_dir)).unwrap();
    assert_eq!(
        config_layers.custom_config.get("plugin"),
        Some(&json!(["opencode-see-image"]))
    );
    assert!(config_layers.project_config.is_empty());
    assert_eq!(
        config_layers.merged_config.get("plugin"),
        Some(&json!(["opencode-see-image"]))
    );
    assert_eq!(config_layers.layer_errors.len(), 1);
    assert_eq!(config_layers.layer_errors[0].path, project_file);
    assert_eq!(config_layers.layer_errors[0].code, "INVALID_JSONC");

    // MCP mutation writes the custom layer and leaves the project file alone.
    mcp::update_mcp_config(
        &env,
        "openproject",
        &json!({ "enabled": false }),
        Some(&project_dir),
    )
    .unwrap();
    let rewritten = read_json(&custom);
    assert_eq!(
        rewritten
            .get("mcp")
            .unwrap()
            .get("openproject")
            .unwrap()
            .get("enabled"),
        Some(&json!(false))
    );
    assert_eq!(
        std::fs::read_to_string(&project_file).unwrap(),
        PARTIAL_PARSE_JSONC
    );
    assert!(!layers::backup_path(&project_file).exists());
}

/// 验证条目来源优先级：project 覆盖 user，仅存在于 user 的条目指向 user 文件，
/// 缺失条目的 exists 为假。
#[test]
fn entry_source_prefers_custom_then_project_then_user() {
    let dir = temp_dir_unique("layers");
    let project_dir = dir.join("project");
    write_file(
        &project_dir.join("opencode.json"),
        r#"{ "agent": { "shared": { "model": "project" }, "projOnly": {} } }"#,
    );
    let (env, _root) = temp_env("layers");
    write_file(
        &env.config_file(),
        r#"{ "agent": { "shared": { "model": "user" }, "userOnly": {} } }"#,
    );

    let config_layers = layers::read_config_layers(&env, Some(&project_dir)).unwrap();
    let shared = layers::get_json_entry_source(&config_layers, "agent", "shared").unwrap();
    assert!(shared.exists);
    assert_eq!(
        shared.path.as_deref(),
        Some(project_dir.join("opencode.json").as_path())
    );
    let user_only = layers::get_json_entry_source(&config_layers, "agent", "userOnly").unwrap();
    assert!(user_only.exists);
    assert_eq!(user_only.path.as_deref(), Some(env.config_file().as_path()));
    let missing = layers::get_json_entry_source(&config_layers, "agent", "nope").unwrap();
    assert!(!missing.exists);
}

// ---------------------------------------------------------------------------
// agents.js / commands.js library behavior
// ---------------------------------------------------------------------------

/// 验证更新 EOF 截断（无闭合换行）的 agent 文件后仍恰好一组 frontmatter
/// 分隔符，且未改字段与正文保留。
#[test]
fn update_agent_preserves_frontmatter_for_eof_closed_files() {
    let (env, root) = temp_env("agents");
    let project_dir = root.join("project");
    let agent_path = project_dir
        .join(".opencode")
        .join("agents")
        .join("strateg.md");
    write_file(
        &agent_path,
        "---\ndescription: Strategy agent\nmodel: anthropic/claude-sonnet-4\ntemperature: 0.7\n---",
    );

    let mut updates = serde_json::Map::new();
    updates.insert("model".into(), json!("openai/gpt-5"));
    super::agents::update_agent(&env, "strateg", &updates, Some(&project_dir)).unwrap();

    let content = std::fs::read_to_string(&agent_path).unwrap();
    assert_eq!(content.lines().filter(|line| *line == "---").count(), 2);
    let md = super::md_file::parse_md_file(&agent_path).unwrap();
    assert_eq!(md.frontmatter.get("model"), Some(&json!("openai/gpt-5")));
    assert_eq!(md.frontmatter.get("temperature"), Some(&json!(0.7)));
    assert_eq!(md.body, "");
}

/// 验证更新仅改写目标字段，无关 frontmatter 字段与正文原样保留。
#[test]
fn update_agent_preserves_unrelated_fields_and_body() {
    let (env, root) = temp_env("agents");
    let project_dir = root.join("project");
    let agent_path = project_dir
        .join(".opencode")
        .join("agents")
        .join("strateg.md");
    write_file(
        &agent_path,
        "---\ndescription: Strategy agent\nmode: primary\ntemperature: 0.7\n---\n\nBody of strateg.\n",
    );

    let mut updates = serde_json::Map::new();
    updates.insert("description".into(), json!("Updated strategy agent"));
    super::agents::update_agent(&env, "strateg", &updates, Some(&project_dir)).unwrap();

    let md = super::md_file::parse_md_file(&agent_path).unwrap();
    assert_eq!(
        md.frontmatter.get("description"),
        Some(&json!("Updated strategy agent"))
    );
    assert_eq!(md.frontmatter.get("mode"), Some(&json!("primary")));
    assert_eq!(md.body, "Body of strateg.");
}

/// 验证创建 agent 落盘 .md 文件；重复 .md 与重复 JSON 条目分别返回对应错误。
#[test]
fn create_agent_conflicts_and_scopes() {
    let (env, root) = temp_env("agents");
    let project_dir = root.join("project");

    let mut config = serde_json::Map::new();
    config.insert("description".into(), json!("D".to_string()));
    config.insert("prompt".into(), json!("P".to_string()));
    super::agents::create_agent(&env, "newbie", &config, Some(&project_dir), Some("project"))
        .unwrap();
    let written = project_dir
        .join(".opencode")
        .join("agents")
        .join("newbie.md");
    assert!(written.exists());
    let md = super::md_file::parse_md_file(&written).unwrap();
    assert_eq!(md.frontmatter.get("description"), Some(&json!("D")));
    assert_eq!(md.body, "P");

    // Duplicate .md (project level).
    let error =
        super::agents::create_agent(&env, "newbie", &config, Some(&project_dir), None).unwrap_err();
    assert_eq!(
        error,
        "Agent newbie already exists as project-level .md file"
    );

    // Duplicate JSON entry.
    write_file(&env.config_file(), r#"{ "agent": { "jsoned": {} } }"#);
    let error = super::agents::create_agent(&env, "jsoned", &serde_json::Map::new(), None, None)
        .unwrap_err();
    assert_eq!(error, "Agent jsoned already exists in opencode.json");
}

/// 验证按 scope 删除项目/用户 .md 与用户 JSON 条目；内置或不存在时报错。
#[test]
fn delete_agent_scopes_and_built_in_error() {
    let (env, root) = temp_env("agents");
    let project_dir = root.join("project");

    write_file(
        &project_dir.join(".opencode").join("agents").join("p.md"),
        "---\ndescription: p\n---\n\nbody\n",
    );
    super::agents::delete_agent(&env, "p", Some(&project_dir), Some("project")).unwrap();
    assert!(
        !project_dir
            .join(".opencode")
            .join("agents")
            .join("p.md")
            .exists()
    );

    write_file(
        &env.agent_dir().join("u.md"),
        "---\ndescription: u\n---\n\nbody\n",
    );
    super::agents::delete_agent(&env, "u", None, Some("user")).unwrap();
    assert!(!env.agent_dir().join("u.md").exists());

    // JSON entry in the user layer.
    write_file(
        &env.config_file(),
        r#"{ "agent": { "ju": { "model": "x" } } }"#,
    );
    super::agents::delete_agent(&env, "ju", None, Some("user")).unwrap();
    assert_eq!(read_json(&env.config_file()), json!({}));

    let error = super::agents::delete_agent(&env, "builtin", None, None).unwrap_err();
    assert_eq!(error, "Agent builtin is built-in or not deletable");
}

/// 验证权限更新写入定义层：user .md 直接改写；内置 agent 的覆盖写入 user JSON 层。
#[test]
fn agent_permission_updates_land_in_the_defining_layer() {
    let (env, root) = temp_env("agents");
    let project_dir = root.join("project");

    // Permission defined in the user .md: update rewrites that file.
    write_file(
        &env.agent_dir().join("perm.md"),
        "---\ndescription: perm\npermission:\n  edit:\n    commit: true\n---\n\nbody\n",
    );
    let mut updates = serde_json::Map::new();
    updates.insert("permission".into(), json!({ "edit": { "commit": false } }));
    super::agents::update_agent(&env, "perm", &updates, Some(&project_dir)).unwrap();
    let md = super::md_file::parse_md_file(&env.agent_dir().join("perm.md")).unwrap();
    assert_eq!(
        md.frontmatter.get("permission"),
        Some(&json!({ "edit": { "commit": false } }))
    );

    // Built-in override with no md/json fields: permission is created in the
    // user JSON layer.
    let mut updates = serde_json::Map::new();
    updates.insert("permission".into(), json!({ "bash": "allow" }));
    super::agents::update_agent(&env, "builtin-x", &updates, Some(&project_dir)).unwrap();
    let config = read_json(&env.config_file());
    assert_eq!(
        config.get("agent").unwrap().get("builtin-x"),
        Some(&json!({ "permission": { "bash": "allow" } }))
    );
}

/// 验证命令建/改/删全流程：.md 层与 JSON 层各自落位，缺失删除报错。
#[test]
fn command_crud_round_trip() {
    let (env, root) = temp_env("commands");
    let project_dir = root.join("project");

    let mut config = serde_json::Map::new();
    config.insert("description".into(), json!("Build".to_string()));
    config.insert("template".into(), json!("Run the build".to_string()));
    commands::create_command(&env, "build", &config, Some(&project_dir), Some("project")).unwrap();
    let written = project_dir
        .join(".opencode")
        .join("commands")
        .join("build.md");
    let md = super::md_file::parse_md_file(&written).unwrap();
    assert_eq!(md.frontmatter.get("description"), Some(&json!("Build")));
    assert_eq!(md.body, "Run the build");

    let mut updates = serde_json::Map::new();
    updates.insert("template".into(), json!("Updated".to_string()));
    commands::update_command(&env, "build", &updates, Some(&project_dir)).unwrap();
    let md = super::md_file::parse_md_file(&written).unwrap();
    assert_eq!(md.body, "Updated");
    assert_eq!(md.frontmatter.get("description"), Some(&json!("Build")));

    // JSON-defined command updates stay in the JSON layer.
    write_file(
        &env.config_file(),
        r#"{ "command": { "jsoncmd": { "template": "x" } } }"#,
    );
    let mut updates = serde_json::Map::new();
    updates.insert("template".into(), json!("y".to_string()));
    commands::update_command(&env, "jsoncmd", &updates, None).unwrap();
    let config = read_json(&env.config_file());
    assert_eq!(
        config.get("command").unwrap().get("jsoncmd"),
        Some(&json!({ "template": "y" }))
    );

    commands::delete_command(&env, "build", Some(&project_dir)).unwrap();
    assert!(!written.exists());
    let error = commands::delete_command(&env, "missing", None).unwrap_err();
    assert_eq!(error, "Command \"missing\" not found");
}

// ---------------------------------------------------------------------------
// mcp.js behavior
// ---------------------------------------------------------------------------

/// 验证 MCP 名称校验规则与条目归一化（丢 name/scope、标量字符串化、
/// trim 与默认 enabled）。
#[test]
fn mcp_name_validation_and_entry_normalization() {
    let (env, _root) = temp_env("mcp");
    assert_eq!(
        mcp::create_mcp_config(&env, "", &json!({}), None, None).unwrap_err(),
        "MCP server name is required"
    );
    let error = mcp::create_mcp_config(&env, "BadName", &json!({ "type": "local" }), None, None)
        .unwrap_err();
    assert_eq!(
        error,
        "MCP server name must be lowercase alphanumeric with hyphens/underscores"
    );

    let entry = mcp::build_mcp_entry(&json!({
        "name": "ignored",
        "scope": "ignored",
        "type": "local",
        "command": ["npx", 1, null],
        "url": "https://x",
        "enabled": false,
    }));
    assert_eq!(
        entry,
        json!({ "type": "local", "command": ["npx", "1", "null"], "enabled": false })
    );

    let entry = mcp::build_mcp_entry(&json!({
        "type": "remote",
        "url": "  https://example.com/mcp  ",
        "command": ["x"],
        "headers": { "X-A": " 1 ", "empty": "", "drop": null },
        "environment": { "K": 1, "gone": null },
        "oauth": { "clientId": "  id  ", "clientSecret": "", "scope": "", "redirectUri": "" },
    }));
    assert_eq!(
        entry,
        json!({
            "type": "remote",
            "url": "https://example.com/mcp",
            "headers": { "X-A": " 1 ", "empty": "" },
            "environment": { "K": "1" },
            "oauth": { "clientId": "id" },
            "enabled": true,
        })
    );
}

/// 验证 MCP 配置跨 user/project 两层的增查改删、scope 标注与缺失报错。
#[test]
fn mcp_crud_over_layers() {
    let (env, root) = temp_env("mcp");
    let project_dir = root.join("project");

    mcp::create_mcp_config(
        &env,
        "local-svc",
        &json!({ "type": "local", "command": ["uvx", "thing"] }),
        None,
        None,
    )
    .unwrap();
    let config = read_json(&env.config_file());
    assert_eq!(
        config.get("mcp").unwrap().get("local-svc"),
        Some(&json!({ "type": "local", "command": ["uvx", "thing"], "enabled": true }))
    );

    mcp::create_mcp_config(
        &env,
        "proj-svc",
        &json!({ "type": "remote", "url": "https://p.example/mcp" }),
        Some(&project_dir),
        Some("project"),
    )
    .unwrap();
    let project_file = project_dir.join(".opencode").join("opencode.json");
    assert!(project_file.exists());

    let listed = mcp::list_mcp_configs(&env, Some(&project_dir)).unwrap();
    let by_name = |name: &str| {
        listed
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
            .cloned()
            .unwrap()
    };
    assert_eq!(by_name("local-svc").get("scope"), Some(&json!("user")));
    assert_eq!(by_name("proj-svc").get("scope"), Some(&json!("project")));

    let fetched = mcp::get_mcp_config(&env, "local-svc", Some(&project_dir))
        .unwrap()
        .unwrap();
    assert_eq!(fetched.get("command"), Some(&json!(["uvx", "thing"])));
    assert!(
        mcp::get_mcp_config(&env, "missing", None)
            .unwrap()
            .is_none()
    );

    mcp::update_mcp_config(&env, "local-svc", &json!({ "enabled": false }), None).unwrap();
    let config = read_json(&env.config_file());
    assert_eq!(
        config
            .get("mcp")
            .unwrap()
            .get("local-svc")
            .unwrap()
            .get("enabled"),
        Some(&json!(false))
    );

    let error = mcp::update_mcp_config(&env, "missing", &json!({}), None).unwrap_err();
    assert_eq!(error, "MCP server \"missing\" not found");

    mcp::delete_mcp_config(&env, "local-svc", None).unwrap();
    assert_eq!(read_json(&env.config_file()), json!({}));
    let error = mcp::delete_mcp_config(&env, "local-svc", None).unwrap_err();
    assert_eq!(error, "MCP server \"local-svc\" not found");
}

// ---------------------------------------------------------------------------
// providers.js behavior
// ---------------------------------------------------------------------------

/// 验证自定义 provider 校验的逐条错误文案（id/baseURL/models/凭据/npm）
/// 与两种放行组合。
#[test]
fn provider_validation_error_strings() {
    use providers::ProviderValidation::{Err as PErr, Ok as POk};
    let check = |id: &str, config: Value, stored: bool| {
        providers::validate_custom_provider_config(id, &config, stored)
    };
    match check(
        "Bad Id",
        json!({ "name": "X", "options": { "baseURL": "https://a" }, "models": { "m": { "name": "M" } } }),
        true,
    ) {
        PErr(e) => assert_eq!(e, "Provider ID must match /^[a-z0-9][a-z0-9-_]*$/"),
        POk { .. } => panic!("expected id error"),
    }
    match check(
        "ok",
        json!({ "name": "X", "options": { "baseURL": "ftp://a" }, "models": { "m": { "name": "M" } } }),
        true,
    ) {
        PErr(e) => assert!(e.contains("http://"), "{e}"),
        POk { .. } => panic!("expected baseURL error"),
    }
    match check(
        "ok",
        json!({ "name": "X", "options": { "baseURL": "https://a" }, "models": {} }),
        true,
    ) {
        PErr(e) => assert_eq!(e, "At least one model is required"),
        POk { .. } => panic!("expected models error"),
    }
    match check(
        "ok",
        json!({ "name": "X", "options": { "baseURL": "https://a" }, "models": { "m": { "name": "M" } } }),
        false,
    ) {
        PErr(e) => assert_eq!(e, "API key or {env:VAR} credentials are required"),
        POk { .. } => panic!("expected credentials error"),
    }
    match check(
        "ok",
        json!({ "name": "X", "npm": "@example/unsupported", "env": ["K"], "options": { "baseURL": "https://a" }, "models": { "m": { "name": "M" } } }),
        true,
    ) {
        PErr(e) => assert!(e.contains("@ai-sdk/openai"), "{e}"),
        POk { .. } => panic!("expected npm error"),
    }
    assert!(matches!(
        check(
            "ok",
            json!({ "name": "X", "env": ["MY_KEY"], "options": { "baseURL": "https://a" }, "models": { "m": { "name": "M" } } }),
            false
        ),
        POk { .. }
    ));
    assert!(matches!(
        check(
            "ok",
            json!({ "name": "X", "options": { "baseURL": "https://a" }, "models": { "m": { "name": "M" } } }),
            true
        ),
        POk { .. }
    ));
}

/// 验证 provider 写入 project 层的完整落盘结构、更新时清除 disabled_providers
/// 记录、删除后来源标记消失。
#[test]
fn provider_upsert_project_scope_and_remove() {
    let (env, root) = temp_env("providers");
    let project_dir = root.join("project");

    let outcome = providers::upsert_provider_config(
        &env,
        "campus-llm",
        &json!({
            "name": "Campus LLM",
            "npm": "@ai-sdk/openai-compatible",
            "options": { "baseURL": "https://llm.example.edu/v1", "headers": { "X-Campus": "1" } },
            "models": { "fast-model": { "name": "Fast" } },
            "env": ["CAMPUS_KEY"],
        }),
        Some(&project_dir),
        "project",
        false,
    )
    .unwrap();
    assert_eq!(outcome.provider_id, "campus-llm");
    let written = read_json(&outcome.path);
    assert_eq!(
        written.get("provider").unwrap().get("campus-llm"),
        Some(&json!({
            "npm": "@ai-sdk/openai-compatible",
            "name": "Campus LLM",
            "env": ["CAMPUS_KEY"],
            "options": { "baseURL": "https://llm.example.edu/v1", "headers": { "X-Campus": "1" } },
            "models": { "fast-model": { "name": "Fast" } },
        }))
    );

    let sources = providers::get_provider_sources(&env, "campus-llm", Some(&project_dir)).unwrap();
    assert_eq!(
        sources.get("project").unwrap().get("exists"),
        Some(&json!(true))
    );

    // Update clears disabled_providers entries.
    write_file(
        &project_dir.join("opencode.json"),
        r#"{ "provider": { "campus-llm": { "name": "Old" } }, "disabled_providers": ["campus-llm", "other"] }"#,
    );
    providers::upsert_provider_config(
        &env,
        "campus-llm",
        &json!({
            "name": "Campus LLM",
            "options": { "baseURL": "https://llm.example.edu/v1" },
            "models": { "b": { "name": "B" } },
            "env": ["CAMPUS_KEY"],
        }),
        Some(&project_dir),
        "project",
        false,
    )
    .unwrap();
    let written = read_json(&project_dir.join("opencode.json"));
    assert_eq!(written.get("disabled_providers"), Some(&json!(["other"])));

    assert!(
        providers::remove_provider_config(&env, "campus-llm", Some(&project_dir), "project")
            .unwrap()
    );
    let sources = providers::get_provider_sources(&env, "campus-llm", Some(&project_dir)).unwrap();
    assert_eq!(
        sources.get("project").unwrap().get("exists"),
        Some(&json!(false))
    );
}

/// 验证 custom scope 只写 custom 层文件，user 层保持不存在。
#[test]
fn provider_custom_scope_writes_custom_layer_only() {
    let dir = temp_dir_unique("providers");
    let custom = dir.join("custom-opencode.json");
    let (env, root) = env_with_custom("providers", &custom);
    let project_dir = root.join("project");

    providers::upsert_provider_config(
        &env,
        "custom-svc",
        &json!({
            "name": "Custom",
            "options": { "baseURL": "https://custom.example.com/v1" },
            "models": { "m": { "name": "M" } },
        }),
        Some(&project_dir),
        "custom",
        true,
    )
    .unwrap();

    assert!(custom.exists());
    assert!(!env.config_file().exists());
    let sources = providers::get_provider_sources(&env, "custom-svc", Some(&project_dir)).unwrap();
    assert_eq!(
        sources.get("custom").unwrap().get("exists"),
        Some(&json!(true))
    );
    assert_eq!(
        sources.get("user").unwrap().get("exists"),
        Some(&json!(false))
    );
}

// ---------------------------------------------------------------------------
// auth.js / claude-cli-auth.js
// ---------------------------------------------------------------------------

/// 验证 auth.json 写读删全流程：首次写不备份、覆盖写生成备份、
/// 删除返回真假区分存在与否。
#[test]
fn auth_file_round_trip_and_removal() {
    let (env, _root) = temp_env("auth");
    let auth_file = env.data_dir.join("auth.json");

    assert!(
        super::auth::get_provider_auth(&env, "anthropic")
            .unwrap()
            .is_none()
    );

    let mut auth = serde_json::Map::new();
    auth.insert("anthropic".into(), json!({ "type": "api" }));
    super::auth::write_auth_file(&env, &Value::Object(auth.clone())).unwrap();
    // No prior file: no backup yet.
    assert!(!layers::backup_path(&auth_file).exists());

    auth.insert("openrouter".into(), json!({ "type": "api" }));
    super::auth::write_auth_file(&env, &Value::Object(auth)).unwrap();

    assert!(
        super::auth::get_provider_auth(&env, "anthropic")
            .unwrap()
            .is_some()
    );
    assert!(
        super::auth::get_provider_auth(&env, "openrouter")
            .unwrap()
            .is_some()
    );
    assert!(auth_file.exists());
    assert!(layers::backup_path(&auth_file).exists());

    assert!(super::auth::remove_provider_auth(&env, "anthropic").unwrap());
    assert!(
        super::auth::get_provider_auth(&env, "anthropic")
            .unwrap()
            .is_none()
    );
    assert!(!super::auth::remove_provider_auth(&env, "anthropic").unwrap());
}

/// 记录型同步 spawn 桩：按序回放预置输出、可让首次调用失败，
/// 并记录全部调用参数供断言。
struct RecordingSpawn {
    /// 已记录的（命令, 参数列表）调用序列。
    calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    /// 预置的标准输出队列（越界时重复取最后一项）。
    outputs: Vec<String>,
    /// 为真时第一次调用返回 error，模拟命令不存在。
    fail_first: bool,
}

/// 对 SyncSpawn trait 的桩实现：记录每次调用并按序回放预置输出。
impl super::claude_cli::SyncSpawn for RecordingSpawn {
    /// 记录命令与参数；fail_first 命中首次时返回 error，
    /// 否则回放与调用序号对应的预置标准输出。
    fn spawn_sync(
        &self,
        command: &str,
        args: &[&str],
        _env: &BTreeMap<String, String>,
        _timeout_ms: u64,
    ) -> super::claude_cli::SpawnOutcome {
        self.calls.lock().unwrap().push((
            command.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ));
        let call = self.calls.lock().unwrap().len();
        if self.fail_first && call == 1 {
            return super::claude_cli::SpawnOutcome {
                stdout: String::new(),
                error: true,
            };
        }
        super::claude_cli::SpawnOutcome {
            stdout: self
                .outputs
                .get(call.saturating_sub(1).min(self.outputs.len() - 1))
                .cloned()
                .unwrap_or_default(),
            error: false,
        }
    }
}

/// 验证登录态探测：调用 claude auth status --json 并解析 loggedIn；
/// 登出时返回对应的 reason，且敏感环境变量不会外泄到命令行。
#[test]
fn claude_cli_status_reports_login_state() {
    let spawn = RecordingSpawn {
        calls: std::sync::Mutex::new(Vec::new()),
        outputs: vec![r#"{"loggedIn":true,"authMethod":"oauth"}"#.to_string()],
        fail_first: false,
    };
    let status = super::claude_cli::get_claude_cli_auth_status(
        &spawn,
        &BTreeMap::from([(
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            "must-not-leak".to_string(),
        )]),
        false,
    );
    assert_eq!(status.connected, true);
    assert_eq!(status.reason, "logged-in");
    let calls = spawn.calls.lock().unwrap();
    assert_eq!(calls[0].0, "claude");
    assert_eq!(calls[0].1, vec!["auth", "status", "--json"]);

    let spawn = RecordingSpawn {
        calls: std::sync::Mutex::new(Vec::new()),
        outputs: vec![r#"{"loggedIn":false}"#.to_string()],
        fail_first: false,
    };
    let status = super::claude_cli::get_claude_cli_auth_status(&spawn, &BTreeMap::new(), false);
    assert_eq!(
        status,
        super::claude_cli::ClaudeCliAuthStatus {
            connected: false,
            reason: "logged-out".to_string(),
        }
    );
}

/// 验证直接执行失败后经登录 shell 定位 claude 可执行文件再执行的三段回退调用链。
#[test]
fn claude_cli_falls_back_to_login_shell() {
    let spawn = RecordingSpawn {
        calls: std::sync::Mutex::new(Vec::new()),
        outputs: vec![
            String::new(), // claude ENOENT path handled by fail_first
            "/Users/test/.local/bin/claude\n".to_string(),
            r#"{"loggedIn":true,"authMethod":"claude.ai"}"#.to_string(),
        ],
        fail_first: true,
    };
    let status = super::claude_cli::get_claude_cli_auth_status(
        &spawn,
        &BTreeMap::from([("SHELL".to_string(), "/bin/zsh".to_string())]),
        false,
    );
    assert_eq!(status.connected, true);
    let calls = spawn.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|(command, _)| command.as_str())
            .collect::<Vec<_>>(),
        vec!["claude", "/bin/zsh", "/Users/test/.local/bin/claude"]
    );
    assert_eq!(calls[1].1, vec!["-lic", "command -v claude"]);
}

// ---------------------------------------------------------------------------
// Routes — behavior/AGENTS.md (mock agent-dir endpoint + fallback)
// ---------------------------------------------------------------------------

/// 验证 AGENTS.md 路径优先取引擎上报的 agent 目录，并附带 legacy 路径信息。
#[tokio::test]
async fn behavior_agents_md_resolves_from_engine_agent_dir() {
    let (env, root) = temp_env("behavior");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let native_dir = env
        .home
        .join(".omp")
        .join("profiles")
        .join("night")
        .join("agent");
    let engine = MockEngine::start(Some(native_dir.to_string_lossy().into_owned()));
    let app = test_app(env.clone(), engine.context_with_engine(&data_dir));

    let (status, body, _) = send(app, request_json("GET", "/api/behavior/agents-md", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some(native_dir.join("AGENTS.md").to_string_lossy().as_ref())
    );
    assert_eq!(body.get("exists"), Some(&json!(false)));
    assert_eq!(
        body.get("legacy")
            .unwrap()
            .get("path")
            .and_then(Value::as_str),
        Some(env.config_dir.join("AGENTS.md").to_string_lossy().as_ref())
    );
}

/// 验证引擎未上报时回落默认目录且不缓存：profile 切换与引擎恢复后路径随请求更新。
#[tokio::test]
async fn behavior_agents_md_falls_back_without_pinning() {
    let (env, root) = temp_env("behavior");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let engine = MockEngine::start(None);
    let app = test_app(env.clone(), engine.context_with_engine(&data_dir));

    let (_, body, _) = send(
        app.clone(),
        request_json("GET", "/api/behavior/agents-md", None),
    )
    .await;
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some(
            env.home
                .join(".omp")
                .join("agent")
                .join("AGENTS.md")
                .to_string_lossy()
                .as_ref()
        )
    );

    // Profile switch changes the resolved dir on the next request (no cache).
    let day_dir = env
        .home
        .join(".omp")
        .join("profiles")
        .join("day")
        .join("agent");
    engine.set_agent_dir(Some(day_dir.to_string_lossy().into_owned()));
    let (_, body, _) = send(
        app.clone(),
        request_json("GET", "/api/behavior/agents-md", None),
    )
    .await;
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some(day_dir.join("AGENTS.md").to_string_lossy().as_ref())
    );

    // Unreachable engine → fallback, then recovery on the next request.
    let fallback_app = test_app(env.clone(), unreachable_engine_context(&data_dir));
    let (_, body, _) = send(
        fallback_app,
        request_json("GET", "/api/behavior/agents-md", None),
    )
    .await;
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some(
            env.home
                .join(".omp")
                .join("agent")
                .join("AGENTS.md")
                .to_string_lossy()
                .as_ref()
        )
    );
    let (_, body, _) = send(app, request_json("GET", "/api/behavior/agents-md", None)).await;
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some(day_dir.join("AGENTS.md").to_string_lossy().as_ref())
    );
}

/// 验证 GET 返回原生 AGENTS.md 内容与 legacy 内容标记；PUT 写回成功并返回延迟重启信封。
#[tokio::test]
async fn behavior_agents_md_serves_and_writes_native_file() {
    let (env, root) = temp_env("behavior");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let native_dir = env.home.join(".omp").join("agent");
    std::fs::create_dir_all(&native_dir).unwrap();
    write_file(&native_dir.join("AGENTS.md"), "native body");
    write_file(&env.config_dir.join("AGENTS.md"), "legacy body");
    let engine = MockEngine::start(Some(native_dir.to_string_lossy().into_owned()));
    let app = test_app(env, engine.context_with_engine(&data_dir));

    let (status, body, _) = send(
        app.clone(),
        request_json("GET", "/api/behavior/agents-md", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("content"), Some(&json!("native body")));
    assert_eq!(body.get("exists"), Some(&json!(true)));
    assert_eq!(
        body.get("legacy").unwrap().get("hasContent"),
        Some(&json!(true))
    );

    let (status, body, _) = send(
        app,
        request_json(
            "PUT",
            "/api/behavior/agents-md",
            Some((r#"{ "content": "updated" }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("success"), Some(&json!(true)));
    assert_eq!(body.get("restartDeferred"), Some(&json!(true)));
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("AGENTS.md saved. Restart the engine to apply.")
    );
    assert_eq!(
        std::fs::read_to_string(native_dir.join("AGENTS.md")).unwrap(),
        "updated"
    );
}

/// 验证超过 1 MiB 的内容写入返回 413 与上限错误文案。
#[tokio::test]
async fn behavior_put_rejects_oversized_content() {
    let (env, root) = temp_env("behavior");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let engine = MockEngine::start(None);
    let app = test_app(env, engine.context_with_engine(&data_dir));

    let big = "x".repeat(1024 * 1024 + 1);
    let payload = format!("{{ \"content\": \"{big}\" }}");
    let (status, body, _) = send(
        app,
        request_json(
            "PUT",
            "/api/behavior/agents-md",
            Some((&payload, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("Content exceeds maximum size of 1048576 bytes")
    );
}

// ---------------------------------------------------------------------------
// Routes — MCP auth pending + OAuth callback
// ---------------------------------------------------------------------------

/// 验证 MCP 授权挂起状态的存/查/删全周期与各错误形态（缺 name、缺 state、未知 state）。
#[tokio::test]
async fn pending_mcp_auth_lifecycle() {
    let (env, root) = temp_env("pending");
    let app = test_app(env, test_context(&root));

    // No state → context null.
    let (_, body, _) = send(
        app.clone(),
        request_json(
            "POST",
            "/api/mcp/auth/pending",
            Some((r#"{}"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(body, json!({ "success": true, "context": null }));

    // Missing name → 400.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "POST",
            "/api/mcp/auth/pending",
            Some((r#"{ "state": "s1" }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body.get("error"),
        Some(&json!("MCP server name is required"))
    );

    // Store.
    let (_, body, _) = send(
        app.clone(),
        request_json(
            "POST",
            "/api/mcp/auth/pending",
            Some((
                r#"{ "state": "s1", "name": "linear", "directory": "/projects/demo", "origin": "desktop" }"#,
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(
        body,
        json!({
            "success": true,
            "context": { "name": "linear", "directory": "/projects/demo", "origin": "desktop" }
        })
    );

    // Read (includes expiresAt).
    let (_, body, _) = send(
        app.clone(),
        request_json("GET", "/api/mcp/auth/pending?state=s1", None),
    )
    .await;
    assert_eq!(body.get("name"), Some(&json!("linear")));
    assert!(body.get("expiresAt").is_some());

    // Missing state → literal null body.
    let (_, body, _) = send(
        app.clone(),
        request_json("GET", "/api/mcp/auth/pending", None),
    )
    .await;
    assert_eq!(body, Value::Null);

    // Unknown state → 404.
    let (status, _, _) = send(
        app.clone(),
        request_json("GET", "/api/mcp/auth/pending?state=other", None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Clear.
    let (_, body, _) = send(
        app.clone(),
        request_json("DELETE", "/api/mcp/auth/pending?state=s1", None),
    )
    .await;
    assert_eq!(body, json!({ "success": true }));
    let (status, _, _) = send(
        app,
        request_json("GET", "/api/mcp/auth/pending?state=s1", None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 验证 OAuth 回调成功页触发引擎侧回调、包含桌面 deep link，且完成后清除挂起状态。
#[tokio::test]
async fn oauth_callback_completes_and_clears_state() {
    let (env, root) = temp_env("oauth");
    let engine = MockEngine::start(None);
    let app = test_app(env, engine.context_with_engine(&root));

    send(
        app.clone(),
        request_json(
            "POST",
            "/api/mcp/auth/pending",
            Some((
                r#"{ "state": "state-1", "name": "linear", "directory": "/projects/demo", "origin": "desktop" }"#,
                "application/json",
            )),
        ),
    )
    .await;

    let (status, _, text) = send(
        app.clone(),
        request_json(
            "GET",
            "/mcp/oauth/callback?state=state-1&code=auth-code",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("Authorization Complete"));
    assert!(text.contains("ompchamber://focus/mcp-auth"));

    let requests = engine.requests.lock().unwrap();
    assert_eq!(
        requests[0],
        (
            "callback".to_string(),
            "/mcp/linear/auth/callback?directory=%2Fprojects%2Fdemo".to_string()
        )
    );
    drop(requests);

    // State cleared after completion.
    let (status, _, _) = send(
        app,
        request_json("GET", "/api/mcp/auth/pending?state=state-1", None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 验证伪造 state 与 provider error 回调均被拒绝（不触达引擎），并清除挂起状态。
#[tokio::test]
async fn oauth_callback_rejects_unknown_state_and_provider_errors() {
    let (env, root) = temp_env("oauth");
    let engine = MockEngine::start(None);
    let app = test_app(env, engine.context_with_engine(&root));

    let (status, _, text) = send(
        app.clone(),
        request_json(
            "GET",
            "/mcp/oauth/callback?state=forged&code=attacker-code",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text.contains("Authorization Failed"));
    assert!(engine.requests.lock().unwrap().is_empty());

    send(
        app.clone(),
        request_json(
            "POST",
            "/api/mcp/auth/pending",
            Some((r#"{ "state": "s2", "name": "linear" }"#, "application/json")),
        ),
    )
    .await;
    let (status, _, text) = send(
        app.clone(),
        request_json(
            "GET",
            "/mcp/oauth/callback?state=s2&error=access_denied&error_description=User%20%3Cdenied%3E%20access",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text.contains("User &lt;denied&gt; access"));
    assert!(engine.requests.lock().unwrap().is_empty());

    let (status, _, _) = send(
        app,
        request_json("GET", "/api/mcp/auth/pending?state=s2", None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 验证 web 来源的授权完成页不包含桌面 deep link。
#[tokio::test]
async fn oauth_callback_omits_deep_link_for_web_origin() {
    let (env, root) = temp_env("oauth");
    let engine = MockEngine::start(None);
    let app = test_app(env, engine.context_with_engine(&root));

    send(
        app.clone(),
        request_json(
            "POST",
            "/api/mcp/auth/pending",
            Some((
                r#"{ "state": "web-1", "name": "linear" }"#,
                "application/json",
            )),
        ),
    )
    .await;
    let (status, _, text) = send(
        app,
        request_json("GET", "/mcp/oauth/callback?state=web-1&code=c", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!text.contains("ompchamber://"));
}

// ---------------------------------------------------------------------------
// Routes — provider CRUD
// ---------------------------------------------------------------------------

/// 验证 PUT /api/provider 的参数、scope 与配置校验失败形态，及 project 目录下的成功持久化。
#[tokio::test]
async fn put_provider_validates_and_persists() {
    let (env, root) = temp_env("provroute");
    let project_dir = root.join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    // The route resolves the directory through realpath (canonicalize).
    let project_dir = project_dir.canonicalize().unwrap();
    let app = test_app(env, test_context(&root));

    // Missing id.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "PUT",
            "/api/provider",
            Some((r#"{ "config": {} }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.get("error"), Some(&json!("Provider ID is required")));

    // Invalid scope.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "PUT",
            "/api/provider",
            Some((
                r#"{ "providerId": "x", "config": { "name": "X" }, "scope": "bogus" }"#,
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.get("error"), Some(&json!("Invalid scope")));

    // Validation failure → 400.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "PUT",
            "/api/provider",
            Some((
                r#"{ "providerId": "x", "config": { "name": "X", "options": { "baseURL": "nope" }, "models": { "m": { "name": "M" } } } }"#,
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body.get("error")
            .and_then(Value::as_str)
            .map(|e| e.contains("Base URL")),
        Some(true)
    );

    // Success with a project directory via query param.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "PUT",
            &format!(
                "/api/provider?directory={}",
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("directory", project_dir.to_str().unwrap())
                    .finish()
                    .strip_prefix("directory=")
                    .unwrap(),
            ),
            Some((
                &format!(
                    r#"{{ "providerId": "campus-llm", "scope": "project", "config": {{ "name": "Campus LLM", "options": {{ "baseURL": "https://llm.example.edu/v1" }}, "models": {{ "fast": {{ "name": "Fast" }} }}, "env": ["CAMPUS_KEY"] }} }}"#
                ),
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("success"), Some(&json!(true)));
    assert_eq!(body.get("restartDeferred"), Some(&json!(true)));
    assert_eq!(body.get("providerId"), Some(&json!("campus-llm")));
    let path = body
        .get("path")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    assert!(path.starts_with(project_dir.to_str().unwrap()), "{path}");
    assert!(Path::new(&path).exists());
}

/// 验证断开 provider 授权：未连接返回 not-connected 信封、已连接删除并延迟重启、非法 scope 返回 400。
#[tokio::test]
async fn delete_provider_auth_flows() {
    let (env, root) = temp_env("provdelete");
    let app = test_app(env.clone(), test_context(&root));

    // Nothing connected → not-connected envelope.
    let (status, body, _) = send(
        app.clone(),
        request_json("DELETE", "/api/provider/anthropic/auth", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "success": true,
            "removed": false,
            "requiresReload": false,
            "message": "Provider was not connected",
        })
    );

    // Store an auth entry, then disconnect.
    let mut auth = serde_json::Map::new();
    auth.insert("anthropic".into(), json!({ "type": "api" }));
    super::auth::write_auth_file(&env, &Value::Object(auth)).unwrap();

    let (status, body, _) = send(
        app.clone(),
        request_json("DELETE", "/api/provider/anthropic/auth", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("removed"), Some(&json!(true)));
    assert_eq!(body.get("restartDeferred"), Some(&json!(true)));
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("Provider disconnected successfully. Restart the engine to apply.")
    );

    // Invalid scope.
    let (status, body, _) = send(
        app,
        request_json("DELETE", "/api/provider/anthropic/auth?scope=zzz", None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.get("error"), Some(&json!("Invalid scope")));
}

/// 验证 provider 来源端点返回各层 exists 标记；非法 directory 返回 400 与错误文案。
#[tokio::test]
async fn provider_source_route_shape() {
    let (env, root) = temp_env("provsource");
    let project_dir = root.join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    write_file(
        &project_dir.join("opencode.json"),
        r#"{ "provider": { "campus-llm": { "name": "Campus" } } }"#,
    );
    let app = test_app(env, test_context(&root));

    let (status, body, _) = send(
        app.clone(),
        request_json(
            "GET",
            &format!(
                "/api/provider/campus-llm/source?directory={}",
                project_dir.to_str().unwrap()
            ),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("providerId"), Some(&json!("campus-llm")));
    let sources = body.get("sources").unwrap();
    assert_eq!(
        sources.get("auth").unwrap().get("exists"),
        Some(&json!(false))
    );
    assert_eq!(
        sources.get("project").unwrap().get("exists"),
        Some(&json!(true))
    );
    assert_eq!(
        sources.get("user").unwrap().get("exists"),
        Some(&json!(false))
    );

    // Explicit but invalid directory → 400 with the resolution error.
    let (status, body, _) = send(
        app,
        request_json(
            "GET",
            "/api/provider/campus-llm/source?directory=/definitely/not/a/real/dir",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.get("error"), Some(&json!("Directory not found")));
}

// ---------------------------------------------------------------------------
// Routes — config entities (agents / commands / mcp)
// -------------------------------------------------------------------

/// 写入 settings.json 指定 lastDirectory，使路由能解析出默认项目目录。
fn settings_with_last_directory(data_dir: &Path, directory: &Path) {
    write_file(
        &data_dir.join("settings.json"),
        &serde_json::to_string(&json!({
            "lastDirectory": directory.to_string_lossy(),
            "projects": [],
        }))
        .unwrap(),
    );
}

/// 验证 agent 实体路由全流程：目录缺失 400、内置 agent 来源查询、创建
/// （延迟重启信封）、重复创建 500、配置读取合并、PATCH 局部更新、按 scope 删除。
#[tokio::test]
async fn agent_entity_routes_deferred_apply() {
    let (env, root) = temp_env("agentroutes");
    let project_dir = root.join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    settings_with_last_directory(&data_dir, &project_dir);
    let app = test_app(env, test_context(&data_dir));

    // No directory resolvable → 400.
    let bare_data = root.join("bare-data");
    std::fs::create_dir_all(&bare_data).unwrap();
    let bare_app = {
        let (env2, _root2) = temp_env("agentroutes-bare");
        test_app(env2, test_context(&bare_data))
    };
    let (status, body, _) = send(
        bare_app,
        request_json("GET", "/api/config/agents/someone", None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body.get("error"),
        Some(&json!("Directory parameter or active project is required"))
    );

    // Sources for a built-in agent.
    let (status, body, _) = send(
        app.clone(),
        request_json("GET", "/api/config/agents/builtin-agent", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("name"), Some(&json!("builtin-agent")));
    assert_eq!(body.get("isBuiltIn"), Some(&json!(true)));
    assert_eq!(body.get("scope"), Some(&Value::Null));
    assert_eq!(
        body.get("sources")
            .unwrap()
            .get("md")
            .unwrap()
            .get("exists"),
        Some(&json!(false))
    );

    // Create (project scope) → deferred envelope + file on disk.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "POST",
            "/api/config/agents/strateg",
            Some((
                r#"{ "scope": "project", "description": "Strategy agent", "prompt": "Do strategy" }"#,
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("requiresRestart"), Some(&json!(true)));
    assert_eq!(body.get("restartDeferred"), Some(&json!(true)));
    assert_eq!(body.get("requiresReload"), Some(&json!(false)));
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("Agent strateg created successfully. Restart the engine to apply.")
    );
    let agent_file = project_dir
        .join(".opencode")
        .join("agents")
        .join("strateg.md");
    assert!(agent_file.exists());

    // Duplicate create → 500 with the exact JS message.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "POST",
            "/api/config/agents/strateg",
            Some((
                r#"{ "scope": "project", "description": "x" }"#,
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body.get("error"),
        Some(&json!(
            "Agent strateg already exists as project-level .md file"
        ))
    );

    // Config read merges frontmatter + prompt.
    let (status, body, _) = send(
        app.clone(),
        request_json("GET", "/api/config/agents/strateg/config", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("source"), Some(&json!("md")));
    assert_eq!(body.get("scope"), Some(&json!("project")));
    assert_eq!(
        body.get("config").unwrap().get("prompt"),
        Some(&json!("Do strategy"))
    );

    // PATCH updates the model and keeps unrelated fields.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "PATCH",
            "/api/config/agents/strateg",
            Some((r#"{ "model": "openai/gpt-5" }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("Agent strateg updated successfully. Restart the engine to apply.")
    );
    let md = super::md_file::parse_md_file(&agent_file).unwrap();
    assert_eq!(md.frontmatter.get("model"), Some(&json!("openai/gpt-5")));
    assert_eq!(
        md.frontmatter.get("description"),
        Some(&json!("Strategy agent"))
    );

    // DELETE with body scope.
    let (status, body, _) = send(
        app.clone(),
        request_json(
            "DELETE",
            "/api/config/agents/strateg",
            Some((r#"{ "scope": "project" }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("Agent strateg deleted successfully. Restart the engine to apply.")
    );
    assert!(!agent_file.exists());
}

/// 验证 command 实体路由通过 directory 查询参数完成建/查/改/删
/// （引擎不可达不影响延迟重启语义）。
#[tokio::test]
async fn command_entity_routes_deferred_apply() {
    let (env, root) = temp_env("cmdroutes");
    let project_dir = root.join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let app = test_app(env, unreachable_engine_context(&root.join("data")));

    let base = format!("?directory={}", project_dir.to_str().unwrap());

    let (status, body, _) = send(
        app.clone(),
        request_json(
            "POST",
            &format!("/api/config/commands/build{base}"),
            Some((
                r#"{ "scope": "project", "description": "Build", "template": "npm run build" }"#,
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("Command build created successfully. Restart the engine to apply.")
    );
    assert!(
        project_dir
            .join(".opencode")
            .join("commands")
            .join("build.md")
            .exists()
    );

    let (status, body, _) = send(
        app.clone(),
        request_json("GET", &format!("/api/config/commands/build{base}"), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("isBuiltIn"), Some(&json!(false)));
    assert_eq!(body.get("scope"), Some(&json!("project")));

    let (status, _, _) = send(
        app.clone(),
        request_json(
            "PATCH",
            &format!("/api/config/commands/build{base}"),
            Some((r#"{ "template": "pnpm build" }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body, _) = send(
        app,
        request_json("DELETE", &format!("/api/config/commands/build{base}"), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("Command build deleted successfully. Restart the engine to apply.")
    );
}

/// 验证 MCP 实体路由：空列表非 null、建/查/改/删信封与 404 错误文案。
#[tokio::test]
async fn mcp_entity_routes_shapes() {
    let (env, root) = temp_env("mcproutes");
    let app = test_app(env, test_context(&root));

    let (status, body, text) =
        send(app.clone(), request_json("GET", "/api/config/mcp", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Array(Vec::new()));
    assert_ne!(text, "null");

    let (status, body, _) = send(
        app.clone(),
        request_json(
            "POST",
            "/api/config/mcp/local-svc",
            Some((
                r#"{ "type": "local", "command": ["uvx", "thing"] }"#,
                "application/json",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("MCP server \"local-svc\" created. Restart the engine to apply.")
    );

    let (status, body, _) = send(
        app.clone(),
        request_json("GET", "/api/config/mcp/local-svc", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("name"), Some(&json!("local-svc")));
    assert_eq!(body.get("scope"), Some(&json!("user")));
    assert_eq!(body.get("enabled"), Some(&json!(true)));

    let (status, body, _) = send(
        app.clone(),
        request_json("GET", "/api/config/mcp/missing", None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body.get("error"),
        Some(&json!("MCP server \"missing\" not found"))
    );

    let (status, body, _) = send(
        app.clone(),
        request_json(
            "PATCH",
            "/api/config/mcp/missing",
            Some((r#"{ "enabled": false }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body.get("error"),
        Some(&json!("MCP server \"missing\" not found"))
    );

    let (status, _, _) = send(
        app.clone(),
        request_json(
            "PATCH",
            "/api/config/mcp/local-svc",
            Some((r#"{ "enabled": false }"#, "application/json")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body, _) = send(
        app.clone(),
        request_json("DELETE", "/api/config/mcp/local-svc", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("message").and_then(Value::as_str),
        Some("MCP server \"local-svc\" deleted. Restart the engine to apply.")
    );
    let (status, body, _) = send(app, request_json("GET", "/api/config/mcp", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Array(Vec::new()));
}
