//! The `tools/list` result's size, serialized exactly as the server sends it.

use rmcp::model::{ListToolsResult, Tool};

use super::{front, GMeshMcpServer, TOOLS_LIST_BYTE_CEILING};

fn serialized_len(tools: Vec<Tool>) -> usize {
    serde_json::to_string(&ListToolsResult::with_all_items(tools)).expect("a tool list serializes").len()
}

/// The front's list is the largest one served; it also checks that the
/// headroom rule in [`TOOLS_LIST_BYTE_CEILING`]'s doc still holds, so the
/// ceiling cannot drift loose as tools shrink.
#[test]
fn the_tools_list_fits_its_ceiling() {
    let tools = front::listed_tools();
    let smallest_tool = tools
        .iter()
        .map(|tool| serde_json::to_string(tool).expect("a tool serializes").len())
        .min()
        .expect("a front lists tools");
    let listed = serialized_len(tools);

    assert!(
        listed <= TOOLS_LIST_BYTE_CEILING,
        "tools/list is {listed} bytes, over its {TOOLS_LIST_BYTE_CEILING}-byte ceiling"
    );
    assert!(
        TOOLS_LIST_BYTE_CEILING - listed < smallest_tool,
        "tools/list is {listed} bytes, {} under its {TOOLS_LIST_BYTE_CEILING}-byte ceiling: the headroom \
         exceeds the smallest tool's {smallest_tool} bytes, so lower the ceiling",
        TOOLS_LIST_BYTE_CEILING - listed
    );
}

/// The front's tool with the `project` property it adds removed.
fn without_project(mut tool: Tool) -> Tool {
    let mut schema = (*tool.input_schema).clone();
    if let Some(properties) = schema.get_mut("properties").and_then(|value| value.as_object_mut()) {
        properties.remove(front::PROJECT_ARG);
    }
    tool.input_schema = std::sync::Arc::new(schema);
    tool
}

/// A project daemon lists a subset of the front's tools, modulo the
/// `project` property the front adds to each, so the front's ceiling bounds
/// it too.
#[test]
fn a_project_daemon_lists_a_subset_of_the_front() {
    let daemon = GMeshMcpServer::tool_router().list_all();
    let front: Vec<Tool> = front::listed_tools().into_iter().map(without_project).collect();
    for tool in &daemon {
        assert!(front.contains(tool), "{} is listed by a project daemon but not by a front", tool.name);
    }
    assert!(serialized_len(daemon) < serialized_len(front::listed_tools()));
}
