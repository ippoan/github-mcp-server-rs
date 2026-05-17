//! Projects v2 (GraphQL のみ) — ci-dashboard `src/mcp/tools/projects.ts` 移植。
//!
//! Projects v2 は REST に surface が無く、すべて GraphQL。書込みは 3 段階の
//! resolve が要る:
//!   1. project number → projectId (`resolve_project_id`)
//!   2. issue/PR number → contentId (`resolve_issue_content_id`)
//!   3. field name → fieldId + optionId (`get_project_fields` 経由)
//!
//! Owner type: `repositoryOwner(login:)` を使い `Organization` / `User` の両方の
//! inline fragment を貼る。`organization(...)` resolver 単独だと user account login
//! ("yhonda-ohishi" 等) で `Could not resolve to an Organization` で hard fail する。

use reqwest::Client;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::github_api::{github_graphql, validate_org, GitHubApiError};
use crate::mcp_server::GithubMcp;

// --------------------------------------------------------------------------
// Helpers (plain async fns; tools call them via `&self.ctx().client` / token)
// --------------------------------------------------------------------------

async fn resolve_project_id(
    client: &Client,
    token: &str,
    org: &str,
    number: i64,
) -> Result<String, GitHubApiError> {
    validate_org(org)?;
    let query = r#"query($login:String!,$number:Int!){
      repositoryOwner(login:$login){
        ... on Organization { projectV2(number:$number){ id } }
        ... on User { projectV2(number:$number){ id } }
      }
    }"#;
    let data: serde_json::Value = github_graphql(
        client,
        token,
        query,
        serde_json::json!({ "login": org, "number": number }),
    )
    .await?;
    data.get("repositoryOwner")
        .and_then(|v| v.get("projectV2"))
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| GitHubApiError::Http {
            status: 404,
            body: format!("Project not found: {org}/projects/{number}"),
        })
}

async fn resolve_issue_content_id(
    client: &Client,
    token: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<String, GitHubApiError> {
    validate_org(owner)?;
    let query = r#"query($owner:String!,$repo:String!,$number:Int!){
      repository(owner:$owner, name:$repo){
        issueOrPullRequest(number:$number){
          ... on Issue { id }
          ... on PullRequest { id }
        }
      }
    }"#;
    let data: serde_json::Value = github_graphql(
        client,
        token,
        query,
        serde_json::json!({ "owner": owner, "repo": repo, "number": number }),
    )
    .await?;
    data.get("repository")
        .and_then(|v| v.get("issueOrPullRequest"))
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| GitHubApiError::Http {
            status: 404,
            body: format!("Issue/PR not found: {owner}/{repo}#{number}"),
        })
}

/// `[{ id, name, dataType, options?, configuration? }]` を返す。
async fn get_project_fields(
    client: &Client,
    token: &str,
    project_id: &str,
) -> Result<Vec<serde_json::Value>, GitHubApiError> {
    let query = r#"query($id:ID!){
      node(id:$id){
        ... on ProjectV2 {
          fields(first:50){
            nodes{
              __typename
              ... on ProjectV2FieldCommon { id name dataType }
              ... on ProjectV2SingleSelectField {
                id name dataType
                options{ id name }
              }
              ... on ProjectV2IterationField {
                id name dataType
                configuration{
                  iterations{ id title startDate duration }
                  completedIterations{ id title startDate duration }
                }
              }
            }
          }
        }
      }
    }"#;
    let data: serde_json::Value = github_graphql(
        client,
        token,
        query,
        serde_json::json!({ "id": project_id }),
    )
    .await?;
    Ok(data
        .get("node")
        .and_then(|v| v.get("fields"))
        .and_then(|v| v.get("nodes"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// `get_project` の出力で field を要約。single_select option / iteration を flatten。
fn summarize_field(f: &serde_json::Value) -> serde_json::Value {
    let mut base = serde_json::Map::new();
    base.insert(
        "id".into(),
        f.get("id").cloned().unwrap_or(serde_json::Value::Null),
    );
    base.insert(
        "name".into(),
        f.get("name").cloned().unwrap_or(serde_json::Value::Null),
    );
    base.insert(
        "dataType".into(),
        f.get("dataType")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    if let Some(opts) = f.get("options").and_then(|v| v.as_array()) {
        let trimmed: Vec<serde_json::Value> = opts
            .iter()
            .map(|o| {
                serde_json::json!({
                    "id": o.get("id"),
                    "name": o.get("name"),
                })
            })
            .collect();
        base.insert("options".into(), serde_json::Value::Array(trimmed));
    }
    if let Some(cfg) = f.get("configuration") {
        let mut iters: Vec<serde_json::Value> = Vec::new();
        for key in ["iterations", "completedIterations"] {
            if let Some(arr) = cfg.get(key).and_then(|v| v.as_array()) {
                for i in arr {
                    iters.push(serde_json::json!({
                        "id": i.get("id"),
                        "title": i.get("title"),
                        "startDate": i.get("startDate"),
                        "duration": i.get("duration"),
                    }));
                }
            }
        }
        if !iters.is_empty() {
            base.insert("iterations".into(), serde_json::Value::Array(iters));
        }
    }
    serde_json::Value::Object(base)
}

/// `list_project_items` の出力。content + field values を平坦化する。
fn format_item(node: &serde_json::Value) -> serde_json::Value {
    let content = node.get("content");
    let mut content_summary = serde_json::json!({ "type": "unknown" });
    if let Some(c) = content {
        let typename = c.get("__typename").and_then(|v| v.as_str()).unwrap_or("");
        match typename {
            "Issue" | "PullRequest" => {
                content_summary = serde_json::json!({
                    "type": if typename == "Issue" { "issue" } else { "pull_request" },
                    "repo": c.get("repository").and_then(|r| r.get("nameWithOwner")),
                    "number": c.get("number"),
                    "title": c.get("title"),
                    "state": c.get("state"),
                    "url": c.get("url"),
                });
            }
            "DraftIssue" => {
                content_summary = serde_json::json!({
                    "type": "draft_issue",
                    "title": c.get("title"),
                });
            }
            _ => {}
        }
    }

    let mut values = serde_json::Map::new();
    if let Some(arr) = node
        .get("fieldValues")
        .and_then(|v| v.get("nodes"))
        .and_then(|v| v.as_array())
    {
        for v in arr {
            let Some(fname) = v
                .get("field")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            else {
                continue;
            };
            let typename = v.get("__typename").and_then(|t| t.as_str()).unwrap_or("");
            let value = match typename {
                "ProjectV2ItemFieldTextValue" => v.get("text").cloned(),
                "ProjectV2ItemFieldNumberValue" => v.get("number").cloned(),
                "ProjectV2ItemFieldDateValue" => v.get("date").cloned(),
                "ProjectV2ItemFieldSingleSelectValue" => v.get("name").cloned(),
                "ProjectV2ItemFieldIterationValue" => v.get("title").cloned(),
                _ => None,
            };
            if let Some(v) = value {
                values.insert(fname.to_string(), v);
            }
        }
    }

    serde_json::json!({
        "item_id": node.get("id"),
        "item_type": node.get("type"),
        "content": content_summary,
        "fields": serde_json::Value::Object(values),
    })
}

// --------------------------------------------------------------------------
// Tool args
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListOrgProjectsArgs {
    /// Organization logins (e.g. ["ippoan", "ohishi-exp", "yhonda-ohishi"]).
    pub orgs: Vec<String>,
    /// Max projects per org (1–100, default 50).
    #[serde(default)]
    pub first: Option<u32>,
    /// Include closed projects (default false).
    #[serde(default)]
    pub include_closed: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetProjectArgs {
    /// Organization login.
    pub org: String,
    /// Project number (the integer in the project URL).
    pub number: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListProjectItemsArgs {
    /// Organization login.
    pub org: String,
    /// Project number.
    pub number: i64,
    /// Max items to return (1–100, default 50).
    #[serde(default)]
    pub first: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddIssueToProjectArgs {
    /// Project owner organization (e.g. 'ippoan').
    pub org: String,
    /// Project number.
    pub project_number: i64,
    /// Repository hosting the issue/PR (e.g. 'rust-alc-api' or 'ippoan/rust-alc-api').
    pub repo: String,
    /// Issue or PR number to add.
    pub issue_number: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveProjectItemArgs {
    /// Project owner organization.
    pub org: String,
    /// Project number.
    pub project_number: i64,
    /// Item node ID (from `list_project_items` or `add_issue_to_project`).
    pub item_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetProjectItemFieldArgs {
    /// Project owner organization.
    pub org: String,
    /// Project number.
    pub project_number: i64,
    /// Item node ID.
    pub item_id: String,
    /// Field name (matches `name` from `get_project`).
    pub field_name: String,
    /// New field value (string for text/date/single_select/iteration, number for number, null to clear).
    /// `serde_json::Value` で受けて null/string/number を 1 つで扱う。
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateProjectFieldArgs {
    /// Project owner organization.
    pub org: String,
    /// Project number.
    pub project_number: i64,
    /// Field name.
    pub name: String,
    /// Field data type: "text" | "number" | "date" | "single_select".
    pub data_type: String,
    /// Required when data_type='single_select': option names.
    #[serde(default)]
    pub single_select_options: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateProjectArgs {
    /// Organization login (e.g. 'ippoan').
    pub org: String,
    /// Project title.
    pub title: String,
    /// Optional short description (applied via a second `updateProjectV2` mutation).
    #[serde(default)]
    pub short_description: Option<String>,
}

// --------------------------------------------------------------------------
// Tool router
// --------------------------------------------------------------------------

#[tool_router(router = projects_router, vis = "pub(crate)")]
impl GithubMcp {
    /// List Projects v2 across one or more orgs.
    #[tool(
        description = "List GitHub project boards / kanban boards / roadmaps (Projects v2) across one or more organizations. Use when you need to find existing planning boards, sprint boards, or cross-repo coordination views. Returns number/title/url/closed for each project, grouped by org. Use `get_project` afterwards to inspect fields / columns."
    )]
    async fn list_org_projects(
        &self,
        Parameters(args): Parameters<ListOrgProjectsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if args.orgs.is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "orgs must be a non-empty array",
                None,
            ));
        }
        for o in &args.orgs {
            validate_org(o)?;
        }
        let first = args.first.unwrap_or(50).clamp(1, 100);
        let include_closed = args.include_closed.unwrap_or(false);
        let query = r#"query($login:String!,$first:Int!){
          repositoryOwner(login:$login){
            ... on Organization {
              projectsV2(first:$first, orderBy:{field:NUMBER,direction:DESC}){
                nodes{ id number title url closed shortDescription }
              }
            }
            ... on User {
              projectsV2(first:$first, orderBy:{field:NUMBER,direction:DESC}){
                nodes{ id number title url closed shortDescription }
              }
            }
          }
        }"#;
        let mut per_org: Vec<serde_json::Value> = Vec::with_capacity(args.orgs.len());
        // Sequential rather than join_all to keep the surface area small —
        // 3 orgs * 1 query each is fine, and parallel join would need pinning.
        for org in &args.orgs {
            let data: serde_json::Value = github_graphql(
                &self.ctx().client,
                &self.ctx().github_token,
                query,
                serde_json::json!({ "login": org, "first": first }),
            )
            .await?;
            let nodes = data
                .get("repositoryOwner")
                .and_then(|v| v.get("projectsV2"))
                .and_then(|v| v.get("nodes"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let filtered: Vec<serde_json::Value> = nodes
                .iter()
                .filter(|p| {
                    include_closed || !p.get("closed").and_then(|v| v.as_bool()).unwrap_or(false)
                })
                .map(|p| {
                    serde_json::json!({
                        "number": p.get("number"),
                        "title": p.get("title"),
                        "url": p.get("url"),
                        "closed": p.get("closed"),
                        "shortDescription": p.get("shortDescription"),
                    })
                })
                .collect();
            per_org.push(serde_json::json!({
                "org": org,
                "projects": filtered,
            }));
        }
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&per_org).unwrap_or_default(),
        )]))
    }

    /// Get a Project's metadata + field definitions (incl. single_select options / iterations).
    #[tool(
        description = "Inspect a GitHub project board / kanban / roadmap (Projects v2): metadata and full field / column definitions, including single-select options (Status: Todo/In Progress/Done etc.) and iteration values (sprints). Required before `set_project_item_field` if you need to know valid column/option names."
    )]
    async fn get_project(
        &self,
        Parameters(args): Parameters<GetProjectArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        validate_org(&args.org)?;
        let query = r#"query($login:String!,$number:Int!){
          repositoryOwner(login:$login){ ... ProjectDetail }
        }
        fragment ProjectDetail on ProjectV2Owner {
          projectV2(number:$number){
            id number title url closed shortDescription
            fields(first:50){
              nodes{
                __typename
                ... on ProjectV2FieldCommon { id name dataType }
                ... on ProjectV2SingleSelectField {
                  id name dataType options{ id name }
                }
                ... on ProjectV2IterationField {
                  id name dataType
                  configuration{
                    iterations{ id title startDate duration }
                    completedIterations{ id title startDate duration }
                  }
                }
              }
            }
          }
        }"#;
        let data: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            query,
            serde_json::json!({ "login": args.org, "number": args.number }),
        )
        .await?;
        let p = data.get("repositoryOwner").and_then(|v| v.get("projectV2"));
        let Some(p) = p else {
            return Err(GitHubApiError::Http {
                status: 404,
                body: format!("Project not found: {}/projects/{}", args.org, args.number),
            }
            .into());
        };
        if p.is_null() {
            return Err(GitHubApiError::Http {
                status: 404,
                body: format!("Project not found: {}/projects/{}", args.org, args.number),
            }
            .into());
        }
        let fields: Vec<serde_json::Value> = p
            .get("fields")
            .and_then(|v| v.get("nodes"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(summarize_field).collect())
            .unwrap_or_default();
        let result = serde_json::json!({
            "id": p.get("id"),
            "number": p.get("number"),
            "title": p.get("title"),
            "url": p.get("url"),
            "closed": p.get("closed"),
            "shortDescription": p.get("shortDescription"),
            "fields": fields,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// List items (issues/PRs/draft) attached to a Project with their field values.
    #[tool(
        description = "List cards / items on a GitHub project board (Projects v2) — the issues, PRs, and draft items currently tracked on the kanban / roadmap. Each item includes its current field values (Status, Priority, Epic, Iteration, etc.), so use this to see what is in each kanban column or what is assigned to a given Epic."
    )]
    async fn list_project_items(
        &self,
        Parameters(args): Parameters<ListProjectItemsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        validate_org(&args.org)?;
        let first = args.first.unwrap_or(50).clamp(1, 100);
        let query = r#"query($login:String!,$number:Int!,$first:Int!){
          repositoryOwner(login:$login){ ... ProjectItems }
        }
        fragment ProjectItems on ProjectV2Owner {
          projectV2(number:$number){
            items(first:$first){
              nodes{
                id
                type
                content{
                  __typename
                  ... on Issue { number title url state repository{ nameWithOwner } }
                  ... on PullRequest { number title url state repository{ nameWithOwner } }
                  ... on DraftIssue { title }
                }
                fieldValues(first:30){
                  nodes{
                    __typename
                    ... on ProjectV2ItemFieldTextValue {
                      text field{ ... on ProjectV2FieldCommon { name } }
                    }
                    ... on ProjectV2ItemFieldNumberValue {
                      number field{ ... on ProjectV2FieldCommon { name } }
                    }
                    ... on ProjectV2ItemFieldDateValue {
                      date field{ ... on ProjectV2FieldCommon { name } }
                    }
                    ... on ProjectV2ItemFieldSingleSelectValue {
                      name optionId field{ ... on ProjectV2FieldCommon { name } }
                    }
                    ... on ProjectV2ItemFieldIterationValue {
                      title iterationId
                      field{ ... on ProjectV2FieldCommon { name } }
                    }
                  }
                }
              }
            }
          }
        }"#;
        let data: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            query,
            serde_json::json!({
                "login": args.org,
                "number": args.number,
                "first": first,
            }),
        )
        .await?;
        let nodes = data
            .get("repositoryOwner")
            .and_then(|v| v.get("projectV2"))
            .and_then(|v| v.get("items"))
            .and_then(|v| v.get("nodes"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let result: Vec<serde_json::Value> = nodes.iter().map(format_item).collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Add an issue/PR to a Project. Resolves project number + issue number internally.
    #[tool(
        description = "Add an issue (or PR) as a card to a GitHub project board / kanban (Projects v2). Use for cross-repo planning: putting issues from multiple repositories onto a single tracking board, assigning work to a sprint/Epic, or building a roadmap. Returns the new item's id, which is needed by `set_project_item_field` and `remove_project_item`."
    )]
    async fn add_issue_to_project(
        &self,
        Parameters(args): Parameters<AddIssueToProjectArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        validate_org(&args.org)?;
        let r = crate::github_api::parse_and_validate_repo(&args.repo)?;
        let project_id = resolve_project_id(
            &self.ctx().client,
            &self.ctx().github_token,
            &args.org,
            args.project_number,
        )
        .await?;
        let content_id = resolve_issue_content_id(
            &self.ctx().client,
            &self.ctx().github_token,
            &r.owner,
            &r.repo,
            args.issue_number,
        )
        .await?;
        let mutation = r#"mutation($projectId:ID!,$contentId:ID!){
          addProjectV2ItemById(input:{projectId:$projectId, contentId:$contentId}){
            item{ id }
          }
        }"#;
        let data: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            mutation,
            serde_json::json!({ "projectId": project_id, "contentId": content_id }),
        )
        .await?;
        let item_id = data
            .get("addProjectV2ItemById")
            .and_then(|v| v.get("item"))
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let result = serde_json::json!({
            "item_id": item_id,
            "project_id": project_id,
            "content_id": content_id,
            "repo": format!("{}/{}", r.owner, r.repo),
            "issue_number": args.issue_number,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Remove an item from a Project (does not delete the issue).
    #[tool(
        description = "Remove a card / item from a GitHub project board / kanban (Projects v2). Detaches the issue from the board but does not delete the underlying issue/PR."
    )]
    async fn remove_project_item(
        &self,
        Parameters(args): Parameters<RemoveProjectItemArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let project_id = resolve_project_id(
            &self.ctx().client,
            &self.ctx().github_token,
            &args.org,
            args.project_number,
        )
        .await?;
        let mutation = r#"mutation($projectId:ID!,$itemId:ID!){
          deleteProjectV2Item(input:{projectId:$projectId, itemId:$itemId}){
            deletedItemId
          }
        }"#;
        let data: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            mutation,
            serde_json::json!({ "projectId": project_id, "itemId": args.item_id }),
        )
        .await?;
        let deleted = data
            .get("deleteProjectV2Item")
            .and_then(|v| v.get("deletedItemId"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let result = serde_json::json!({ "deleted_item_id": deleted });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Update a field value on a Project item (single_select/iteration resolved by name).
    #[tool(
        description = "Set or change a field on a card / item on a GitHub project board (Projects v2). Use to move a card between kanban columns (Status: Todo → In Progress → Done), set Priority (P0/P1), assign to an Epic, set a sprint/Iteration, or update any custom planning field. Specify the field by name; for single_select / iteration fields, pass the option name / iteration title in `value` (resolved to the underlying option/iteration ID internally). For text/number/date, pass the literal value. Pass `value: null` to clear the field."
    )]
    async fn set_project_item_field(
        &self,
        Parameters(args): Parameters<SetProjectItemFieldArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let project_id = resolve_project_id(
            &self.ctx().client,
            &self.ctx().github_token,
            &args.org,
            args.project_number,
        )
        .await?;
        let fields =
            get_project_fields(&self.ctx().client, &self.ctx().github_token, &project_id).await?;
        let Some(field) = fields
            .iter()
            .find(|f| f.get("name").and_then(|v| v.as_str()) == Some(args.field_name.as_str()))
        else {
            let available: Vec<&str> = fields
                .iter()
                .filter_map(|f| f.get("name").and_then(|v| v.as_str()))
                .collect();
            return Err(GitHubApiError::Http {
                status: 404,
                body: format!(
                    "Field not found: {}. Available: {}",
                    args.field_name,
                    available.join(", ")
                ),
            }
            .into());
        };
        let field_id = field
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let data_type = field
            .get("dataType")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // `value: null` → clearProjectV2ItemFieldValue
        if args.value.is_null() {
            let mutation = r#"mutation($projectId:ID!,$itemId:ID!,$fieldId:ID!){
              clearProjectV2ItemFieldValue(input:{
                projectId:$projectId, itemId:$itemId, fieldId:$fieldId
              }){ projectV2Item{ id } }
            }"#;
            let data: serde_json::Value = github_graphql(
                &self.ctx().client,
                &self.ctx().github_token,
                mutation,
                serde_json::json!({
                    "projectId": project_id,
                    "itemId": args.item_id,
                    "fieldId": field_id,
                }),
            )
            .await?;
            let cleared_id = data
                .get("clearProjectV2ItemFieldValue")
                .and_then(|v| v.get("projectV2Item"))
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let result = serde_json::json!({
                "item_id": cleared_id,
                "field": args.field_name,
                "cleared": true,
            });
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        // Build value input by data type.
        let mut value_input = serde_json::Map::new();
        match data_type.as_str() {
            "TEXT" => {
                let Some(s) = args.value.as_str() else {
                    return Err(GitHubApiError::Http {
                        status: 400,
                        body: format!("Field {} (TEXT) requires a string value", args.field_name),
                    }
                    .into());
                };
                value_input.insert("text".into(), serde_json::Value::String(s.to_string()));
            }
            "NUMBER" => {
                let n = if let Some(n) = args.value.as_f64() {
                    n
                } else if let Some(s) = args.value.as_str() {
                    s.parse::<f64>().map_err(|_| GitHubApiError::Http {
                        status: 400,
                        body: format!(
                            "Field {} (NUMBER) requires a numeric value",
                            args.field_name
                        ),
                    })?
                } else {
                    return Err(GitHubApiError::Http {
                        status: 400,
                        body: format!(
                            "Field {} (NUMBER) requires a numeric value",
                            args.field_name
                        ),
                    }
                    .into());
                };
                value_input.insert(
                    "number".into(),
                    serde_json::Number::from_f64(n)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            "DATE" => {
                let Some(s) = args.value.as_str() else {
                    return Err(GitHubApiError::Http {
                        status: 400,
                        body: format!(
                            "Field {} (DATE) requires an ISO date string (YYYY-MM-DD)",
                            args.field_name
                        ),
                    }
                    .into());
                };
                value_input.insert("date".into(), serde_json::Value::String(s.to_string()));
            }
            "SINGLE_SELECT" => {
                let option_name = args
                    .value
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| args.value.to_string());
                let opts = field.get("options").and_then(|v| v.as_array());
                let opt = opts.and_then(|arr| {
                    arr.iter().find(|o| {
                        o.get("name").and_then(|v| v.as_str()) == Some(option_name.as_str())
                    })
                });
                let Some(opt) = opt else {
                    let avail: Vec<&str> = opts
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|o| o.get("name").and_then(|v| v.as_str()))
                                .collect()
                        })
                        .unwrap_or_default();
                    return Err(GitHubApiError::Http {
                        status: 404,
                        body: format!(
                            "Option not found on {}: {}. Available: {}",
                            args.field_name,
                            option_name,
                            avail.join(", ")
                        ),
                    }
                    .into());
                };
                let opt_id = opt
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                value_input.insert(
                    "singleSelectOptionId".into(),
                    serde_json::Value::String(opt_id),
                );
            }
            "ITERATION" => {
                let iter_title = args
                    .value
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| args.value.to_string());
                // Merge iterations + completedIterations.
                let cfg = field.get("configuration");
                let mut all: Vec<&serde_json::Value> = Vec::new();
                for key in ["iterations", "completedIterations"] {
                    if let Some(arr) = cfg.and_then(|c| c.get(key)).and_then(|v| v.as_array()) {
                        for i in arr {
                            all.push(i);
                        }
                    }
                }
                let it = all
                    .iter()
                    .find(|i| i.get("title").and_then(|v| v.as_str()) == Some(iter_title.as_str()));
                let Some(it) = it else {
                    let avail: Vec<&str> = all
                        .iter()
                        .filter_map(|i| i.get("title").and_then(|v| v.as_str()))
                        .collect();
                    return Err(GitHubApiError::Http {
                        status: 404,
                        body: format!(
                            "Iteration not found on {}: {}. Available: {}",
                            args.field_name,
                            iter_title,
                            avail.join(", ")
                        ),
                    }
                    .into());
                };
                let it_id = it
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                value_input.insert("iterationId".into(), serde_json::Value::String(it_id));
            }
            other => {
                return Err(GitHubApiError::Http {
                    status: 400,
                    body: format!(
                        "Field {} has unsupported dataType {} for set_project_item_field",
                        args.field_name, other
                    ),
                }
                .into());
            }
        }

        let mutation = r#"mutation($projectId:ID!,$itemId:ID!,$fieldId:ID!,$value:ProjectV2FieldValue!){
          updateProjectV2ItemFieldValue(input:{
            projectId:$projectId, itemId:$itemId, fieldId:$fieldId, value:$value
          }){ projectV2Item{ id } }
        }"#;
        let data: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            mutation,
            serde_json::json!({
                "projectId": project_id,
                "itemId": args.item_id,
                "fieldId": field_id,
                "value": serde_json::Value::Object(value_input),
            }),
        )
        .await?;
        let item_id = data
            .get("updateProjectV2ItemFieldValue")
            .and_then(|v| v.get("projectV2Item"))
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let result = serde_json::json!({
            "item_id": item_id,
            "field": args.field_name,
            "dataType": data_type,
            "value": args.value,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Create a custom field (text/number/date/single_select).
    #[tool(
        description = "Add a custom column / field to a GitHub project board (Projects v2): Status column (single_select), Priority, Epic label (text), due date, estimate (number), etc. For `single_select`, supply `single_select_options` (array of option names) — these become the kanban column values. `iteration` (sprint) fields cannot be created here — make them in the GitHub UI."
    )]
    async fn create_project_field(
        &self,
        Parameters(args): Parameters<CreateProjectFieldArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let project_id = resolve_project_id(
            &self.ctx().client,
            &self.ctx().github_token,
            &args.org,
            args.project_number,
        )
        .await?;
        let gql_type = match args.data_type.as_str() {
            "text" => "TEXT",
            "number" => "NUMBER",
            "date" => "DATE",
            "single_select" => "SINGLE_SELECT",
            other => {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("unknown data_type: {other} (must be text/number/date/single_select)"),
                    None,
                ));
            }
        };
        let mut input = serde_json::Map::new();
        input.insert("projectId".into(), serde_json::Value::String(project_id));
        input.insert(
            "dataType".into(),
            serde_json::Value::String(gql_type.to_string()),
        );
        input.insert("name".into(), serde_json::Value::String(args.name.clone()));
        if args.data_type == "single_select" {
            let Some(opts) = args.single_select_options.as_ref() else {
                return Err(GitHubApiError::Http {
                    status: 400,
                    body: "single_select_options is required for data_type='single_select'"
                        .to_string(),
                }
                .into());
            };
            if opts.is_empty() {
                return Err(GitHubApiError::Http {
                    status: 400,
                    body: "single_select_options is required for data_type='single_select'"
                        .to_string(),
                }
                .into());
            }
            // GitHub requires a color + description per option. Default GRAY / empty.
            let options: Vec<serde_json::Value> = opts
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "name": n,
                        "color": "GRAY",
                        "description": "",
                    })
                })
                .collect();
            input.insert(
                "singleSelectOptions".into(),
                serde_json::Value::Array(options),
            );
        }
        let mutation = r#"mutation($input:CreateProjectV2FieldInput!){
          createProjectV2Field(input:$input){
            projectV2Field{
              __typename
              ... on ProjectV2FieldCommon { id name dataType }
              ... on ProjectV2SingleSelectField {
                id name dataType options{ id name }
              }
            }
          }
        }"#;
        let data: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            mutation,
            serde_json::json!({ "input": serde_json::Value::Object(input) }),
        )
        .await?;
        let result = serde_json::json!({
            "field": data
                .get("createProjectV2Field")
                .and_then(|v| v.get("projectV2Field"))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Create a new Project under an org; optional `short_description` is applied via a follow-up `updateProjectV2`.
    #[tool(
        description = "Create a new GitHub project board / kanban / roadmap / planning view (Projects v2) under an organization. Use for spinning up a new tracking board, a sprint planning surface, a milestone board, or a cross-repo coordination view (e.g. one Epic spanning multiple repositories). Returns the new project's id/number/title/url — `number` can be fed directly into `add_issue_to_project` / `set_project_item_field` / `create_project_field`. If `short_description` is provided, a follow-up `updateProjectV2` mutation is issued (the create mutation does not accept it). On that follow-up failing, the created project is still returned with a `warning` field."
    )]
    async fn create_project(
        &self,
        Parameters(args): Parameters<CreateProjectArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        validate_org(&args.org)?;
        if args.title.is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "title must not be empty",
                None,
            ));
        }
        // Step 1: resolve ownerId via repositoryOwner (works for User + Organization).
        let owner_query = r#"query($login:String!){ repositoryOwner(login:$login){ id } }"#;
        let owner_data: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            owner_query,
            serde_json::json!({ "login": args.org }),
        )
        .await?;
        let owner_id = owner_data
            .get("repositoryOwner")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let Some(owner_id) = owner_id else {
            return Err(GitHubApiError::Http {
                status: 404,
                body: format!("Owner not found: {}", args.org),
            }
            .into());
        };

        // Step 2: createProjectV2 (title only).
        let create_mutation = r#"mutation($ownerId:ID!,$title:String!){
          createProjectV2(input:{ownerId:$ownerId, title:$title}){
            projectV2{ id number title url shortDescription }
          }
        }"#;
        let created: serde_json::Value = github_graphql(
            &self.ctx().client,
            &self.ctx().github_token,
            create_mutation,
            serde_json::json!({ "ownerId": owner_id, "title": args.title }),
        )
        .await?;
        let project = created
            .get("createProjectV2")
            .and_then(|v| v.get("projectV2"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let project_id = project
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let project_number = project
            .get("number")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let mut result = serde_json::json!({
            "id": project.get("id"),
            "number": project_number,
            "title": project.get("title"),
            "url": project.get("url"),
            "shortDescription": project.get("shortDescription"),
        });

        // Step 3 (optional): apply shortDescription via updateProjectV2.
        if let Some(desc) = args.short_description {
            let update_mutation = r#"mutation($id:ID!,$desc:String!){
              updateProjectV2(input:{projectId:$id, shortDescription:$desc}){
                projectV2{ shortDescription }
              }
            }"#;
            match github_graphql::<serde_json::Value>(
                &self.ctx().client,
                &self.ctx().github_token,
                update_mutation,
                serde_json::json!({ "id": project_id, "desc": desc }),
            )
            .await
            {
                Ok(updated) => {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "shortDescription".into(),
                            updated
                                .get("updateProjectV2")
                                .and_then(|v| v.get("projectV2"))
                                .and_then(|v| v.get("shortDescription"))
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        );
                    }
                }
                Err(e) => {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "warning".into(),
                            serde_json::Value::String(format!(
                                "Project created (number={project_number}) but setting shortDescription failed: {e}"
                            )),
                        );
                    }
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}
