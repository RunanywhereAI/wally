//! Points Claude Desktop at a wally endpoint and puts it back (port of
//! src/desktop/claude_profile.cpp).

pub fn apply_gateway(
    base_url: &str,
    api_key: &str,
    models: &[(String, String)],
    display_name: &str,
) -> Result<(), String> {
    let _ = (api_key, models);
    todo!("harness port: ApplyGateway ({base_url}, {display_name})")
}

pub fn restore_gateway() -> Result<(), String> {
    todo!("harness port: RestoreGateway")
}

pub fn gateway_applied() -> bool {
    todo!("harness port: GatewayApplied")
}

pub fn profile_directory() -> String {
    todo!("harness port: ProfileDirectory")
}
