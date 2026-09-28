//! Port of `server/lib/linear/issues.js` — list/search/get issues, team
//! workflow states, `issueUpdate` (identifier→UUID resolution first), and
//! `commentCreate` for session status comments.
//! 中文说明：移植自 JS 版 issues.js。承载 Linear issue 的列表/搜索/详情
//! 查询、团队 workflow 状态读取、`issueUpdate`（先把 identifier 解析成
//! UUID，因为 Linear 的 mutation 只接受 UUID）、以及会话状态评论用的
//! `commentCreate`。所有 GraphQL 查询文本与 JS 版逐字对齐。

use std::sync::Arc;

use serde_json::{Value, json};

use super::client::{fetch_linear_graphql, get_valid_linear_access_token};
use super::parse::{is_plain_object, read_finite_number, read_trimmed_string};
use super::{LinearError, LinearState};

/// 分页大小：与 JS 版一致，每页拉取 50 条 issue。
const PAGE_SIZE: i64 = 50;
/// 列表查询：按 `updatedAt` 倒序分页拉取 issue 摘要（不含 description 与评论）。
const LIST_QUERY: &str = r#"
  query ListLinearIssues($first: Int!, $after: String, $filter: IssueFilter) {
    issues(first: $first, after: $after, filter: $filter, orderBy: updatedAt) {
      nodes {
  id
  identifier
  title
  url
  priority
  state { id name type }
  assignee { name displayName avatarUrl }
  team { id key name }
  labels { nodes { id name color } }
      }
      pageInfo { hasNextPage endCursor }
    }
  }
"#;

/// 搜索查询：走 Linear 的 `searchIssues` 全文搜索端点，节点结构与列表查询相同。
const SEARCH_QUERY: &str = r#"
  query SearchLinearIssues($term: String!, $first: Int!, $after: String, $filter: IssueFilter) {
    searchIssues(term: $term, first: $first, after: $after, filter: $filter) {
      nodes {
  id
  identifier
  title
  url
  priority
  state { id name type }
  assignee { name displayName avatarUrl }
  team { id key name }
  labels { nodes { id name color } }
      }
      pageInfo { hasNextPage endCursor }
    }
  }
"#;

/// 详情查询：单条 issue，额外带 description 和前 50 条评论（含作者信息）。
const GET_QUERY: &str = r#"
  query GetLinearIssue($id: String!) {
    issue(id: $id) {
      id
  identifier
  title
  url
  priority
  state { id name type }
  assignee { name displayName avatarUrl }
  team { id key name }
  labels { nodes { id name color } }
      description
      comments(first: 50) {
        nodes {
          id
          body
          createdAt
          user { name displayName avatarUrl }
        }
      }
    }
  }
"#;

/// 评论创建 mutation：向指定 issue 写入一条评论，只回读 `comment.id`。
const COMMENT_CREATE: &str = r#"
  mutation CommentCreate($input: CommentCreateInput!) {
    commentCreate(input: $input) {
      success
      comment { id }
    }
  }
"#;

/// 团队状态查询：拉取指定团队最多 50 个 workflow 状态（id/name/type/position）。
const STATES_QUERY: &str = r#"
  query TeamWorkflowStates($id: String!) {
    team(id: $id) {
      states(first: 50) {
        nodes { id name type position }
      }
    }
  }
"#;

/// 更新 mutation：`issueUpdate` 修改 issue（当前用于切换状态），
/// 返回完整 issue（含 description 与评论）。
const ISSUE_UPDATE: &str = r#"
  mutation IssueUpdate($id: String!, $input: IssueUpdateInput!) {
    issueUpdate(id: $id, input: $input) {
      success
      issue {
        id
  identifier
  title
  url
  priority
  state { id name type }
  assignee { name displayName avatarUrl }
  team { id key name }
  labels { nodes { id name color } }
        description
        comments(first: 50) {
          nodes {
            id
            body
            createdAt
            user { name displayName avatarUrl }
          }
        }
      }
    }
  }
"#;

/// 解析后的 issue 引用：`kind` 标明 `value` 是人类可读 identifier 还是 Linear UUID。
#[derive(Debug, Clone, PartialEq)]
pub struct IssueRef {
    /// 引用类型：Identifier（如 `ENG-123`）或 Id（UUID）。
    pub kind: IssueRefKind,
    /// 归一化后的引用值：identifier 转大写、UUID 转小写。
    pub value: String,
}

/// issue 引用的两种形态，决定后续走 identifier 解析还是直接按 UUID 查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueRefKind {
    /// 人类可读的团队前缀编号（如 `ENG-123`），查询前需先解析成 UUID。
    Identifier,
    /// Linear 内部 UUID，可直接用于 GraphQL 查询与 mutation。
    Id,
}

/// JS `parseLinearIssueRef`: identifier, Linear issue URL, or UUID.
///
/// 中文说明：依次尝试 Linear URL、identifier、UUID 三种形态；identifier
/// 统一转大写、UUID 统一转小写，无法识别时返回 `None`。
pub fn parse_linear_issue_ref(value: &str) -> Option<IssueRef> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(identifier) = match_url_identifier(trimmed) {
        return Some(IssueRef {
            kind: IssueRefKind::Identifier,
            value: identifier.to_uppercase(),
        });
    }
    if is_identifier(trimmed) {
        return Some(IssueRef {
            kind: IssueRefKind::Identifier,
            value: trimmed.to_uppercase(),
        });
    }
    if is_uuid(trimmed) {
        return Some(IssueRef {
            kind: IssueRefKind::Id,
            value: trimmed.to_lowercase(),
        });
    }
    None
}

/// `^[A-Za-z][A-Za-z0-9]*-\d+$`
///
/// 中文说明：手写实现该正则（字母开头的团队 key + `-` + 纯数字编号），
/// 避免引入 regex 依赖。
fn is_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut i = 0;
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == 0 || i >= bytes.len() || bytes[i] != b'-' {
        return false;
    }
    let digits = &bytes[i + 1..];
    !digits.is_empty() && digits.iter().all(|b| b.is_ascii_digit())
}

/// `^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$` (case
/// insensitive).
///
/// 中文说明：按 8-4-4-4-12 分组逐段校验十六进制，不依赖 regex crate。
fn is_uuid(value: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let bytes = value.as_bytes();
    let mut position = 0;
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            if position >= bytes.len() || bytes[position] != b'-' {
                return false;
            }
            position += 1;
        }
        let end = position + group;
        if end > bytes.len() {
            return false;
        }
        if !bytes[position..end].iter().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        position = end;
    }
    position == bytes.len()
}

/// `/linear\.app\/(?:[^/]+\/)?issue\/([A-Za-z][A-Za-z0-9]*-\d+)/i`
///
/// 中文说明：扫描所有 `linear.app/` 出现位置，手写复刻正则的贪婪匹配与
/// 回溯（先试带团队段，再退回无团队段）。
fn match_url_identifier(input: &str) -> Option<String> {
    let hay = input.as_bytes();
    let marker = b"linear.app/";
    for start in 0..hay.len() {
        if !matches_ci(hay, start, marker) {
            continue;
        }
        let after = start + marker.len();
        // Greedy `(?:[^/]+\/)?` first: one path segment, then "issue/".
        if let Some(slash) = hay[after..]
            .iter()
            .position(|&c| c == b'/')
            .map(|offset| after + offset)
            && let Some(identifier) = read_identifier_after_issue(hay, slash + 1)
        {
            return Some(identifier);
        }
        // Backtrack to the zero-width alternative.
        if let Some(identifier) = read_identifier_after_issue(hay, after) {
            return Some(identifier);
        }
    }
    None
}

/// 在 `position` 处匹配 `issue/` 前缀并读取紧随其后的 identifier；
/// 前缀不匹配或格式非法（无数字编号等）时返回 `None`。
fn read_identifier_after_issue(hay: &[u8], position: usize) -> Option<String> {
    let issue = b"issue/";
    if !matches_ci(hay, position, issue) {
        return None;
    }
    let start = position + issue.len();
    let mut end = start;
    if end >= hay.len() || !hay[end].is_ascii_alphabetic() {
        return None;
    }
    end += 1;
    while end < hay.len() && hay[end].is_ascii_alphanumeric() {
        end += 1;
    }
    if end >= hay.len() || hay[end] != b'-' {
        return None;
    }
    end += 1;
    let digits_start = end;
    while end < hay.len() && hay[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return None;
    }
    String::from_utf8(hay[start..end].to_vec()).ok()
}

/// ASCII 大小写不敏感地判断 `hay` 在 `position` 处是否以 `needle` 开头；
/// 长度不足（越界）视为不匹配。
fn matches_ci(hay: &[u8], position: usize, needle: &[u8]) -> bool {
    hay.len() >= position + needle.len()
        && hay[position..position + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Filters for `listLinearIssues`.
///
/// 中文说明：全部字段来自 query string，空字符串表示该过滤条件不生效。
#[derive(Debug, Clone, Default)]
pub struct ListIssuesParams {
    /// 搜索词：能解析成 issue 引用时直接查那条 issue，否则做全文搜索；为空则纯列表。
    pub query: String,
    /// 分页游标（上一页返回的 `endCursor`），为空表示第一页。
    pub cursor: String,
    /// 状态过滤：open/backlog/todo/started/inReview/completed/canceled/duplicate/all。
    pub status: String,
    /// 负责人过滤：`me` 只看自己，`any`（默认）不过滤。
    pub assignee: String,
    /// 团队 id 过滤，为空表示所有团队。
    pub team_id: String,
    /// 优先级过滤：none/urgent/high/medium/low，默认 `all` 不过滤。
    pub priority: String,
}

/// JS `buildIssueListFilter`.
///
/// 中文说明：把过滤参数组装成 Linear 的 `IssueFilter` JSON；所有条件都
/// 无效时返回 `None`（不传 filter）。status/priority 的非法值在读取阶段
/// 已被归一化，不会出现在这里。
pub fn build_issue_list_filter(params: &ListIssuesParams) -> Option<Value> {
    let status = read_list_status(&params.status);
    let assignee = read_list_assignee(&params.assignee);
    let priority = read_list_priority(&params.priority);
    let team = params.team_id.trim();
    let mut filter = serde_json::Map::new();
    if status != "all" {
        filter.insert("state".into(), list_status_state(&status));
    }
    if assignee == "me" {
        filter.insert("assignee".into(), json!({ "isMe": { "eq": true } }));
    }
    if !team.is_empty() {
        filter.insert("team".into(), json!({ "id": { "eq": team } }));
    }
    if priority != "all" {
        filter.insert(
            "priority".into(),
            json!({ "eq": list_priority_value(&priority) }),
        );
    }
    (!filter.is_empty()).then_some(Value::Object(filter))
}

/// 归一化 status 过滤值：合法值原样返回（含 `all`），非法值回退到 `open`。
fn read_list_status(value: &str) -> String {
    let status = value.trim();
    if status == "all" || list_status_state(status).is_object() {
        status.to_string()
    } else {
        "open".to_string()
    }
}

/// 把 status 关键字翻译成 Linear `IssueFilter.state` 条件；
/// `started`/`canceled` 需按名称排除 In Review/Duplicate，`duplicate` 是
/// 类型或名称的或组合，未知值返回 `Null`（表示无法映射为过滤器）。
fn list_status_state(status: &str) -> Value {
    match status {
        "open" => json!({ "type": { "nin": ["completed", "canceled", "duplicate"] } }),
        "backlog" => json!({ "type": { "eq": "backlog" } }),
        "todo" => json!({ "type": { "eq": "unstarted" } }),
        "started" => {
            json!({ "type": { "eq": "started" }, "name": { "neqIgnoreCase": "In Review" } })
        }
        "inReview" => json!({ "name": { "eqIgnoreCase": "In Review" } }),
        "completed" => json!({ "type": { "eq": "completed" } }),
        "canceled" => {
            json!({ "type": { "eq": "canceled" }, "name": { "neqIgnoreCase": "Duplicate" } })
        }
        "duplicate" => json!({
            "or": [
                { "type": { "eq": "duplicate" } },
                { "name": { "eqIgnoreCase": "Duplicate" } }
            ]
        }),
        _ => Value::Null,
    }
}

/// 归一化 assignee 过滤值：只接受 `me` 与 `any`，其余回退 `any`。
fn read_list_assignee(value: &str) -> String {
    match value.trim() {
        "me" | "any" => value.trim().to_string(),
        _ => "any".to_string(),
    }
}

/// 归一化 priority 过滤值：只接受 none/urgent/high/medium/low，其余回退 `all`。
fn read_list_priority(value: &str) -> String {
    match value.trim() {
        "none" | "urgent" | "high" | "medium" | "low" => value.trim().to_string(),
        _ => "all".to_string(),
    }
}

/// 把优先级关键字映射为 Linear 的数字优先级（0=none … 4=low），未知值按 0 处理。
fn list_priority_value(priority: &str) -> i64 {
    match priority {
        "none" => 0,
        "urgent" => 1,
        "high" => 2,
        "medium" => 3,
        "low" => 4,
        _ => 0,
    }
}

/// issue 的工作流状态摘要（id/name/type 皆可缺失，全缺失则视为无状态）。
#[derive(Debug, Clone, PartialEq)]
struct IssueState {
    /// 状态的 UUID；输入缺失时为 `None`。
    id: Option<String>,
    /// 状态名称（如 In Review）；输入缺失时为 `None`。
    name: Option<String>,
    /// 状态类型（started/completed 等）；输入缺失时为 `None`。
    kind: Option<String>,
}

/// 序列化为 JS 版一致的 `{ id, name, type }` JSON（缺失字段输出 null）。
impl IssueState {
    /// 生成 `{ id, name, type }` JSON，`None` 字段序列化为 null。
    fn to_json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "type": self.kind })
    }
}

/// 从 GraphQL 节点读取状态：非对象或三个字段全空时返回 `None`。
fn read_state(value: &Value) -> Option<IssueState> {
    if !is_plain_object(value) {
        return None;
    }
    let id = optional(&value["id"]);
    let name = optional(&value["name"]);
    let kind = optional(&value["type"]);
    if id.is_none() && name.is_none() && kind.is_none() {
        return None;
    }
    Some(IssueState { id, name, kind })
}

/// 读取字符串并去首尾空白：空串视为 `None`（JS 的“存在且非空”语义）。
fn optional(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 团队 workflow 状态（`listLinearIssueStates` 的返回单元），用于状态切换 UI。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowState {
    /// 状态 UUID，作为 `issueUpdate` 的 `stateId`。
    pub id: String,
    /// 状态显示名（必填，缺失则整条记录被丢弃）。
    pub name: String,
    /// 状态类型（triage/backlog/started 等），用于排序分级。
    pub kind: Option<String>,
    /// 同类型内的顺序位置，缺失时按 0 处理。
    pub position: f64,
}

/// 序列化为 `{ id, name, type, position }`，position 经 `num` 规整为有限数。
impl WorkflowState {
    /// 生成 `{ id, name, type, position }` JSON。
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "type": self.kind,
            "position": super::parse::num(self.position),
        })
    }
}

/// JS `WORKFLOW_TYPE_ORDER` (triage < backlog < unstarted < started <
/// completed < canceled; unknown last).
///
/// 中文说明：返回状态类型的排序权重；未知类型排最后（99）。
fn workflow_type_rank(kind: &str) -> i64 {
    match kind {
        "triage" => 0,
        "backlog" => 1,
        "unstarted" => 2,
        "started" => 3,
        "completed" => 4,
        "canceled" => 5,
        _ => 99,
    }
}

/// 从 GraphQL 节点读取 workflow 状态：id 或 name 缺失时返回 `None`。
fn read_workflow_state(value: &Value) -> Option<WorkflowState> {
    if !is_plain_object(value) {
        return None;
    }
    let id = read_trimmed_string(&value["id"]);
    let name = read_trimmed_string(&value["name"]);
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(WorkflowState {
        id,
        name,
        kind: optional(&value["type"]),
        position: read_finite_number(&value["position"]).unwrap_or(0.0),
    })
}

/// JS `compareWorkflowStates` (stable sort, then type rank, position, name).
///
/// 中文说明：先按类型权重，再按 position，最后按名称（用 ASCII 比较近似
/// JS 的 localeCompare，覆盖实际字符集）。
fn compare_workflow_states(left: &WorkflowState, right: &WorkflowState) -> std::cmp::Ordering {
    let kind_of = |state: &WorkflowState| state.kind.clone().unwrap_or_default();
    let type_delta = workflow_type_rank(&kind_of(left)).cmp(&workflow_type_rank(&kind_of(right)));
    if type_delta != std::cmp::Ordering::Equal {
        return type_delta;
    }
    let position_delta = left
        .position
        .partial_cmp(&right.position)
        .unwrap_or(std::cmp::Ordering::Equal);
    if position_delta != std::cmp::Ordering::Equal {
        return position_delta;
    }
    // JS `localeCompare`; ASCII comparison is the practical subset.
    left.name.cmp(&right.name)
}

/// issue 负责人摘要（三个字段全缺失视为无人负责）。
#[derive(Debug, Clone, PartialEq)]
struct Assignee {
    /// 负责人全名。
    name: Option<String>,
    /// 负责人显示名。
    display_name: Option<String>,
    /// 负责人头像 URL。
    avatar_url: Option<String>,
}

/// 序列化为 `{ name, displayName, avatarUrl }`。
impl Assignee {
    /// 生成 `{ name, displayName, avatarUrl }` JSON。
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "displayName": self.display_name,
            "avatarUrl": self.avatar_url,
        })
    }
}

/// 从 GraphQL 节点读取负责人：非对象或三个字段全空时返回 `None`。
fn read_assignee(value: &Value) -> Option<Assignee> {
    if !is_plain_object(value) {
        return None;
    }
    let name = optional(&value["name"]);
    let display_name = optional(&value["displayName"]);
    let avatar_url = optional(&value["avatarUrl"]);
    if name.is_none() && display_name.is_none() && avatar_url.is_none() {
        return None;
    }
    Some(Assignee {
        name,
        display_name,
        avatar_url,
    })
}

/// Linear 团队（id/key/name，被 issue 与 mapping 共用）。
#[derive(Debug, Clone, PartialEq)]
pub struct LinearTeam {
    /// 团队 UUID。
    pub id: String,
    /// 团队 key（identifier 的前缀，如 `ENG`）。
    pub key: String,
    /// 团队显示名。
    pub name: String,
}

/// 序列化为 `{ id, key, name }`。
impl LinearTeam {
    /// 生成 `{ id, key, name }` JSON。
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "key": self.key, "name": self.name })
    }
}

/// 从 GraphQL 节点读取团队：id/key/name 任一缺失时返回 `None`。
pub fn read_team(value: &Value) -> Option<LinearTeam> {
    if !is_plain_object(value) {
        return None;
    }
    let id = read_trimmed_string(&value["id"]);
    let key = read_trimmed_string(&value["key"]);
    let name = read_trimmed_string(&value["name"]);
    if id.is_empty() || key.is_empty() || name.is_empty() {
        return None;
    }
    Some(LinearTeam { id, key, name })
}

/// JS `readPriority`: integer 0–4.
///
/// 中文说明：只接受 0–4 的整数值（JS 的 Number 语义），小数或越界返回
/// `None`。
fn read_priority(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    if number.fract() != 0.0 || !(0.0..=4.0).contains(&number) {
        return None;
    }
    Some(number as i64)
}

/// JS `readLabelColor`: normalized `#rrggbb`.
///
/// 中文说明：容忍可省略的 `#` 前缀，其余必须是 6 位十六进制；输出统一
/// 小写加 `#`。
fn read_label_color(value: &Value) -> Option<String> {
    let raw = read_trimmed_string(value);
    if raw.is_empty() {
        return None;
    }
    let hex = raw.strip_prefix('#').unwrap_or(&raw);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", hex.to_lowercase()))
}

/// 从 GraphQL 节点读取单个 label：id 与 name 必填，color 可为 null。
fn read_label(value: &Value) -> Option<Value> {
    if !is_plain_object(value) {
        return None;
    }
    let id = read_trimmed_string(&value["id"]);
    let name = read_trimmed_string(&value["name"]);
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(json!({
        "id": id,
        "name": name,
        "color": read_label_color(&value["color"]),
    }))
}

/// 读取 label 连接的 nodes（容忍直接给数组），逐条解析并静默丢弃非法项。
fn read_labels(value: &Value) -> Vec<Value> {
    let nodes = value["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| value.as_array().cloned().unwrap_or_default());
    nodes.iter().filter_map(read_label).collect()
}

/// issue 摘要：列表/搜索返回的字段子集（不含 description 与评论）。
#[derive(Debug, Clone)]
struct IssueSummary {
    /// issue UUID（必填）。
    id: String,
    /// 人类可读编号，如 `ENG-123`（必填）。
    identifier: String,
    /// 标题（必填）。
    title: String,
    /// Linear Web URL（必填）。
    url: String,
    /// 工作流状态，输入缺失/无效时为 `None`。
    state: Option<IssueState>,
    /// 负责人，无人负责时为 `None`。
    assignee: Option<Assignee>,
    /// 所属团队，缺失/无效时为 `None`。
    team: Option<LinearTeam>,
    /// 0–4 的数字优先级，非法值（小数/越界/缺失）为 `None`。
    priority: Option<i64>,
    /// 已解析的 label JSON 数组（原样透传给前端）。
    labels: Vec<Value>,
}

/// 序列化为 JS 版一致的 issue 摘要 JSON（可选字段输出 null）。
impl IssueSummary {
    /// 生成完整摘要对象，可选成员为 null。
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "identifier": self.identifier,
            "title": self.title,
            "url": self.url,
            "state": self.state.as_ref().map(IssueState::to_json),
            "assignee": self.assignee.as_ref().map(Assignee::to_json),
            "team": self.team.as_ref().map(LinearTeam::to_json),
            "priority": self.priority,
            "labels": self.labels,
        })
    }
}

/// 从 GraphQL 节点读取 issue 摘要：四个必填字符串任一为空即返回 `None`。
fn read_issue_summary(node: &Value) -> Option<IssueSummary> {
    if !is_plain_object(node) {
        return None;
    }
    let id = read_trimmed_string(&node["id"]);
    let identifier = read_trimmed_string(&node["identifier"]);
    let title = read_trimmed_string(&node["title"]);
    let url = read_trimmed_string(&node["url"]);
    if id.is_empty() || identifier.is_empty() || title.is_empty() || url.is_empty() {
        return None;
    }
    Some(IssueSummary {
        id,
        identifier,
        title,
        url,
        state: read_state(&node["state"]),
        assignee: read_assignee(&node["assignee"]),
        team: read_team(&node["team"]),
        priority: read_priority(&node["priority"]),
        labels: read_labels(&node["labels"]),
    })
}

/// 完整 issue：摘要 + description + 评论列表（详情查询/更新返回的形态）。
#[derive(Debug, Clone)]
struct Issue {
    /// 基础摘要字段（必填校验由 `read_issue_summary` 完成）。
    summary: IssueSummary,
    /// 正文描述，仅接受字符串值（null/缺失为 `None`）。
    description: Option<String>,
    /// 已解析的评论 JSON 列表，缺失时为空数组。
    comments: Vec<Value>,
}

/// 在摘要 JSON 上追加 `description` 与 `comments` 两个键。
impl Issue {
    /// 在摘要 JSON 上追加 `description`/`comments` 后返回。
    fn to_json(&self) -> Value {
        let mut payload = self.summary.to_json();
        payload["description"] = json!(self.description);
        payload["comments"] = json!(self.comments);
        payload
    }
}

/// JS `readComment`.
///
/// 中文说明：从 GraphQL 节点读取评论，id 必填；仅当作者有 name 或
/// displayName 时才保留 user 字段（与 JS 一致）。
fn read_comment(node: &Value) -> Option<Value> {
    if !is_plain_object(node) {
        return None;
    }
    let id = read_trimmed_string(&node["id"]);
    if id.is_empty() {
        return None;
    }
    let body = match &node["body"] {
        Value::String(body) => body.clone(),
        _ => String::new(),
    };
    let user = super::parse::as_plain_object(&node["user"]).map(|user| {
        json!({
            "name": optional(&user["name"]),
            "displayName": optional(&user["displayName"]),
            "avatarUrl": optional(&user["avatarUrl"]),
        })
    });
    // JS keeps the user only when a name or display name survived.
    let user = user.filter(|user| user["name"].is_string() || user["displayName"].is_string());
    Some(json!({
        "id": id,
        "body": body,
        "createdAt": optional(&node["createdAt"]),
        "user": user,
    }))
}

/// 从 GraphQL 节点读取完整 issue：摘要校验失败返回 `None`，评论逐条解析。
fn read_issue(node: &Value) -> Option<Issue> {
    let summary = read_issue_summary(node)?;
    let comments = node["comments"]["nodes"]
        .as_array()
        .map(|nodes| nodes.iter().filter_map(read_comment).collect())
        .unwrap_or_default();
    Some(Issue {
        summary,
        description: match &node["description"] {
            Value::String(description) => Some(description.clone()),
            _ => None,
        },
        comments,
    })
}

/// 读取连接的 pageInfo：返回 (是否还有下一页, 下一页游标)；
/// pageInfo 缺失时视为 (false, None)。
fn read_page_info(connection: &Value) -> (bool, Option<String>) {
    let page_info = &connection["pageInfo"];
    if !is_plain_object(page_info) {
        return (false, None);
    }
    (
        matches!(page_info["hasNextPage"], Value::Bool(true)),
        optional(&page_info["endCursor"]),
    )
}

/// 把连接的 nodes 逐条解析为 issue 摘要 JSON，非法节点被静默丢弃。
fn read_issue_nodes(connection: &Value) -> Vec<Value> {
    connection["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(read_issue_summary)
                .map(|s| s.to_json())
                .collect()
        })
        .unwrap_or_default()
}

/// JS `withLinearToken`: resolves a token, runs the call, and maps a 401 to
/// `{ connected: false }` after clearing that workspace only.
///
/// 中文说明：先取有效 token（必要时刷新），拿不到直接返回
/// `{ connected: false }`；调用返回 401 时清除对应 workspace 的凭据并
/// 同样返回未连接，让上层 UI 引导重新授权。
async fn with_linear_token<F, Fut>(
    state: &Arc<LinearState>,
    workspace_id: Option<&str>,
    run: F,
) -> Result<Value, LinearError>
where
    F: FnOnce(Arc<LinearState>, String) -> Fut,
    Fut: Future<Output = Result<Value, LinearError>>,
{
    let outcome = match get_valid_linear_access_token(state, workspace_id).await {
        Ok(Some(token)) => run(state.clone(), token).await,
        Ok(None) => Ok(json!({ "connected": false })),
        Err(error) => Err(error),
    };
    match outcome {
        Err(error) if error.http_status() == Some(401) => {
            let failed = match workspace_id.map(str::trim).filter(|v| !v.is_empty()) {
                Some(id) => state.get_auth_by_workspace_id(id),
                None => state.get_auth(),
            };
            let workspace = failed
                .map(|auth| auth.workspace_id)
                .or_else(|| workspace_id.map(str::to_string));
            state.clear_auth(workspace.as_deref());
            Ok(json!({ "connected": false }))
        }
        other => other,
    }
}

/// 按引用（identifier 或 UUID）执行详情查询并解析为 `Issue`；
/// Linear 找不到该 issue 时返回 `Ok(None)`。
async fn fetch_issue_by_ref(
    state: &Arc<LinearState>,
    token: &str,
    reference: &IssueRef,
) -> Result<Option<Issue>, LinearError> {
    let data = fetch_linear_graphql(
        state,
        token,
        GET_QUERY,
        Some(&json!({ "id": reference.value })),
    )
    .await?;
    Ok(read_issue(&data["issue"]))
}

/// JS `listLinearIssues`.
///
/// 中文说明：查询词能解析成 issue 引用时直接查那条 issue（单条结果、无
/// 分页）；否则按过滤条件走搜索或列表查询，返回
/// `{ connected, issues, cursor, hasMore }`。
pub async fn list_linear_issues(
    state: &Arc<LinearState>,
    params: &ListIssuesParams,
) -> Result<Value, LinearError> {
    with_linear_token(state, None, |state, token| async move {
        if let Some(reference) = parse_linear_issue_ref(params.query.trim()) {
            let issue = fetch_issue_by_ref(&state, &token, &reference).await?;
            return Ok(json!({
                "connected": true,
                "issues": issue.iter().map(Issue::to_json).collect::<Vec<_>>(),
                "cursor": Value::Null,
                "hasMore": false,
            }));
        }

        let after = optional(&Value::String(params.cursor.clone()));
        let term = params.query.trim().to_string();
        let filter = build_issue_list_filter(params);
        let mut variables = json!({ "first": PAGE_SIZE });
        if let Some(filter) = filter {
            variables["filter"] = filter;
        }
        if let Some(after) = after {
            variables["after"] = json!(after);
        }

        if !term.is_empty() {
            variables["term"] = json!(term);
            let data = fetch_linear_graphql(&state, &token, SEARCH_QUERY, Some(&variables)).await?;
            let connection = data["searchIssues"].clone();
            let (has_more, cursor) = read_page_info(&connection);
            return Ok(json!({
                "connected": true,
                "issues": read_issue_nodes(&connection),
                "cursor": cursor,
                "hasMore": has_more,
            }));
        }

        let data = fetch_linear_graphql(&state, &token, LIST_QUERY, Some(&variables)).await?;
        let connection = data["issues"].clone();
        let (has_more, cursor) = read_page_info(&connection);
        Ok(json!({
            "connected": true,
            "issues": read_issue_nodes(&connection),
            "cursor": cursor,
            "hasMore": has_more,
        }))
    })
    .await
}

/// JS `getLinearIssue`.
///
/// 中文说明：按 id/identifier/URL 加载单条 issue；id 无法解析且非空时按
/// UUID 兜底，返回 `{ connected, issue }`（找不到时 issue 为 null）。
pub async fn get_linear_issue(state: &Arc<LinearState>, id: &str) -> Result<Value, LinearError> {
    let trimmed = id.trim();
    let reference = parse_linear_issue_ref(trimmed).or_else(|| {
        (!trimmed.is_empty()).then(|| IssueRef {
            kind: IssueRefKind::Id,
            value: trimmed.to_string(),
        })
    });
    let Some(reference) = reference else {
        return Ok(json!({ "connected": true, "issue": Value::Null }));
    };
    with_linear_token(state, None, |state, token| async move {
        let issue = fetch_issue_by_ref(&state, &token, &reference).await?;
        Ok(json!({
            "connected": true,
            "issue": issue.as_ref().map(Issue::to_json),
        }))
    })
    .await
}

/// JS `listLinearIssueStates`.
///
/// 中文说明：拉取团队 workflow 状态并按 `compare_workflow_states` 排序；
/// teamId 为空返回 INVALID 错误。
pub async fn list_linear_issue_states(
    state: &Arc<LinearState>,
    team_id: &str,
) -> Result<Value, LinearError> {
    let id = team_id.trim();
    if id.is_empty() {
        return Err(LinearError::invalid("teamId is required"));
    }
    with_linear_token(state, None, |state, token| async move {
        let data =
            fetch_linear_graphql(&state, &token, STATES_QUERY, Some(&json!({ "id": id }))).await?;
        let nodes = data["team"]["states"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut states: Vec<WorkflowState> = nodes.iter().filter_map(read_workflow_state).collect();
        states.sort_by(compare_workflow_states);
        Ok(json!({
            "connected": true,
            "states": states.iter().map(WorkflowState::to_json).collect::<Vec<_>>(),
        }))
    })
    .await
}

/// JS `updateLinearIssue`.
///
/// 中文说明：切换 issue 状态：identifier 引用先查详情解析出 UUID；目标
/// issue 不存在时返回 `{ connected: true, issue: null }` 而非报错。
pub async fn update_linear_issue(
    state: &Arc<LinearState>,
    id: &str,
    state_id: &str,
) -> Result<Value, LinearError> {
    let issue_id = id.trim();
    let next_state_id = state_id.trim();
    if issue_id.is_empty() || next_state_id.is_empty() {
        return Err(LinearError::invalid("id and stateId are required"));
    }
    let reference = parse_linear_issue_ref(issue_id).unwrap_or(IssueRef {
        kind: IssueRefKind::Id,
        value: issue_id.to_string(),
    });
    with_linear_token(state, None, |state, token| async move {
        // Identifiers resolve to UUIDs first: Linear's mutation rejects them.
        let resolved = if reference.kind == IssueRefKind::Identifier {
            fetch_issue_by_ref(&state, &token, &reference).await?
        } else {
            None
        };
        let resolved_id = resolved
            .map(|issue| issue.summary.id)
            .or_else(|| (reference.kind == IssueRefKind::Id).then(|| reference.value.clone()))
            .unwrap_or_default();
        if resolved_id.is_empty() {
            return Ok(json!({ "connected": true, "issue": Value::Null }));
        }
        let data = fetch_linear_graphql(
            &state,
            &token,
            ISSUE_UPDATE,
            Some(&json!({ "id": resolved_id, "input": { "stateId": next_state_id } })),
        )
        .await?;
        let issue = read_issue(&data["issueUpdate"]["issue"]);
        Ok(json!({
            "connected": true,
            "issue": issue.as_ref().map(Issue::to_json),
        }))
    })
    .await
}

/// JS `createLinearIssueComment`.
///
/// 中文说明：向 issue 写评论：issue 引用先解析为详情，body 仅接受非空白
/// 字符串；未连接或找不到 issue 时返回 null comment 而非错误。
pub async fn create_linear_issue_comment(
    state: &Arc<LinearState>,
    issue_id: &str,
    body: &Value,
    organization_id: Option<&str>,
) -> Result<Value, LinearError> {
    let text = match body {
        Value::String(text) => text.clone(),
        _ => String::new(),
    };
    let trimmed_issue_id = issue_id.trim();
    let reference = parse_linear_issue_ref(trimmed_issue_id).or_else(|| {
        (!trimmed_issue_id.is_empty()).then(|| IssueRef {
            kind: IssueRefKind::Id,
            value: trimmed_issue_id.to_string(),
        })
    });
    let reference = match reference {
        Some(reference) if !text.trim().is_empty() => reference,
        _ => return Ok(json!({ "connected": true, "comment": Value::Null })),
    };
    with_linear_token(state, organization_id, |state, token| async move {
        let Some(issue) = fetch_issue_by_ref(&state, &token, &reference).await? else {
            return Ok(json!({ "connected": true, "comment": Value::Null }));
        };
        let data = fetch_linear_graphql(
            &state,
            &token,
            COMMENT_CREATE,
            Some(&json!({ "input": { "issueId": issue.summary.id, "body": text } })),
        )
        .await?;
        let id = read_trimmed_string(&data["commentCreate"]["comment"]["id"]);
        Ok(json!({
            "connected": true,
            "comment": (!id.is_empty()).then(|| json!({ "id": id })),
        }))
    })
    .await
}
