//! `create_worktree`: explicit worktree creation with idempotent replays.
//!
//! External write. Conflicts never overwrite; a repeated request with the same
//! persisted metadata is a no-op that reconciles against the original
//! destination. The receipt survives a rendering failure from Rust.
use crate::response::{self, Class, Templates};
use crate::service::{self, CreateOutcome, Service};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};

pub const IMPLEMENTED: bool = true;
pub const TEMPLATE: &str = include_str!("../../assets/mcp/tools/create_worktree.txt.j2");

/// Argument hint used in invalid-arguments refusals.
const ARGS: &str = "Accepted fields: repo, name, base?, branch?, detached?, \
creator, session?, purpose.";

/// Typed success view rendered by the embedded template.
#[derive(Serialize)]
struct View {
    status: &'static str,
    key: String,
    path: String,
    branch: String,
    head: String,
    creator: String,
    purpose: String,
    created_at: String,
    warnings: Option<String>,
}

/// Display label for the branch axis; detached checkouts say so.
fn branch_label(branch: &Option<String>) -> String {
    branch.clone().unwrap_or_else(|| "detached".to_owned())
}

/// Rust-side receipt used when the template fails after a confirmed effect.
fn degraded(status: &str, key: &str, path: &str, branch: &str) -> String {
    format!(
        "{status} worktree {key}\nPath: {path}\nBranch: {branch}\n\
Presentation: degraded (presentation_failed).\n\
Do not repeat the mutation to repair this response.\n"
    )
}

pub fn definition() -> Value {
    json!({"name":"create_worktree",
        "description":"Create one Git worktree under the managed root and record it. External write: creates a directory and a branch (default aw/<name>), never resets an existing branch. A repeated identical request is a no-op; any conflict refuses without overwriting.",
        "inputSchema":{"type":"object","required":["repo","name","creator","purpose"],
            "properties":{
                "repo":{"type":"string","description":"Repository root or any worktree inside it."},
                "name":{"type":"string","maxLength":64,"pattern":"^[a-z0-9][a-z0-9-]*$","description":"Worktree name."},
                "base":{"type":"string","maxLength":200,"description":"Base ref or commit; repository HEAD when omitted."},
                "branch":{"type":"string","maxLength":200,"description":"Existing branch to check out; mutually exclusive with detached."},
                "detached":{"type":"boolean","description":"Check out base detached instead of a branch."},
                "creator":{"type":"string","maxLength":64,"description":"Creating harness attribution."},
                "session":{"type":"string","maxLength":128,"description":"Creating session identifier."},
                "purpose":{"type":"string","maxLength":200,"description":"Why the worktree exists."}},
            "additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true}})
}

pub async fn call(args: Value, templates: &Templates, service: &Service) -> CallToolResult {
    let Ok(parsed) = serde_json::from_value::<service::CreateArgs>(args) else {
        return response::invalid_arguments(templates, "create_worktree", ARGS);
    };
    match service.create_worktree(&parsed).await {
        Ok(CreateOutcome::Created {
            key,
            path,
            branch,
            head,
            creator,
            purpose,
            created_at,
            warnings,
        }) => {
            let path_text = path.display().to_string();
            let branch_text = branch_label(&branch);
            let view = View {
                status: "COMMITTED",
                warnings: response::join_warnings(&warnings),
                creator: creator.clone(),
                purpose: purpose
                    .as_deref()
                    .map(|p| response::bounded(p, 120))
                    .unwrap_or_else(|| "-".to_owned()),
                created_at: response::iso_utc(created_at),
                key,
                path: path_text.clone(),
                branch: branch_text.clone(),
                head,
            };
            match templates.render("create_worktree", &view, Class::Entity) {
                Ok(text) => response::text_result(text, false),
                Err(_) => response::text_result(
                    degraded("COMMITTED", &view.key, &path_text, &branch_text),
                    false,
                ),
            }
        }
        Ok(CreateOutcome::Noop {
            key,
            path,
            branch,
            head,
            creator,
            purpose,
            created_at,
        }) => {
            let view = View {
                status: "NOOP",
                warnings: None,
                creator,
                purpose: purpose
                    .as_deref()
                    .map(|p| response::bounded(p, 120))
                    .unwrap_or_else(|| "-".to_owned()),
                created_at: response::iso_utc(created_at),
                key,
                path: path.display().to_string(),
                branch: branch_label(&branch),
                head: head.unwrap_or_else(|| "unknown".to_owned()),
            };
            match templates.render("create_worktree", &view, Class::Entity) {
                Ok(text) => response::text_result(text, false),
                Err(_) => response::text_result(
                    degraded("NOOP", &view.key, &view.path, &view.branch),
                    false,
                ),
            }
        }
        Err(error) => response::failure(templates, &error),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;
    use crate::response::Templates;

    fn templates() -> Templates {
        Templates::new(&[
            ("create_worktree", TEMPLATE),
            ("error", crate::response::ERROR_TEMPLATE),
            ("outcome_unknown", crate::response::OUTCOME_UNKNOWN_TEMPLATE),
        ])
        .unwrap()
    }
    fn base_args() -> Value {
        json!({"repo":"/repo","name":"task-1","creator":"claude-code","purpose":"ship"})
    }

    #[tokio::test]
    async fn unknown_field_rejected() {
        let mut args = base_args();
        args["unexpected"] = json!(true);
        let result = call(args, &templates(), &Service::new().unwrap()).await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR invalid_arguments:"), "{text}");
    }

    #[tokio::test]
    async fn missing_required_field_rejected() {
        let mut args = base_args();
        args.as_object_mut().unwrap().remove("purpose");
        let result = call(args, &templates(), &Service::new().unwrap()).await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR invalid_arguments:"), "{text}");
    }

    #[tokio::test]
    async fn bad_name_refused_before_any_effect() {
        let mut args = base_args();
        args["name"] = json!("Bad_Name");
        let result = call(args, &templates(), &Service::new().unwrap()).await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR name_charset:"), "{text}");
    }

    #[tokio::test]
    async fn valid_arguments_reach_the_service() {
        // The refusal depends on the environment's configured root: with a
        // root the call proceeds to Git and is refused by it; without one it
        // is refused by the root check. Either way the typed ERROR proves the
        // call passed argument validation and reached the service.
        let env_home = std::env::var_os("AGENT_WORKTREE_HOME").map(std::path::PathBuf::from);
        let platform_home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        let expected = match crate::store::resolve_layout(env_home, platform_home) {
            Ok(layout) if layout.root.is_some() => "ERROR not_a_repository:",
            Ok(_) => "ERROR root_not_configured:",
            Err(_) => "ERROR invalid_config:",
        };
        let result = call(base_args(), &templates(), &Service::new().unwrap()).await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with(expected), "{text}");
    }

    #[test]
    fn template_fixture() {
        let view = View {
            status: "COMMITTED",
            key: "0123456789ab/task-1".to_owned(),
            path: "/tmp/w/demo--0123456789ab/task-1".to_owned(),
            branch: "aw/task-1".to_owned(),
            head: "0f1e2d3c4b5a6978879665544332211ff1e2d3c4".to_owned(),
            creator: "claude-code".to_owned(),
            purpose: "ship the release".to_owned(),
            created_at: "2027-01-15T08:00:00Z".to_owned(),
            warnings: None,
        };
        let text = templates()
            .render("create_worktree", &view, Class::Entity)
            .unwrap();
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/create_worktree.txt")
        );
    }

    #[test]
    fn degraded_receipt_keeps_the_facts() {
        let text = degraded(
            "COMMITTED",
            "ab12/task-1",
            "/w/task-1",
            "refs/heads/aw/task-1",
        );
        assert!(text.starts_with("COMMITTED worktree ab12/task-1\n"));
        assert!(text.contains("Do not repeat the mutation"));
    }
}
