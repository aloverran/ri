pub mod bash;
pub mod read;
pub mod write;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;

use std::sync::Arc;
use ri_core::tool::Tool;

/// Create the default set of coding tools rooted at `cwd`.
pub fn coding_tools(cwd: &str) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(bash::BashTool::new(cwd)),
        Arc::new(read::ReadTool::new(cwd)),
        Arc::new(write::WriteTool::new(cwd)),
        Arc::new(edit::EditTool::new(cwd)),
    ]
}

/// Create the full set of tools including find, grep, ls.
pub fn all_tools(cwd: &str) -> Vec<Arc<dyn Tool>> {
    let mut tools = coding_tools(cwd);
    tools.push(Arc::new(find::FindTool::new(cwd)));
    tools.push(Arc::new(grep::GrepTool::new(cwd)));
    tools.push(Arc::new(ls::LsTool::new(cwd)));
    tools
}

/// Read-only tools (for safe/restricted modes).
pub fn read_only_tools(cwd: &str) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(read::ReadTool::new(cwd)),
        Arc::new(find::FindTool::new(cwd)),
        Arc::new(grep::GrepTool::new(cwd)),
        Arc::new(ls::LsTool::new(cwd)),
    ]
}
