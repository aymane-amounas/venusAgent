use crate::capability::{
    CapabilityDescriptor, CapabilityEffect, CapabilityRequest, CapabilityState,
    validate_identifier, validate_version,
};
use anyhow::{Context, Result, bail};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    #[default]
    Open,
    Controlled,
    Restricted,
}

impl PermissionProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Controlled => "controlled",
            Self::Restricted => "restricted",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "open" => Ok(Self::Open),
            "controlled" => Ok(Self::Controlled),
            "restricted" => Ok(Self::Restricted),
            _ => bail!("Unknown permission profile '{value}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrantDraft {
    pub capability_id: String,
    pub capability_version: String,
    pub operation: String,
    pub permissions: Vec<String>,
    pub requires_approval: bool,
    pub expires_at: Option<i64>,
}

impl PermissionGrantDraft {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.capability_id, "capability identifier")?;
        validate_version(&self.capability_version)?;
        validate_identifier(&self.operation, "operation identifier")?;
        let mut permissions = BTreeSet::new();
        for permission in &self.permissions {
            validate_identifier(permission, "permission identifier")?;
            if !permissions.insert(permission) {
                bail!("Permission grants must not contain duplicate permissions");
            }
        }
        if self.expires_at.is_some_and(|expires_at| expires_at <= 0) {
            bail!("Grant expiration must be a positive Unix timestamp");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub id: i64,
    pub capability_id: String,
    pub capability_version: String,
    pub operation: String,
    pub permissions: Vec<String>,
    pub requires_approval: bool,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

impl PermissionGrant {
    pub fn from_draft(id: i64, draft: PermissionGrantDraft, created_at: i64) -> Self {
        Self {
            id,
            capability_id: draft.capability_id,
            capability_version: draft.capability_version,
            operation: draft.operation,
            permissions: draft.permissions,
            requires_approval: draft.requires_approval,
            expires_at: draft.expires_at,
            created_at,
            revoked_at: None,
        }
    }

    pub fn is_active_at(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    Revoked,
    Expired,
    Consumed,
}

impl ApprovalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::Consumed => "consumed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            "revoked" => Ok(Self::Revoked),
            "expired" => Ok(Self::Expired),
            "consumed" => Ok(Self::Consumed),
            _ => bail!("Unknown approval state '{value}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: i64,
    pub request_id: i64,
    pub capability_id: String,
    pub capability_version: String,
    pub operation: String,
    pub request_fingerprint: String,
    pub argument_summary: String,
    pub required_permissions: Vec<String>,
    pub consequences: Vec<String>,
    pub grant_id: i64,
    pub permission_profile: PermissionProfile,
    pub scope: String,
    pub expires_at: i64,
    pub state: ApprovalState,
    pub created_at: i64,
}

impl ApprovalRecord {
    pub fn matches_request(
        &self,
        descriptor: &CapabilityDescriptor,
        request: &CapabilityRequest,
    ) -> Result<bool> {
        Ok(self.capability_id == descriptor.id
            && self.capability_version == descriptor.version
            && self.operation == request.operation
            && self.scope == "single_request"
            && self.request_fingerprint == request_fingerprint(descriptor, request)?)
    }

    pub fn is_active_at(&self, now: i64) -> bool {
        self.state == ApprovalState::Approved && self.expires_at > now
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationDecision {
    Allowed { reason: String },
    Denied { reason: String },
    RequiresApproval { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityAuditStatus {
    Proposed,
    Denied,
    Approved,
    Started,
    Completed,
    Failed,
    Cancelled,
}

impl CapabilityAuditStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Denied => "denied",
            Self::Approved => "approved",
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "proposed" => Ok(Self::Proposed),
            "denied" => Ok(Self::Denied),
            "approved" => Ok(Self::Approved),
            "started" => Ok(Self::Started),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => bail!("Unknown capability audit status '{value}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAuditRecord {
    pub id: i64,
    pub request_id: i64,
    pub capability_id: String,
    pub capability_version: String,
    pub operation: String,
    pub argument_summary: String,
    pub permission_profile: PermissionProfile,
    pub authorization_decision: String,
    pub decision_reason: String,
    pub approval_id: Option<i64>,
    pub status: CapabilityAuditStatus,
    pub error_classification: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationResult {
    pub decision: AuthorizationDecision,
    pub approval_id: Option<i64>,
    pub request_id: i64,
    pub audit_id: i64,
}

pub fn evaluate_authorization(
    profile: PermissionProfile,
    descriptor: &CapabilityDescriptor,
    request: &CapabilityRequest,
    grants: &[PermissionGrant],
    approval: Option<&ApprovalRecord>,
    now: i64,
) -> Result<AuthorizationDecision> {
    descriptor.validate()?;
    validate_identifier(&request.operation, "operation identifier")?;
    if request.capability_id != descriptor.id || request.capability_version != descriptor.version {
        return Ok(AuthorizationDecision::Denied {
            reason: "Request identity does not match the registered capability version".to_owned(),
        });
    }
    if !descriptor.operations.contains(&request.operation) {
        return Ok(AuthorizationDecision::Denied {
            reason: format!(
                "Capability '{}' does not declare operation '{}'",
                descriptor.id, request.operation
            ),
        });
    }
    if descriptor.state != CapabilityState::Available {
        return Ok(AuthorizationDecision::Denied {
            reason: format!("Capability '{}' is not available", descriptor.id),
        });
    }
    descriptor.validate_input(&request.input)?;

    let requires_write_consent = descriptor.effect == CapabilityEffect::Mutating;
    if profile == PermissionProfile::Open && !requires_write_consent {
        return Ok(AuthorizationDecision::Allowed {
            reason: "Open profile adds no Venus permission or approval gate; operating-system permissions remain authoritative"
                .to_owned(),
        });
    }

    let required: BTreeSet<_> = descriptor
        .requirements
        .iter()
        .map(|requirement| requirement.permission.as_str())
        .collect();
    let grant = grants.iter().find(|grant| {
        grant.is_active_at(now)
            && grant.capability_id == descriptor.id
            && grant.capability_version == descriptor.version
            && grant.operation == request.operation
            && required.iter().all(|permission| {
                grant
                    .permissions
                    .iter()
                    .any(|granted| granted == permission)
            })
    });
    let Some(grant) = grant else {
        return Ok(AuthorizationDecision::Denied {
            reason: "No active grant covers this capability version, operation, and required permissions"
                .to_owned(),
        });
    };

    let approval_required = requires_write_consent
        || profile == PermissionProfile::Restricted
        || grant.requires_approval;
    if !approval_required {
        return Ok(AuthorizationDecision::Allowed {
            reason: "An active user grant covers this operation and its required permissions"
                .to_owned(),
        });
    }

    let Some(approval) = approval else {
        return Ok(AuthorizationDecision::RequiresApproval {
            reason: "A request-specific user approval is required".to_owned(),
        });
    };
    if !approval.matches_request(descriptor, request)? {
        return Ok(AuthorizationDecision::Denied {
            reason: "Approval does not match this capability request".to_owned(),
        });
    }
    if approval.permission_profile != profile {
        return Ok(AuthorizationDecision::Denied {
            reason: "Approval is stale because the permission profile has changed".to_owned(),
        });
    }
    if !approval.is_active_at(now) {
        return Ok(AuthorizationDecision::Denied {
            reason: if approval.expires_at <= now {
                "Approval is expired".to_owned()
            } else {
                format!(
                    "Approval is not usable in state '{}'",
                    approval.state.as_str()
                )
            },
        });
    }
    if approval.grant_id != grant.id {
        return Ok(AuthorizationDecision::Denied {
            reason: "Approval is bound to a different permission grant".to_owned(),
        });
    }
    Ok(AuthorizationDecision::Allowed {
        reason:
            "The active grant and matching request-specific user approval authorize this request"
                .to_owned(),
    })
}

pub fn request_fingerprint(
    descriptor: &CapabilityDescriptor,
    request: &CapabilityRequest,
) -> Result<String> {
    let serialized = serde_json::to_vec(&(descriptor, request.operation.as_str(), &request.input))
        .context("Could not encode capability request fingerprint")?;
    let hash = digest(&SHA256, &serialized);
    Ok(hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn argument_summary(input: &Value) -> String {
    match input {
        Value::Object(object) => {
            let mut fields: Vec<_> = object
                .keys()
                .take(12)
                .map(|name| {
                    name.chars()
                        .take(64)
                        .map(|character| {
                            if character.is_ascii_alphanumeric()
                                || matches!(character, '_' | '.' | '-')
                            {
                                character
                            } else {
                                '?'
                            }
                        })
                        .collect::<String>()
                })
                .collect();
            if let Some(workspace) = object.get("workspace").and_then(Value::as_str)
                && let Some(name) = Path::new(workspace)
                    .file_name()
                    .and_then(|name| name.to_str())
            {
                let safe_name: String = name
                    .chars()
                    .take(64)
                    .map(|character| {
                        if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
                        {
                            character
                        } else {
                            '?'
                        }
                    })
                    .collect();
                fields.push(format!("workspace={safe_name}"));
            }
            let suffix = if object.len() > 12 { ", …" } else { "" };
            format!(
                "Object with {} field(s): {}; other values omitted{}",
                object.len(),
                fields.join(", "),
                suffix
            )
        }
        Value::Array(values) => format!("Array with {} item(s); values omitted", values.len()),
        Value::String(_) => "String argument; value omitted".to_owned(),
        Value::Number(_) => "Numeric argument; value omitted".to_owned(),
        Value::Bool(_) => "Boolean argument; value omitted".to_owned(),
        Value::Null => "Null argument".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityDependency, ExecutionRequirement};
    use serde_json::json;

    fn make_descriptor(state: CapabilityState) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: "test.read".to_owned(),
            name: "Read example".to_owned(),
            description: "Test-only read capability".to_owned(),
            version: "1.0.0".to_owned(),
            input_schema: json!({"type": "object", "properties": {"prompt": {"type": "string"}}}),
            output_schema: json!({"type": "string"}),
            configuration_schema: None,
            effect: crate::capability::CapabilityEffect::ReadOnly,
            effects: vec!["Reads a bounded resource.".to_owned()],
            operations: vec!["read".to_owned()],
            state,
            dependencies: vec![CapabilityDependency {
                id: "core.base".to_owned(),
                version: "1.0.0".to_owned(),
            }],
            requirements: vec![ExecutionRequirement {
                permission: "data.read".to_owned(),
                description: "Read a bounded resource".to_owned(),
                sensitive: true,
            }],
        }
    }

    fn make_request(prompt: &str) -> CapabilityRequest {
        CapabilityRequest {
            capability_id: "test.read".to_owned(),
            capability_version: "1.0.0".to_owned(),
            operation: "read".to_owned(),
            input: json!({"prompt": prompt}),
        }
    }

    fn grant(approval: bool, expires_at: Option<i64>) -> PermissionGrant {
        PermissionGrant::from_draft(
            7,
            PermissionGrantDraft {
                capability_id: "test.read".to_owned(),
                capability_version: "1.0.0".to_owned(),
                operation: "read".to_owned(),
                permissions: vec!["data.read".to_owned()],
                requires_approval: approval,
                expires_at,
            },
            1,
        )
    }

    #[test]
    fn open_bypasses_venus_grants_but_not_availability_or_schema() -> Result<()> {
        let descriptor = make_descriptor(CapabilityState::Available);
        let result = evaluate_authorization(
            PermissionProfile::Open,
            &descriptor,
            &make_request("ok"),
            &[],
            None,
            10,
        )?;
        assert!(matches!(result, AuthorizationDecision::Allowed { .. }));
        let mut invalid_request = make_request("ok");
        invalid_request.operation = "invalid operation".to_owned();
        assert!(
            evaluate_authorization(
                PermissionProfile::Open,
                &descriptor,
                &invalid_request,
                &[],
                None,
                10
            )
            .is_err()
        );
        let disabled = make_descriptor(CapabilityState::Disabled);
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Open,
                &disabled,
                &make_request("ok"),
                &[],
                None,
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        let mut unlisted_operation = make_request("ok");
        unlisted_operation.operation = "write".to_owned();
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Open,
                &descriptor,
                &unlisted_operation,
                &[],
                None,
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        Ok(())
    }

    #[test]
    fn open_profile_requires_a_grant_and_approval_for_mutating_capabilities() -> Result<()> {
        let mut descriptor = make_descriptor(CapabilityState::Available);
        descriptor.effect = CapabilityEffect::Mutating;
        descriptor.requirements[0].permission = "data.write".to_owned();
        let request = make_request("replace");
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Open,
                &descriptor,
                &request,
                &[],
                None,
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        let write_grant = PermissionGrant::from_draft(
            12,
            PermissionGrantDraft {
                capability_id: descriptor.id.clone(),
                capability_version: descriptor.version.clone(),
                operation: "read".to_owned(),
                permissions: vec!["data.write".to_owned()],
                requires_approval: false,
                expires_at: None,
            },
            1,
        );
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Open,
                &descriptor,
                &request,
                &[write_grant],
                None,
                10
            )?,
            AuthorizationDecision::RequiresApproval { .. }
        ));
        Ok(())
    }

    #[test]
    fn controlled_requires_grant_and_configured_approval() -> Result<()> {
        let descriptor = make_descriptor(CapabilityState::Available);
        let request = make_request("ok");
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Controlled,
                &descriptor,
                &request,
                &[],
                None,
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Controlled,
                &descriptor,
                &request,
                &[grant(true, None)],
                None,
                10
            )?,
            AuthorizationDecision::RequiresApproval { .. }
        ));
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Controlled,
                &descriptor,
                &request,
                &[grant(false, None)],
                None,
                10
            )?,
            AuthorizationDecision::Allowed { .. }
        ));
        let incomplete = PermissionGrant::from_draft(
            9,
            PermissionGrantDraft {
                capability_id: "test.read".to_owned(),
                capability_version: "1.0.0".to_owned(),
                operation: "read".to_owned(),
                permissions: Vec::new(),
                requires_approval: false,
                expires_at: None,
            },
            1,
        );
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Controlled,
                &descriptor,
                &request,
                &[incomplete],
                None,
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        Ok(())
    }

    #[test]
    fn restricted_requires_grant_and_exact_unexpired_approval() -> Result<()> {
        let descriptor = make_descriptor(CapabilityState::Available);
        let request = make_request("safe");
        let grant = grant(false, Some(100));
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Restricted,
                &descriptor,
                &request,
                std::slice::from_ref(&grant),
                None,
                10
            )?,
            AuthorizationDecision::RequiresApproval { .. }
        ));
        let approval = ApprovalRecord {
            id: 8,
            request_id: 1,
            capability_id: descriptor.id.clone(),
            capability_version: descriptor.version.clone(),
            operation: "read".to_owned(),
            request_fingerprint: request_fingerprint(&descriptor, &request)?,
            argument_summary: argument_summary(&request.input),
            required_permissions: vec!["data.read".to_owned()],
            consequences: vec!["Read a bounded resource".to_owned()],
            grant_id: grant.id,
            permission_profile: PermissionProfile::Restricted,
            scope: "single_request".to_owned(),
            expires_at: 20,
            state: ApprovalState::Approved,
            created_at: 1,
        };
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Restricted,
                &descriptor,
                &request,
                std::slice::from_ref(&grant),
                Some(&approval),
                10
            )?,
            AuthorizationDecision::Allowed { .. }
        ));
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Restricted,
                &descriptor,
                &request,
                std::slice::from_ref(&grant),
                Some(&approval),
                20
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        let different_request = make_request("different");
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Restricted,
                &descriptor,
                &different_request,
                std::slice::from_ref(&grant),
                Some(&approval),
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        let mut changed_contract = descriptor.clone();
        changed_contract.requirements[0].description = "Changed consequences".to_owned();
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Restricted,
                &changed_contract,
                &request,
                std::slice::from_ref(&grant),
                Some(&approval),
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        let mut wrong_profile_approval = approval.clone();
        wrong_profile_approval.permission_profile = PermissionProfile::Controlled;
        assert!(matches!(
            evaluate_authorization(
                PermissionProfile::Restricted,
                &descriptor,
                &request,
                &[grant],
                Some(&wrong_profile_approval),
                10
            )?,
            AuthorizationDecision::Denied { .. }
        ));
        Ok(())
    }

    #[test]
    fn safe_argument_summary_never_contains_values() {
        let summary = argument_summary(&json!({"token": "secret", "path": "/private"}));
        assert!(summary.contains("2 field(s)"));
        assert!(summary.contains("path, token"));
        assert!(!summary.contains("secret"));
        assert!(!summary.contains("/private"));
    }

    #[test]
    fn workspace_summary_shows_only_a_sanitized_basename() {
        let summary = argument_summary(&json!({"workspace": "/private/repo\n"}));
        assert!(summary.contains("workspace=repo?"));
        assert!(!summary.contains("/private"));
    }
}
