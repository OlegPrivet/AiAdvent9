use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextReport {
    pub(crate) limit: u32,
    pub(crate) before: usize,
    pub(crate) after: usize,
    pub(crate) output_reserve: u32,
    pub(crate) removed_messages: usize,
}
