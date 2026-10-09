//! ACP `session/update` plan notifications from the todo board.
//!
//! After a `write_todo_list` call the session's live board (the
//! [`crate::agent::tools::todo::TODO_LIST`] mirror) is projected into an
//! ACP [`Plan`] so editor clients can render the agent's checklist. ACP
//! plans are full replacements: every update carries the whole list.
//!
//! The mapping is pure ([`plan_from_todos`]); the bridge in `acp::mod`
//! only decides when to send it ([`is_plan_tool`]).

use agent_client_protocol::schema::v1::{Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus};

use crate::agent::tools::todo::TodoItem;

/// Tool names whose result changes the todo board and so warrants a plan
/// update to the client.
pub(crate) fn is_plan_tool(name: &str) -> bool {
    name == "write_todo_list"
}

/// Map a board status onto the ACP plan status. The board uses the
/// normalized issue vocabulary (`open`, `in_progress`, `blocked`, `done`,
/// `cancelled`); ACP only knows pending / in-progress / completed, so
/// `blocked` reads as pending and both terminal states read as completed.
pub(crate) fn plan_status(status: &str) -> PlanEntryStatus {
    match crate::extras::issue_db::normalize_status(status) {
        Some("in_progress") => PlanEntryStatus::InProgress,
        Some("done") | Some("cancelled") => PlanEntryStatus::Completed,
        _ => PlanEntryStatus::Pending,
    }
}

/// Map a board priority onto the ACP plan priority; anything not high or
/// low (including unrecognised input) is medium.
pub(crate) fn plan_priority(priority: &str) -> PlanEntryPriority {
    match crate::extras::issue_db::normalize_priority(priority) {
        Some("high") => PlanEntryPriority::High,
        Some("low") => PlanEntryPriority::Low,
        _ => PlanEntryPriority::Medium,
    }
}

/// Project the board into a full ACP plan, preserving order.
pub(crate) fn plan_from_todos(items: &[TodoItem]) -> Plan {
    Plan::new(
        items
            .iter()
            .map(|t| {
                PlanEntry::new(
                    t.content.clone(),
                    plan_priority(&t.priority),
                    plan_status(&t.status),
                )
            })
            .collect(),
    )
}

/// Snapshot the current board mirror as an ACP plan.
pub(crate) fn current_plan() -> Plan {
    let items = crate::agent::tools::todo::TODO_LIST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    plan_from_todos(&items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: &str, priority: &str) -> TodoItem {
        TodoItem {
            content: content.into(),
            status: status.into(),
            priority: priority.into(),
        }
    }

    #[test]
    fn only_the_todo_tool_triggers_a_plan() {
        assert!(is_plan_tool("write_todo_list"));
        assert!(!is_plan_tool("issue"));
        assert!(!is_plan_tool("bash"));
    }

    #[test]
    fn statuses_map_onto_the_acp_vocabulary() {
        assert_eq!(plan_status("open"), PlanEntryStatus::Pending);
        assert_eq!(plan_status("pending"), PlanEntryStatus::Pending);
        assert_eq!(plan_status("blocked"), PlanEntryStatus::Pending);
        assert_eq!(plan_status("in_progress"), PlanEntryStatus::InProgress);
        assert_eq!(plan_status("done"), PlanEntryStatus::Completed);
        assert_eq!(plan_status("completed"), PlanEntryStatus::Completed);
        assert_eq!(plan_status("cancelled"), PlanEntryStatus::Completed);
        assert_eq!(plan_status("???"), PlanEntryStatus::Pending);
    }

    #[test]
    fn priorities_map_with_medium_as_default() {
        assert_eq!(plan_priority("high"), PlanEntryPriority::High);
        assert_eq!(plan_priority("p0"), PlanEntryPriority::High);
        assert_eq!(plan_priority("low"), PlanEntryPriority::Low);
        assert_eq!(plan_priority("normal"), PlanEntryPriority::Medium);
        assert_eq!(plan_priority("weird"), PlanEntryPriority::Medium);
    }

    #[test]
    fn plan_keeps_board_order_and_content() {
        let plan = plan_from_todos(&[
            item("write tests", "in_progress", "high"),
            item("ship", "open", "normal"),
        ]);
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].content, "write tests");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::InProgress);
        assert_eq!(plan.entries[0].priority, PlanEntryPriority::High);
        assert_eq!(plan.entries[1].content, "ship");
        assert_eq!(plan.entries[1].status, PlanEntryStatus::Pending);
    }

    #[test]
    fn empty_board_is_an_empty_plan() {
        assert!(plan_from_todos(&[]).entries.is_empty());
    }
}
