//! Port of `server/lib/linear/issues.js` — list/search/get issues, team
//! workflow states, `issueUpdate` (identifier→UUID resolution first), and
//! `commentCreate` for session status comments.

use std::sync::Arc;

use serde_json::{Value, json};

use super::client::{fetch_linear_graphql, get_valid_linear_access_token};
use super::parse::{is_plain_object, read_finite_number, read_trimmed_string};
use super::{LinearError, LinearState};

const PAGE_SIZE: i64 = 50;
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

const COMMENT_CREATE: &str = r#"
  mutation CommentCreate($input: CommentCreateInput!) {
    commentCreate(input: $input) {
      success
      comment { id }
    }
  }
"#;

const STATES_QUERY: &str = r#"
  query TeamWorkflowStates($id: String!) {
    team(id: $id) {
      states(first: 50) {
        nodes { id name type position }
      }
    }
  }
"#;

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

#[derive(Debug, Clone, PartialEq)]
pub struct IssueRef {
    pub kind: IssueRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueRefKind {
    Identifier,
    Id,
}

/// JS `parseLinearIssueRef`: identifier, Linear issue URL, or UUID.
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

fn matches_ci(hay: &[u8], position: usize, needle: &[u8]) -> bool {
    hay.len() >= position + needle.len()
        && hay[position..position + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Filters for `listLinearIssues`.
#[derive(Debug, Clone, Default)]
pub struct ListIssuesParams {
    pub query: String,
    pub cursor: String,
    pub status: String,
    pub assignee: String,
    pub team_id: String,
    pub priority: String,
}

/// JS `buildIssueListFilter`.
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

fn read_list_status(value: &str) -> String {
    let status = value.trim();
    if status == "all" || list_status_state(status).is_object() {
        status.to_string()
    } else {
        "open".to_string()
    }
}

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

fn read_list_assignee(value: &str) -> String {
    match value.trim() {
        "me" | "any" => value.trim().to_string(),
        _ => "any".to_string(),
    }
}

fn read_list_priority(value: &str) -> String {
    match value.trim() {
        "none" | "urgent" | "high" | "medium" | "low" => value.trim().to_string(),
        _ => "all".to_string(),
    }
}

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

#[derive(Debug, Clone, PartialEq)]
struct IssueState {
    id: Option<String>,
    name: Option<String>,
    kind: Option<String>,
}

impl IssueState {
    fn to_json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "type": self.kind })
    }
}

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

fn optional(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub position: f64,
}

impl WorkflowState {
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

#[derive(Debug, Clone, PartialEq)]
struct Assignee {
    name: Option<String>,
    display_name: Option<String>,
    avatar_url: Option<String>,
}

impl Assignee {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "displayName": self.display_name,
            "avatarUrl": self.avatar_url,
        })
    }
}

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

#[derive(Debug, Clone, PartialEq)]
pub struct LinearTeam {
    pub id: String,
    pub key: String,
    pub name: String,
}

impl LinearTeam {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "key": self.key, "name": self.name })
    }
}

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
fn read_priority(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    if number.fract() != 0.0 || !(0.0..=4.0).contains(&number) {
        return None;
    }
    Some(number as i64)
}

/// JS `readLabelColor`: normalized `#rrggbb`.
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

fn read_labels(value: &Value) -> Vec<Value> {
    let nodes = value["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| value.as_array().cloned().unwrap_or_default());
    nodes.iter().filter_map(read_label).collect()
}

#[derive(Debug, Clone)]
struct IssueSummary {
    id: String,
    identifier: String,
    title: String,
    url: String,
    state: Option<IssueState>,
    assignee: Option<Assignee>,
    team: Option<LinearTeam>,
    priority: Option<i64>,
    labels: Vec<Value>,
}

impl IssueSummary {
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

#[derive(Debug, Clone)]
struct Issue {
    summary: IssueSummary,
    description: Option<String>,
    comments: Vec<Value>,
}

impl Issue {
    fn to_json(&self) -> Value {
        let mut payload = self.summary.to_json();
        payload["description"] = json!(self.description);
        payload["comments"] = json!(self.comments);
        payload
    }
}

/// JS `readComment`.
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
