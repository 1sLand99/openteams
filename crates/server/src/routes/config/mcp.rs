fn member_mcp_catalog() -> McpConfig {
    McpConfig::new(
        vec!["mcpServers".to_string()],
        json!({ "mcpServers": {} }),
        PRECONFIGURED_MCP_SERVERS.clone(),
        false,
    )
}

async fn get_member_mcp_catalog() -> ResponseJson<ApiResponse<McpConfig>> {
    ResponseJson(ApiResponse::success(member_mcp_catalog()))
}

#[cfg(test)]
mod member_mcp_catalog_tests {
    use super::*;

    #[test]
    fn catalog_uses_canonical_member_shape_and_builtin_definitions() {
        let catalog = serde_json::to_value(member_mcp_catalog()).expect("serialize MCP catalog");

        assert_eq!(catalog["servers_path"], json!(["mcpServers"]));
        assert_eq!(catalog["template"], json!({ "mcpServers": {} }));
        assert_eq!(catalog["servers"], json!({}));
        assert_eq!(
            catalog["preconfigured"]["playwright"],
            json!({
                "command": "npx",
                "args": ["@playwright/mcp@latest"]
            })
        );
        assert_eq!(
            catalog["preconfigured"]["meta"]["playwright"]["name"],
            "Playwright"
        );
    }
}
