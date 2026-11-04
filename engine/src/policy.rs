use serde_json::Value;

/// Policy extracted from the app-server `config/read` answer.
#[derive(Clone, Debug)]
pub struct Policy {
    pub allow_browser_and_computer_use: bool,
    /// "allow" | "ask" | ... (default_app_access)
    pub default_app_access: String,
    pub allow_persistent_approval: bool,
    pub apps: Vec<AppRule>,
}

#[derive(Clone, Debug)]
pub struct AppRule {
    /// aumids / exes match list (whatever the policy provided)
    #[allow(dead_code)] // kept for policy-shape compatibility (see requires_approval)
    pub matches: Vec<String>,
    #[allow(dead_code)] // ditto
    pub access: String,
}

impl Default for Policy {
    fn default() -> Self {
        // Same effective policy the dsh app-server stub answers with.
        Policy {
            allow_browser_and_computer_use: true,
            default_app_access: "allow".into(),
            allow_persistent_approval: true,
            apps: Vec::new(),
        }
    }
}

impl Policy {
    /// Whether the first touch of an app must produce an approvalRequest.
    /// `candidates` are all identity forms of the target (app id, process
    /// path, exe name, AUMID prefix, display name).
    #[allow(dead_code)] // stub policy answers default_app_access=allow, so today this stays false
    pub fn requires_approval(&self, candidates: &[String]) -> bool {
        for rule in &self.apps {
            let hit = rule.matches.iter().any(|m| {
                let m = m.to_lowercase();
                candidates.iter().any(|c| c.to_lowercase() == m)
            });
            if hit {
                return rule.access.to_lowercase() != "allow";
            }
        }
        self.default_app_access.to_lowercase() != "allow"
    }
}

pub fn policy_from_config_read(config: &Value) -> Result<Policy, String> {
    let cu = config
        .get("computer_use")
        .or_else(|| config.get("computerUse"))
        .ok_or_else(|| "config/read did not return config".to_string())?;
    let mut p = Policy::default();
    if let Some(v) = cu
        .get("allow_browser_and_computer_use")
        .or_else(|| cu.get("allowBrowserAndComputerUse"))
        .and_then(Value::as_bool)
    {
        p.allow_browser_and_computer_use = v;
    }
    if let Some(v) = cu
        .get("default_app_access")
        .or_else(|| cu.get("defaultAppAccess"))
        .and_then(Value::as_str)
    {
        p.default_app_access = v.to_string();
    }
    if let Some(v) = cu
        .get("allow_persistent_approval")
        .or_else(|| cu.get("allowPersistentApproval"))
        .and_then(Value::as_bool)
    {
        p.allow_persistent_approval = v;
    }
    if let Some(apps) = cu.get("apps").and_then(Value::as_array) {
        for a in apps {
            let mut matches = Vec::new();
            for key in ["aumids", "exes"] {
                if let Some(list) = a.get(key).and_then(Value::as_array) {
                    for item in list {
                        if let Some(s) = item.as_str() {
                            matches.push(s.to_string());
                        }
                    }
                }
            }
            let access = a
                .get("access")
                .and_then(Value::as_str)
                .unwrap_or("allow")
                .to_string();
            p.apps.push(AppRule { matches, access });
        }
    }
    Ok(p)
}
