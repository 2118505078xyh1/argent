//! MCP prompts 与 resources 的模型侧入口。
//!
//! MCP 的 tools 会各自注册成工具；prompts（模板）和 resources（文档）
//! 没有对应的"调用"形态，所以这里各做一个管理工具：list 看有什么、
//! get/read 取回内容。两者都只读，不需要审批。

use std::sync::{Arc, Mutex};

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{Tool, ToolCtx, ToolResult};

use crate::client::McpClient;

/// 服务器名与共享 client 的对应。
pub type ClientSlot = Arc<Mutex<McpClient>>;

fn server_of(slot: &ClientSlot) -> String {
    slot.lock()
        .map(|client| client.name().to_string())
        .unwrap_or_else(|_| "(poisoned)".to_string())
}

/// 找到名字匹配的 client；空名字表示唯一/全部。
fn find<'a>(clients: &'a [ClientSlot], wanted: &str) -> Vec<&'a ClientSlot> {
    if wanted.is_empty() {
        return clients.iter().collect();
    }
    clients
        .iter()
        .filter(|slot| server_of(slot) == wanted)
        .collect()
}

/// `mcp_prompt`：列出或渲染 MCP prompt 模板。
pub struct McpPromptTool {
    clients: Vec<ClientSlot>,
}

impl McpPromptTool {
    pub fn new(clients: Vec<ClientSlot>) -> McpPromptTool {
        McpPromptTool { clients }
    }
}

impl Tool for McpPromptTool {
    fn name(&self) -> &str {
        "mcp_prompt"
    }

    fn description(&self) -> &str {
        "List or render MCP prompt templates. action=list shows every prompt \
         on every server; action=get renders one prompt into text."
    }

    fn is_mutating(&self) -> bool {
        false
    }

    fn needs_approval(&self, _args: &Json) -> bool {
        false
    }

    fn parameters(&self) -> Json {
        Json::parse(
            r#"{
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "get"]},
                    "server": {"type": "string", "description": "MCP server name; omit for all"},
                    "name": {"type": "string", "description": "prompt name for get"},
                    "arguments": {"type": "object", "description": "prompt arguments for get"}
                },
                "required": ["action"]
            }"#,
        )
        .unwrap_or_else(|_| Json::obj())
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = args.str_or("action", "list");
        let server = args.str_or("server", "");
        let slots = find(&self.clients, &server);
        if slots.is_empty() {
            return Ok(ToolResult::error(format!(
                "no MCP server matches '{}'",
                server
            )));
        }
        match action.as_str() {
            "list" => {
                let mut out = String::new();
                for slot in slots {
                    let name = server_of(slot);
                    let prompts = match slot.lock() {
                        Ok(client) => client.prompts.clone(),
                        Err(_) => continue,
                    };
                    if prompts.is_empty() {
                        continue;
                    }
                    for prompt in prompts {
                        let prompt_name = prompt.str_or("name", "");
                        let description = prompt.str_or("description", "");
                        out.push_str(&format!(
                            "{}/{}  {}\n",
                            name, prompt_name, description
                        ));
                    }
                }
                if out.is_empty() {
                    Ok(ToolResult::ok("no MCP prompts on the connected servers"))
                } else {
                    Ok(ToolResult::ok(out))
                }
            }
            "get" => {
                let name = args.str_or("name", "");
                if name.is_empty() {
                    return Ok(ToolResult::error("mcp_prompt get needs name"));
                }
                let arguments = args.get("arguments").cloned().unwrap_or_else(Json::obj);
                for slot in slots {
                    let matched = match slot.lock() {
                        Ok(client) => client
                            .prompts
                            .iter()
                            .any(|prompt| prompt.str_or("name", "") == name),
                        Err(_) => false,
                    };
                    if !matched {
                        continue;
                    }
                    let rendered = match slot.lock() {
                        Ok(mut client) => client.get_prompt(&name, arguments),
                        Err(_) => Err(argent_core::error::Error::internal(
                            "MCP client is poisoned",
                        )),
                    };
                    return match rendered {
                        Ok(text) => Ok(ToolResult::ok(text)),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    };
                }
                Ok(ToolResult::error(format!(
                    "no MCP prompt named '{}'",
                    name
                )))
            }
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use list or get",
                other
            ))),
        }
    }
}

/// `mcp_resource`：列出或读取 MCP resources。
pub struct McpResourceTool {
    clients: Vec<ClientSlot>,
}

impl McpResourceTool {
    pub fn new(clients: Vec<ClientSlot>) -> McpResourceTool {
        McpResourceTool { clients }
    }
}

impl Tool for McpResourceTool {
    fn name(&self) -> &str {
        "mcp_resource"
    }

    fn description(&self) -> &str {
        "List or read MCP resources (documents a server exposes). \
         action=list shows uri and name; action=read returns the text."
    }

    fn is_mutating(&self) -> bool {
        false
    }

    fn needs_approval(&self, _args: &Json) -> bool {
        false
    }

    fn parameters(&self) -> Json {
        Json::parse(
            r#"{
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "read"]},
                    "server": {"type": "string", "description": "MCP server name; omit for all"},
                    "uri": {"type": "string", "description": "resource uri for read"}
                },
                "required": ["action"]
            }"#,
        )
        .unwrap_or_else(|_| Json::obj())
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = args.str_or("action", "list");
        let server = args.str_or("server", "");
        let slots = find(&self.clients, &server);
        if slots.is_empty() {
            return Ok(ToolResult::error(format!(
                "no MCP server matches '{}'",
                server
            )));
        }
        match action.as_str() {
            "list" => {
                let mut out = String::new();
                for slot in slots {
                    let name = server_of(slot);
                    let resources = match slot.lock() {
                        Ok(client) => client.resources.clone(),
                        Err(_) => continue,
                    };
                    for resource in resources {
                        out.push_str(&format!(
                            "{}/{}  {}\n",
                            name,
                            resource.str_or("uri", ""),
                            resource.str_or("name", "")
                        ));
                    }
                }
                if out.is_empty() {
                    Ok(ToolResult::ok("no MCP resources on the connected servers"))
                } else {
                    Ok(ToolResult::ok(out))
                }
            }
            "read" => {
                let uri = args.str_or("uri", "");
                if uri.is_empty() {
                    return Ok(ToolResult::error("mcp_resource read needs uri"));
                }
                for slot in slots {
                    let matched = match slot.lock() {
                        Ok(client) => client
                            .resources
                            .iter()
                            .any(|resource| resource.str_or("uri", "") == uri),
                        Err(_) => false,
                    };
                    if !matched {
                        continue;
                    }
                    let text = match slot.lock() {
                        Ok(mut client) => client.read_resource(&uri),
                        Err(_) => Err(argent_core::error::Error::internal(
                            "MCP client is poisoned",
                        )),
                    };
                    return match text {
                        Ok(text) => Ok(ToolResult::ok(text)),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    };
                }
                Ok(ToolResult::error(format!(
                    "no MCP resource with uri '{}'",
                    uri
                )))
            }
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use list or read",
                other
            ))),
        }
    }
}
