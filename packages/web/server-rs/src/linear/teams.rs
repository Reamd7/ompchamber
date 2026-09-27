//! Port of `server/lib/linear/teams.js` — paginated Linear team list for the
//! mapping UI.
//! 本模块是 `server/lib/linear/teams.js` 的 Rust 移植：为映射 UI 分页
//! 拉取 Linear team 列表。

use std::sync::Arc;

use serde_json::{Value, json};

use super::client::{fetch_linear_graphql, get_valid_linear_access_token};
use super::issues::read_team;
use super::parse::{is_plain_object, read_trimmed_string};
use super::{LinearError, LinearState};

/// 分页列出 team（id/key/name）的 GraphQL 查询，以 cursor 翻页。
const TEAMS_QUERY: &str = r#"
  query ListLinearTeams($first: Int!, $after: String) {
    teams(first: $first, after: $after) {
      nodes { id key name }
      pageInfo { hasNextPage endCursor }
    }
  }
"#;
/// 每页拉取的 team 数量。
const PAGE_SIZE: i64 = 50;
/// 翻页次数上限，防止异常的分页响应造成无限循环。
const MAX_PAGES: usize = 20;

/// 对应 JS listLinearTeams：返回 { connected, teams }；未连接时返回
/// connected=false；遇到 401 时清除对应 workspace 的授权并同样返回
/// connected=false（而不是把错误抛给调用方）。
/// JS `listLinearTeams`.
pub async fn list_linear_teams(state: &Arc<LinearState>) -> Result<Value, LinearError> {
    let outcome = run(state).await;
    match outcome {
        Err(error) if error.http_status() == Some(401) => {
            let workspace = state.get_auth().map(|auth| auth.workspace_id);
            state.clear_auth(workspace.as_deref());
            Ok(json!({ "connected": false }))
        }
        other => other,
    }
}

/// 实际的分页拉取循环：先取有效 token（无则 connected=false），随后按
/// endCursor 翻页收集 team，直到没有下一页、缺 endCursor 或达到页数上限。
async fn run(state: &Arc<LinearState>) -> Result<Value, LinearError> {
    let Some(token) = get_valid_linear_access_token(state, None).await? else {
        return Ok(json!({ "connected": false }));
    };

    let mut teams: Vec<Value> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut variables = json!({ "first": PAGE_SIZE });
        if let Some(after) = after.clone() {
            variables["after"] = json!(after);
        }
        let data = fetch_linear_graphql(state, &token, TEAMS_QUERY, Some(&variables)).await?;
        let connection = &data["teams"];
        if let Some(nodes) = connection["nodes"].as_array() {
            for node in nodes {
                if let Some(team) = read_team(node) {
                    teams.push(team.to_json());
                }
            }
        }
        let page_info = &connection["pageInfo"];
        if !is_plain_object(page_info) || !matches!(page_info["hasNextPage"], Value::Bool(true)) {
            break;
        }
        let next = read_trimmed_string(&page_info["endCursor"]);
        if next.is_empty() {
            break;
        }
        after = Some(next);
    }

    Ok(json!({ "connected": true, "teams": teams }))
}
