//! Authoritative Rust tool registry. JSON discovery is an exported snapshot, not a second source.
use crate::response::{self, Class, Templates};
use crate::service::Service;
use mcp_presentation::Renderer;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};
#[path = "create_worktree.rs"]
mod tool_create_worktree;
#[path = "inspect_worktree.rs"]
mod tool_inspect_worktree;
#[path = "list_worktrees.rs"]
mod tool_list_worktrees;
#[path = "prune_worktrees.rs"]
mod tool_prune_worktrees;
#[path = "remove_worktree.rs"]
mod tool_remove_worktree;
// xtask:modules

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"name":"get_status", "description":"Report the product identity and release qualification status. Read-only; does not access files or external services.",
        "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}),
        tool_create_worktree::definition(),
        tool_list_worktrees::definition(),
        tool_inspect_worktree::definition(),
        tool_remove_worktree::definition(),
        tool_prune_worktrees::definition(),
        // xtask:definitions
    ]
}
pub fn templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "invalid_arguments",
            "ERROR invalid_arguments: get_status accepts an empty argument object.\n",
        ),
        ("error", response::ERROR_TEMPLATE),
        ("outcome_unknown", response::OUTCOME_UNKNOWN_TEMPLATE),
        ("create_worktree", tool_create_worktree::TEMPLATE),
        ("list_worktrees", tool_list_worktrees::TEMPLATE),
        ("inspect_worktree", tool_inspect_worktree::TEMPLATE),
        ("remove_worktree", tool_remove_worktree::TEMPLATE),
        (
            "remove_worktree_receipt",
            tool_remove_worktree::RECEIPT_TEMPLATE,
        ),
        (
            "remove_worktree_batch",
            tool_remove_worktree::BATCH_TEMPLATE,
        ),
        (
            "remove_worktree_batch_receipt",
            tool_remove_worktree::BATCH_RECEIPT_TEMPLATE,
        ),
        ("prune_worktrees", tool_prune_worktrees::TEMPLATE),
        // xtask:templates
    ]
}
pub fn incomplete() -> Vec<&'static str> {
    let statuses: &[(&str, bool)] = &[
        ("get_status", true),
        ("create_worktree", tool_create_worktree::IMPLEMENTED),
        ("list_worktrees", tool_list_worktrees::IMPLEMENTED),
        ("inspect_worktree", tool_inspect_worktree::IMPLEMENTED),
        ("remove_worktree", tool_remove_worktree::IMPLEMENTED),
        ("prune_worktrees", tool_prune_worktrees::IMPLEMENTED),
        // xtask:readiness
    ];
    statuses
        .iter()
        .filter(|(_, implemented)| !implemented)
        .map(|(name, _)| *name)
        .collect()
}
pub async fn call(
    name: &str,
    args: Value,
    identity: &Renderer,
    _templates: &Templates,
    service: &Service,
) -> Option<CallToolResult> {
    match name {
        "get_status" => {
            if !args.as_object().is_some_and(|a| a.is_empty()) {
                let text = _templates
                    .render("invalid_arguments", &(), Class::Ack)
                    .unwrap_or_else(|_| {
                        "ERROR invalid_arguments: no effect performed. Presentation: degraded.\n"
                            .to_owned()
                    });
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.is_error = Some(true);
                return Some(result);
            }
            let reply = identity.identity(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                crate::qualification(),
            );
            let mut result =
                CallToolResult::success(vec![ContentBlock::text(reply.text().to_owned())]);
            result.is_error = Some(reply.is_error());
            Some(result)
        }
        "create_worktree" => Some(tool_create_worktree::call(args, _templates, service).await),
        "list_worktrees" => Some(tool_list_worktrees::call(args, _templates, service).await),
        "inspect_worktree" => Some(tool_inspect_worktree::call(args, _templates, service).await),
        "remove_worktree" => Some(tool_remove_worktree::call(args, _templates, service).await),
        "prune_worktrees" => Some(tool_prune_worktrees::call(args, _templates, service).await),
        // xtask:routes
        _ => None,
    }
}
