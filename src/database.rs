use crate::{
    capability::{CapabilityDescriptor, CapabilityEffect, CapabilityRequest},
    permissions::{
        ApprovalRecord, ApprovalState, AuthorizationDecision, AuthorizationResult,
        CapabilityAuditRecord, CapabilityAuditStatus, PermissionGrant, PermissionGrantDraft,
        PermissionProfile, argument_summary, evaluate_authorization, request_fingerprint,
    },
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::Path;

const APPROVAL_TTL_SECONDS: i64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub struct Task {
    pub id: i64,
    pub description: String,
    pub status: String,
    pub created_at: String,
}

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .context("Database path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create database directory {}", parent.display()))?;

        let connection = Connection::open(path)
            .with_context(|| format!("Could not open database {}", path.display()))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("Could not configure database timeout")?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .context("Could not configure SQLite")?;
        migrate(&connection)?;
        Ok(Self { connection })
    }

    pub fn register_workspace(&self, root: &Path) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO workspaces (root) VALUES (?1)
                 ON CONFLICT(root) DO NOTHING",
                [path_string(root)?],
            )
            .context("Could not register workspace")?;
        Ok(())
    }

    pub fn add_task(&self, root: &Path, description: &str) -> Result<i64> {
        let workspace_id = self.workspace_id(root)?;
        let description = description.trim();
        if description.is_empty() {
            bail!("Task description must not be empty.");
        }
        self.connection
            .execute(
                "INSERT INTO tasks (workspace_id, description) VALUES (?1, ?2)",
                params![workspace_id, description],
            )
            .context("Could not add task")?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn list_tasks(&self, root: &Path) -> Result<Vec<Task>> {
        let workspace_id = self.workspace_id(root)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, description, status, created_at
                 FROM tasks
                 WHERE workspace_id = ?1
                 ORDER BY id",
            )
            .context("Could not prepare task query")?;
        let tasks = statement
            .query_map([workspace_id], |row| {
                Ok(Task {
                    id: row.get(0)?,
                    description: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .context("Could not list tasks")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Could not read task records")?;
        Ok(tasks)
    }

    pub fn complete_task(&self, root: &Path, id: i64) -> Result<bool> {
        let workspace_id = self.workspace_id(root)?;
        let updated = self
            .connection
            .execute(
                "UPDATE tasks SET status = 'completed', updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1 AND workspace_id = ?2 AND status = 'pending'",
                params![id, workspace_id],
            )
            .context("Could not complete task")?;
        Ok(updated == 1)
    }

    pub fn prune_completed_tasks(&self, retention_days: u32) -> Result<usize> {
        if retention_days == 0 {
            bail!("Task retention must be a positive number of days.");
        }
        let modifier = format!("-{retention_days} days");
        self.connection
            .execute(
                "DELETE FROM tasks
                 WHERE status = 'completed' AND updated_at < datetime('now', ?1)",
                [modifier],
            )
            .context("Could not prune completed task history")
    }

    pub fn record_model_interaction(
        &self,
        provider: &str,
        model: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO model_interactions (provider, model, input_tokens, output_tokens)
                 VALUES (?1, ?2, ?3, ?4)",
                params![provider, model, input_tokens, output_tokens],
            )
            .context("Could not persist model interaction metadata")?;
        Ok(())
    }

    pub fn prune_model_interactions(&self, retention_days: u32) -> Result<usize> {
        if retention_days == 0 {
            bail!("Model metadata retention must be a positive number of days.");
        }
        let modifier = format!("-{retention_days} days");
        self.connection
            .execute(
                "DELETE FROM model_interactions WHERE created_at < datetime('now', ?1)",
                [modifier],
            )
            .context("Could not prune model interaction metadata")
    }

    pub fn permission_profile(&self) -> Result<PermissionProfile> {
        let value: String = self
            .connection
            .query_row(
                "SELECT profile FROM permission_settings WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .context("Could not read permission profile")?;
        PermissionProfile::parse(&value)
    }

    pub fn set_permission_profile(&self, profile: PermissionProfile, now: i64) -> Result<()> {
        if now <= 0 {
            bail!("Permission profile update time must be a positive Unix timestamp");
        }
        let stale_approvals = self.approval_ids_for_previous_profiles(profile)?;
        self.connection
            .execute(
                "UPDATE permission_settings
                 SET profile = ?1, updated_at = ?2
                 WHERE id = 1",
                params![profile.as_str(), now],
            )
            .context("Could not update permission profile")?;
        for id in stale_approvals {
            let approval = self.get_approval(id)?;
            let changed = self.connection.execute(
                "UPDATE capability_approvals SET state = 'expired'
                 WHERE id = ?1 AND state IN ('pending', 'approved')",
                [id],
            )?;
            if changed == 1 {
                self.append_approval_audit(
                    &ApprovalRecord {
                        state: ApprovalState::Expired,
                        ..approval.clone()
                    },
                    approval.permission_profile,
                    now,
                )?;
            }
        }
        Ok(())
    }

    pub fn create_permission_grant(&self, draft: &PermissionGrantDraft, now: i64) -> Result<i64> {
        draft.validate()?;
        if now <= 0 {
            bail!("Grant creation time must be a positive Unix timestamp");
        }
        let permissions = serde_json::to_string(&draft.permissions)
            .context("Could not encode permission grant")?;
        self.connection
            .execute(
                "INSERT INTO permission_grants (
                    capability_id, capability_version, operation, permissions,
                    requires_approval, expires_at, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    draft.capability_id,
                    draft.capability_version,
                    draft.operation,
                    permissions,
                    draft.requires_approval,
                    draft.expires_at,
                    now
                ],
            )
            .context("Could not create permission grant")?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn list_permission_grants(&self) -> Result<Vec<PermissionGrant>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, capability_id, capability_version, operation, permissions,
                        requires_approval, expires_at, created_at, revoked_at
                 FROM permission_grants ORDER BY id",
            )
            .context("Could not prepare permission grant query")?;
        let rows = statement
            .query_map([], |row| {
                let permissions: String = row.get(4)?;
                let permissions = serde_json::from_str(&permissions).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(error.to_string())),
                    )
                })?;
                Ok(PermissionGrant {
                    id: row.get(0)?,
                    capability_id: row.get(1)?,
                    capability_version: row.get(2)?,
                    operation: row.get(3)?,
                    permissions,
                    requires_approval: row.get(5)?,
                    expires_at: row.get(6)?,
                    created_at: row.get(7)?,
                    revoked_at: row.get(8)?,
                })
            })
            .context("Could not list permission grants")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Could not read permission grant records")
    }

    pub fn revoke_permission_grant(&self, id: i64, now: i64) -> Result<bool> {
        if now <= 0 {
            bail!("Grant revocation time must be a positive Unix timestamp");
        }
        let changed = self
            .connection
            .execute(
                "UPDATE permission_grants SET revoked_at = ?1
                 WHERE id = ?2 AND revoked_at IS NULL",
                params![now, id],
            )
            .context("Could not revoke permission grant")?;
        if changed == 1 {
            let approvals = self.approvals_for_grant(id)?;
            self.connection
                .execute(
                    "UPDATE capability_approvals SET state = 'revoked'
                     WHERE grant_id = ?1 AND state IN ('pending', 'approved')",
                    [id],
                )
                .context("Could not invalidate approvals for revoked grant")?;
            for approval in approvals {
                self.append_approval_audit(
                    &ApprovalRecord {
                        state: ApprovalState::Revoked,
                        ..approval.clone()
                    },
                    approval.permission_profile,
                    now,
                )?;
            }
        }
        Ok(changed == 1)
    }

    pub fn authorize_capability(
        &self,
        descriptor: &CapabilityDescriptor,
        request: &CapabilityRequest,
        approval_id: Option<i64>,
        now: i64,
    ) -> Result<AuthorizationResult> {
        self.authorize_capability_inner(descriptor, request, approval_id, now, false)
    }

    pub fn authorize_capability_for_execution(
        &self,
        descriptor: &CapabilityDescriptor,
        request: &CapabilityRequest,
        approval_id: Option<i64>,
        now: i64,
    ) -> Result<AuthorizationResult> {
        self.authorize_capability_inner(descriptor, request, approval_id, now, true)
    }

    fn authorize_capability_inner(
        &self,
        descriptor: &CapabilityDescriptor,
        request: &CapabilityRequest,
        approval_id: Option<i64>,
        now: i64,
        start_execution: bool,
    ) -> Result<AuthorizationResult> {
        if now <= 0 {
            bail!("Authorization time must be a positive Unix timestamp");
        }
        let tx = self
            .connection
            .unchecked_transaction()
            .context("Could not begin capability authorization transaction")?;
        self.expire_approvals(now)?;
        let profile = self.permission_profile()?;
        let grants = self.list_permission_grants()?;
        let approval_required = descriptor.effect == CapabilityEffect::Mutating
            || (profile != PermissionProfile::Open
                && (profile == PermissionProfile::Restricted
                    || matching_grant(&grants, descriptor, request, now)
                        .is_some_and(|grant| grant.requires_approval)));
        let approval = if !approval_required {
            None
        } else {
            approval_id.map(|id| self.get_approval(id)).transpose()?
        };
        let evaluation = evaluate_authorization(
            profile,
            descriptor,
            request,
            &grants,
            approval.as_ref(),
            now,
        )
        .unwrap_or_else(|error| AuthorizationDecision::Denied {
            reason: format!("Capability request validation failed: {error}"),
        });
        let summary = argument_summary(&request.input);
        let fingerprint = request_fingerprint(descriptor, request)?;

        match evaluation {
            AuthorizationDecision::Allowed { reason } => {
                let mut effective_approval_id = None;
                let mut request_id = None;
                if let Some(approval) = approval.as_ref() {
                    let consumed = if profile == PermissionProfile::Open
                        && descriptor.effect != CapabilityEffect::Mutating
                    {
                        0
                    } else {
                        tx.execute(
                            "UPDATE capability_approvals SET state = 'consumed'
                             WHERE id = ?1 AND state = 'approved' AND expires_at > ?2
                               AND permission_profile = ?3
                               AND grant_id IN (
                                   SELECT id FROM permission_grants
                                   WHERE revoked_at IS NULL
                                     AND (expires_at IS NULL OR expires_at > ?2)
                               )",
                            params![approval.id, now, profile.as_str()],
                        )
                        .context("Could not consume request approval")?
                    };
                    if consumed != 1 {
                        let proposal = AuditMetadata {
                            profile,
                            decision: "denied",
                            reason: "Approval expired, was revoked, or its grant is no longer active",
                            classification: Some("approval_stale"),
                            approval_id: None,
                            summary: &summary,
                            now,
                        };
                        let (_, request_id) = insert_proposal(&tx, descriptor, request, &proposal)?;
                        let audit_id = insert_audit_event(
                            &tx,
                            request_id,
                            descriptor,
                            request,
                            &proposal,
                            CapabilityAuditStatus::Denied,
                        )?;
                        tx.commit()
                            .context("Could not persist authorization denial")?;
                        return Ok(AuthorizationResult {
                            decision: AuthorizationDecision::Denied {
                                reason: "Approval expired, was revoked, or its grant is no longer active"
                                    .to_owned(),
                            },
                            approval_id: None,
                            request_id,
                            audit_id,
                        });
                    }
                    effective_approval_id = Some(approval.id);
                    request_id = Some(approval.request_id);
                }
                let (mut audit_id, request_id) = match request_id {
                    Some(request_id) => {
                        let audit_id = latest_audit_id(&tx, request_id)?;
                        (audit_id, request_id)
                    }
                    None => {
                        let proposal = AuditMetadata {
                            profile,
                            decision: "allowed",
                            reason: &reason,
                            classification: None,
                            approval_id: None,
                            summary: &summary,
                            now,
                        };
                        insert_proposal(&tx, descriptor, request, &proposal)?
                    }
                };
                if start_execution {
                    let started = AuditMetadata {
                        profile,
                        decision: "allowed",
                        reason: &reason,
                        classification: None,
                        approval_id: effective_approval_id,
                        summary: &summary,
                        now,
                    };
                    audit_id = insert_audit_event(
                        &tx,
                        request_id,
                        descriptor,
                        request,
                        &started,
                        CapabilityAuditStatus::Started,
                    )?;
                }
                tx.commit()
                    .context("Could not persist authorization result")?;
                Ok(AuthorizationResult {
                    decision: AuthorizationDecision::Allowed {
                        reason: if start_execution {
                            format!("{reason}; execution authorized")
                        } else {
                            format!("{reason}; no operation was executed")
                        },
                    },
                    approval_id: effective_approval_id,
                    request_id,
                    audit_id,
                })
            }
            AuthorizationDecision::RequiresApproval { reason } => {
                let (request_id, approval) = if let Some(approval) = approval.as_ref() {
                    (approval.request_id, approval.id)
                } else {
                    let proposal = AuditMetadata {
                        profile,
                        decision: "approval_required",
                        reason: &reason,
                        classification: None,
                        approval_id: None,
                        summary: &summary,
                        now,
                    };
                    let (audit_id, request_id) =
                        insert_proposal(&tx, descriptor, request, &proposal)?;
                    let grant = matching_grant(&grants, descriptor, request, now)
                        .context("Missing grant for an approval-required request")?;
                    let expires_at = now
                        .checked_add(APPROVAL_TTL_SECONDS)
                        .context("Approval expiration overflowed")?;
                    let required_permissions = descriptor
                        .requirements
                        .iter()
                        .map(|requirement| requirement.permission.clone())
                        .collect::<Vec<_>>();
                    let mut consequences = descriptor
                        .effects
                        .iter()
                        .map(|effect| sanitize_audit_text(effect, 256))
                        .collect::<Vec<_>>();
                    consequences.extend(
                        descriptor
                            .requirements
                            .iter()
                            .map(|requirement| sanitize_audit_text(&requirement.description, 256)),
                    );
                    let required_permissions = serde_json::to_string(&required_permissions)?;
                    let consequences = serde_json::to_string(&consequences)?;
                    tx.execute(
                        "INSERT INTO capability_approvals (
                            request_id, capability_id, capability_version, operation,
                            request_fingerprint, argument_summary, required_permissions,
                            consequences, grant_id, permission_profile, scope, expires_at,
                            state, created_at
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                                   'single_request', ?11, 'pending', ?12)",
                        params![
                            request_id,
                            sanitize_audit_text(&descriptor.id, 128),
                            sanitize_audit_text(&descriptor.version, 32),
                            sanitize_audit_text(&request.operation, 128),
                            fingerprint,
                            summary,
                            required_permissions,
                            consequences,
                            grant.id,
                            profile.as_str(),
                            expires_at,
                            now
                        ],
                    )
                    .context("Could not create request approval")?;
                    let approval_id = tx.last_insert_rowid();
                    tx.execute(
                        "UPDATE capability_audit SET approval_id = ?1 WHERE id = ?2",
                        params![approval_id, audit_id],
                    )
                    .context("Could not link approval to audit record")?;
                    (request_id, approval_id)
                };
                let audit_id = latest_audit_id(&tx, request_id)?;
                tx.commit().context("Could not persist approval request")?;
                Ok(AuthorizationResult {
                    decision: AuthorizationDecision::RequiresApproval { reason },
                    approval_id: Some(approval),
                    request_id,
                    audit_id,
                })
            }
            AuthorizationDecision::Denied { reason } => {
                let classification = if reason.contains("Approval") || reason.contains("approval") {
                    "approval_invalid"
                } else if reason.contains("grant") {
                    "permission_denied"
                } else {
                    "invalid_request"
                };
                let denied = AuditMetadata {
                    profile,
                    decision: "denied",
                    reason: &reason,
                    classification: Some(classification),
                    approval_id,
                    summary: &summary,
                    now,
                };
                let (_, request_id) = insert_proposal(&tx, descriptor, request, &denied)?;
                let audit_id = insert_audit_event(
                    &tx,
                    request_id,
                    descriptor,
                    request,
                    &denied,
                    CapabilityAuditStatus::Denied,
                )?;
                tx.commit()
                    .context("Could not persist authorization denial")?;
                Ok(AuthorizationResult {
                    decision: AuthorizationDecision::Denied { reason },
                    approval_id: None,
                    request_id,
                    audit_id,
                })
            }
        }
    }

    pub fn resolve_approval(&self, id: i64, approve: bool, now: i64) -> Result<ApprovalRecord> {
        if now <= 0 {
            bail!("Approval resolution time must be a positive Unix timestamp");
        }
        self.expire_approvals(now)?;
        let approval = self.get_approval(id)?;
        if approval.state != ApprovalState::Pending {
            bail!("Approval #{id} is not pending");
        }
        let profile = self.permission_profile()?;
        if profile != approval.permission_profile {
            self.set_approval_state(id, ApprovalState::Expired)?;
            self.append_approval_audit(
                &ApprovalRecord {
                    state: ApprovalState::Expired,
                    ..approval.clone()
                },
                approval.permission_profile,
                now,
            )?;
            bail!("Approval #{id} is stale because the permission profile changed");
        }
        let active_grant: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM permission_grants
                 WHERE id = ?1 AND revoked_at IS NULL
                   AND (expires_at IS NULL OR expires_at > ?2))",
                params![approval.grant_id, now],
                |row| row.get(0),
            )
            .context("Could not check approval grant")?;
        if !active_grant {
            self.set_approval_state(id, ApprovalState::Revoked)?;
            self.append_approval_audit(
                &ApprovalRecord {
                    state: ApprovalState::Revoked,
                    ..approval.clone()
                },
                profile,
                now,
            )?;
            bail!("Approval #{id} is stale because its permission grant was revoked or expired");
        }
        let state = if approve {
            ApprovalState::Approved
        } else {
            ApprovalState::Denied
        };
        let changed = self.connection.execute(
            "UPDATE capability_approvals SET state = ?1
             WHERE id = ?2 AND state = 'pending' AND expires_at > ?3
               AND permission_profile = (
                   SELECT profile FROM permission_settings WHERE id = 1
               )
               AND grant_id IN (
                   SELECT id FROM permission_grants
                   WHERE revoked_at IS NULL
                     AND (expires_at IS NULL OR expires_at > ?3)
               )",
            params![state.as_str(), id, now],
        )?;
        if changed != 1 {
            bail!("Approval #{id} expired or changed while it was being resolved");
        }
        let resolved = self.get_approval(id)?;
        self.append_approval_audit(&resolved, profile, now)?;
        Ok(resolved)
    }

    pub fn revoke_approval(&self, id: i64, now: i64) -> Result<bool> {
        if now <= 0 {
            bail!("Approval revocation time must be a positive Unix timestamp");
        }
        let approval = self.get_approval(id)?;
        let changed = self.connection.execute(
            "UPDATE capability_approvals SET state = 'revoked'
             WHERE id = ?1 AND state IN ('pending', 'approved')",
            [id],
        )?;
        if changed == 1 {
            self.append_approval_audit(
                &ApprovalRecord {
                    state: ApprovalState::Revoked,
                    ..approval
                },
                self.permission_profile()?,
                now,
            )?;
        }
        Ok(changed == 1)
    }

    pub fn list_approvals(&self, now: i64) -> Result<Vec<ApprovalRecord>> {
        self.expire_approvals(now)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, request_id, capability_id, capability_version, operation,
                        request_fingerprint, argument_summary, required_permissions,
                        consequences, grant_id, permission_profile, scope, expires_at,
                        state, created_at
                 FROM capability_approvals ORDER BY id",
            )
            .context("Could not prepare approval query")?;
        let rows = statement
            .query_map([], approval_from_row)
            .context("Could not list approval records")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Could not read approval records")
    }

    pub fn list_capability_audit(&self, limit: u32) -> Result<Vec<CapabilityAuditRecord>> {
        if limit == 0 || limit > 1000 {
            bail!("Audit limit must be between 1 and 1000");
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, request_id, capability_id, capability_version, operation,
                        argument_summary,
                        permission_profile, authorization_decision, decision_reason, approval_id,
                        status, error_classification, created_at
                 FROM capability_audit ORDER BY id DESC LIMIT ?1",
            )
            .context("Could not prepare capability audit query")?;
        let rows = statement
            .query_map([limit], |row| {
                let profile: String = row.get(6)?;
                let status: String = row.get(10)?;
                Ok(CapabilityAuditRecord {
                    id: row.get(0)?,
                    request_id: row.get(1)?,
                    capability_id: row.get(2)?,
                    capability_version: row.get(3)?,
                    operation: row.get(4)?,
                    argument_summary: row.get(5)?,
                    permission_profile: PermissionProfile::parse(&profile).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(error.to_string())),
                        )
                    })?,
                    authorization_decision: row.get(7)?,
                    decision_reason: row.get(8)?,
                    approval_id: row.get(9)?,
                    status: CapabilityAuditStatus::parse(&status).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            10,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::other(error.to_string())),
                        )
                    })?,
                    error_classification: row.get(11)?,
                    created_at: row.get(12)?,
                })
            })
            .context("Could not query capability audit records")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Could not read capability audit records")
    }

    pub fn record_execution_event(
        &self,
        request_id: i64,
        status: CapabilityAuditStatus,
        error_classification: Option<&str>,
        now: i64,
    ) -> Result<i64> {
        if !matches!(
            status,
            CapabilityAuditStatus::Started
                | CapabilityAuditStatus::Completed
                | CapabilityAuditStatus::Failed
                | CapabilityAuditStatus::Cancelled
        ) {
            bail!("Execution event status is not a valid execution transition");
        }
        if error_classification.is_some_and(|value| {
            !matches!(
                value,
                "invalid_input"
                    | "invalid_output"
                    | "invalid_configuration"
                    | "dependency_unavailable"
                    | "capability_unavailable"
                    | "permission_denied"
                    | "approval_expired"
                    | "capability_failure"
                    | "cancelled"
                    | "unknown"
            )
        }) {
            bail!("Unknown execution error classification");
        }
        let previous = self.latest_audit(request_id)?;
        let valid_transition = match status {
            CapabilityAuditStatus::Started => {
                (previous.status == CapabilityAuditStatus::Proposed
                    && previous.authorization_decision == "allowed")
                    || (previous.status == CapabilityAuditStatus::Approved
                        && previous.authorization_decision == "approved")
            }
            CapabilityAuditStatus::Completed | CapabilityAuditStatus::Failed => {
                previous.status == CapabilityAuditStatus::Started
            }
            CapabilityAuditStatus::Cancelled => {
                previous.status == CapabilityAuditStatus::Started
                    || (previous.status == CapabilityAuditStatus::Proposed
                        && matches!(
                            previous.authorization_decision.as_str(),
                            "allowed" | "approval_required"
                        ))
                    || (previous.status == CapabilityAuditStatus::Approved
                        && previous.authorization_decision == "approved")
            }
            _ => false,
        };
        if !valid_transition {
            bail!(
                "Invalid capability audit transition from '{}' to '{}'",
                previous.status.as_str(),
                status.as_str()
            );
        }
        self.insert_audit_event(&previous, status, error_classification, now)
    }

    fn get_approval(&self, id: i64) -> Result<ApprovalRecord> {
        self.connection
            .query_row(
                "SELECT id, request_id, capability_id, capability_version, operation,
                        request_fingerprint, argument_summary, required_permissions,
                        consequences, grant_id, permission_profile, scope, expires_at,
                        state, created_at
                 FROM capability_approvals WHERE id = ?1",
                [id],
                approval_from_row,
            )
            .optional()
            .context("Could not read capability approval")?
            .with_context(|| format!("Approval #{id} does not exist"))
    }

    fn approval_ids_for_previous_profiles(&self, profile: PermissionProfile) -> Result<Vec<i64>> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM capability_approvals
             WHERE permission_profile != ?1 AND state IN ('pending', 'approved')",
        )?;
        statement
            .query_map([profile.as_str()], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Could not find approvals invalidated by profile change")
    }

    fn approvals_for_grant(&self, grant_id: i64) -> Result<Vec<ApprovalRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, request_id, capability_id, capability_version, operation,
                    request_fingerprint, argument_summary, required_permissions,
                    consequences, grant_id, permission_profile, scope, expires_at,
                    state, created_at
             FROM capability_approvals
             WHERE grant_id = ?1 AND state IN ('pending', 'approved')",
        )?;
        statement
            .query_map([grant_id], approval_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Could not find approvals invalidated by grant revocation")
    }

    fn set_approval_state(&self, id: i64, state: ApprovalState) -> Result<()> {
        self.connection
            .execute(
                "UPDATE capability_approvals SET state = ?1 WHERE id = ?2",
                params![state.as_str(), id],
            )
            .context("Could not update approval state")?;
        Ok(())
    }

    fn expire_approvals(&self, now: i64) -> Result<()> {
        let ids = {
            let mut statement = self.connection.prepare(
                "SELECT approval.id FROM capability_approvals AS approval
                 WHERE approval.state IN ('pending', 'approved')
                   AND (
                       approval.expires_at <= ?1
                       OR approval.permission_profile != (
                           SELECT profile FROM permission_settings WHERE id = 1
                       )
                       OR NOT EXISTS (
                           SELECT 1 FROM permission_grants AS grant_record
                           WHERE grant_record.id = approval.grant_id
                             AND grant_record.revoked_at IS NULL
                             AND (grant_record.expires_at IS NULL OR grant_record.expires_at > ?1)
                       )
                   )",
            )?;
            statement
                .query_map([now], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
        };
        for id in ids {
            let approval = self.get_approval(id)?;
            let grant_active: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM permission_grants
                 WHERE id = ?1 AND revoked_at IS NULL
                   AND (expires_at IS NULL OR expires_at > ?2))",
                params![approval.grant_id, now],
                |row| row.get(0),
            )?;
            let state = if !grant_active {
                ApprovalState::Revoked
            } else {
                ApprovalState::Expired
            };
            let changed = self.connection.execute(
                "UPDATE capability_approvals SET state = ?1
                 WHERE id = ?2 AND state IN ('pending', 'approved')",
                params![state.as_str(), id],
            )?;
            if changed == 1 {
                self.append_approval_audit(
                    &ApprovalRecord {
                        state,
                        ..approval.clone()
                    },
                    approval.permission_profile,
                    now,
                )?;
            }
        }
        Ok(())
    }

    fn append_approval_audit(
        &self,
        approval: &ApprovalRecord,
        profile: PermissionProfile,
        now: i64,
    ) -> Result<()> {
        let (decision, decision_reason, status, classification) = match approval.state {
            ApprovalState::Approved => (
                "approved",
                "User approved this exact capability request",
                CapabilityAuditStatus::Approved,
                None,
            ),
            ApprovalState::Denied => (
                "denied",
                "User denied this capability request",
                CapabilityAuditStatus::Denied,
                Some("approval_denied"),
            ),
            ApprovalState::Revoked => (
                "denied",
                "Approval was revoked",
                CapabilityAuditStatus::Cancelled,
                Some("approval_revoked"),
            ),
            ApprovalState::Expired => (
                "denied",
                "Approval expired before use",
                CapabilityAuditStatus::Denied,
                Some("approval_expired"),
            ),
            ApprovalState::Pending | ApprovalState::Consumed => {
                bail!("Approval state cannot be recorded as a user decision")
            }
        };
        self.connection
            .execute(
                "INSERT INTO capability_audit (
                    request_id, capability_id, capability_version, operation,
                    argument_summary, permission_profile, authorization_decision,
                    decision_reason, approval_id, status, error_classification, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    approval.request_id,
                    approval.capability_id,
                    approval.capability_version,
                    approval.operation,
                    approval.argument_summary,
                    profile.as_str(),
                    decision,
                    decision_reason,
                    approval.id,
                    status.as_str(),
                    classification,
                    now
                ],
            )
            .context("Could not persist approval audit event")?;
        Ok(())
    }

    fn latest_audit(&self, request_id: i64) -> Result<CapabilityAuditRecord> {
        self.connection
            .query_row(
                "SELECT id, request_id, capability_id, capability_version, operation,
                        argument_summary, permission_profile, authorization_decision,
                        decision_reason, approval_id, status, error_classification, created_at
                 FROM capability_audit WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
                [request_id],
                audit_from_row,
            )
            .optional()
            .context("Could not read capability audit history")?
            .with_context(|| format!("Capability request #{request_id} has no audit record"))
    }

    fn insert_audit_event(
        &self,
        previous: &CapabilityAuditRecord,
        status: CapabilityAuditStatus,
        classification: Option<&str>,
        now: i64,
    ) -> Result<i64> {
        self.connection
            .execute(
                "INSERT INTO capability_audit (
                    request_id, capability_id, capability_version, operation,
                    argument_summary, permission_profile, authorization_decision,
                    decision_reason, approval_id, status, error_classification, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    previous.request_id,
                    previous.capability_id,
                    previous.capability_version,
                    previous.operation,
                    previous.argument_summary,
                    previous.permission_profile.as_str(),
                    previous.authorization_decision,
                    previous.decision_reason,
                    previous.approval_id,
                    status.as_str(),
                    classification,
                    now
                ],
            )
            .context("Could not append capability audit event")?;
        Ok(self.connection.last_insert_rowid())
    }

    fn workspace_id(&self, root: &Path) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT id FROM workspaces WHERE root = ?1",
                [path_string(root)?],
                |row| row.get(0),
            )
            .optional()
            .context("Could not look up workspace")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Workspace {} is not initialized. Run `venus-agent init --workspace <path>` first.",
                    root.display()
                )
            })
    }
}

fn approval_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    let required_permissions: String = row.get(7)?;
    let consequences: String = row.get(8)?;
    let profile: String = row.get(10)?;
    let state: String = row.get(13)?;
    Ok(ApprovalRecord {
        id: row.get(0)?,
        request_id: row.get(1)?,
        capability_id: row.get(2)?,
        capability_version: row.get(3)?,
        operation: row.get(4)?,
        request_fingerprint: row.get(5)?,
        argument_summary: row.get(6)?,
        required_permissions: serde_json::from_str(&required_permissions).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?,
        consequences: serde_json::from_str(&consequences).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?,
        grant_id: row.get(9)?,
        permission_profile: PermissionProfile::parse(&profile).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?,
        scope: row.get(11)?,
        expires_at: row.get(12)?,
        state: ApprovalState::parse(&state).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                13,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?,
        created_at: row.get(14)?,
    })
}

fn audit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CapabilityAuditRecord> {
    let profile: String = row.get(6)?;
    let status: String = row.get(10)?;
    Ok(CapabilityAuditRecord {
        id: row.get(0)?,
        request_id: row.get(1)?,
        capability_id: row.get(2)?,
        capability_version: row.get(3)?,
        operation: row.get(4)?,
        argument_summary: row.get(5)?,
        permission_profile: PermissionProfile::parse(&profile).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?,
        authorization_decision: row.get(7)?,
        decision_reason: row.get(8)?,
        approval_id: row.get(9)?,
        status: CapabilityAuditStatus::parse(&status).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?,
        error_classification: row.get(11)?,
        created_at: row.get(12)?,
    })
}

fn matching_grant<'a>(
    grants: &'a [PermissionGrant],
    descriptor: &CapabilityDescriptor,
    request: &CapabilityRequest,
    now: i64,
) -> Option<&'a PermissionGrant> {
    grants.iter().find(|grant| {
        grant.is_active_at(now)
            && grant.capability_id == descriptor.id
            && grant.capability_version == descriptor.version
            && grant.operation == request.operation
            && descriptor.requirements.iter().all(|requirement| {
                grant
                    .permissions
                    .iter()
                    .any(|permission| permission == &requirement.permission)
            })
    })
}

struct AuditMetadata<'a> {
    profile: PermissionProfile,
    decision: &'a str,
    reason: &'a str,
    classification: Option<&'a str>,
    approval_id: Option<i64>,
    summary: &'a str,
    now: i64,
}

fn insert_proposal(
    connection: &Connection,
    descriptor: &CapabilityDescriptor,
    request: &CapabilityRequest,
    metadata: &AuditMetadata<'_>,
) -> Result<(i64, i64)> {
    connection
        .execute(
            "INSERT INTO capability_audit (
                request_id, capability_id, capability_version, operation,
                argument_summary, permission_profile, authorization_decision,
                decision_reason, approval_id, status, error_classification, created_at
             ) VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'proposed', ?9, ?10)",
            params![
                sanitize_audit_text(&descriptor.id, 128),
                sanitize_audit_text(&descriptor.version, 32),
                sanitize_audit_text(&request.operation, 128),
                metadata.summary,
                metadata.profile.as_str(),
                metadata.decision,
                sanitize_audit_text(metadata.reason, 512),
                metadata.approval_id,
                metadata.classification,
                metadata.now
            ],
        )
        .context("Could not record capability proposal")?;
    let id = connection.last_insert_rowid();
    connection
        .execute(
            "UPDATE capability_audit SET request_id = ?1 WHERE id = ?1",
            [id],
        )
        .context("Could not assign capability request identifier")?;
    Ok((id, id))
}

fn insert_audit_event(
    connection: &Connection,
    request_id: i64,
    descriptor: &CapabilityDescriptor,
    request: &CapabilityRequest,
    metadata: &AuditMetadata<'_>,
    status: CapabilityAuditStatus,
) -> Result<i64> {
    connection
        .execute(
            "INSERT INTO capability_audit (
                request_id, capability_id, capability_version, operation,
                argument_summary, permission_profile, authorization_decision,
                decision_reason, approval_id, status, error_classification, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                request_id,
                sanitize_audit_text(&descriptor.id, 128),
                sanitize_audit_text(&descriptor.version, 32),
                sanitize_audit_text(&request.operation, 128),
                metadata.summary,
                metadata.profile.as_str(),
                metadata.decision,
                sanitize_audit_text(metadata.reason, 512),
                metadata.approval_id,
                status.as_str(),
                metadata.classification,
                metadata.now
            ],
        )
        .context("Could not append capability audit event")?;
    Ok(connection.last_insert_rowid())
}

fn latest_audit_id(connection: &Connection, request_id: i64) -> Result<i64> {
    connection
        .query_row(
            "SELECT id FROM capability_audit WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
            [request_id],
            |row| row.get(0),
        )
        .context("Could not find capability audit request")
}

fn sanitize_audit_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .take(max_chars)
        .map(|character| {
            if character.is_ascii_graphic() || character == ' ' {
                character
            } else {
                '?'
            }
        })
        .collect()
}

fn migrate(connection: &Connection) -> Result<()> {
    let mut version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("Could not read database schema version")?;
    if version > 3 {
        bail!("Database schema version {version} is newer than this application.");
    }
    if version == 0 {
        connection
            .execute_batch(
                "BEGIN;
                     CREATE TABLE workspaces (
                         id INTEGER PRIMARY KEY,
                         root TEXT NOT NULL UNIQUE,
                         created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                     );
                     CREATE TABLE tasks (
                         id INTEGER PRIMARY KEY,
                         workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                         description TEXT NOT NULL CHECK (length(trim(description)) > 0),
                         status TEXT NOT NULL DEFAULT 'pending'
                             CHECK (status IN ('pending', 'completed')),
                         created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                         updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                     );
                     CREATE INDEX tasks_by_workspace ON tasks(workspace_id, id);
                     PRAGMA user_version = 1;
                     COMMIT;",
            )
            .context("Could not initialize database schema")?;
        version = 1;
    }
    if version == 1 {
        connection
            .execute_batch(
                "BEGIN;
             CREATE TABLE model_interactions (
                 id INTEGER PRIMARY KEY,
                 provider TEXT NOT NULL,
                 model TEXT NOT NULL,
                 input_tokens INTEGER,
                 output_tokens INTEGER,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE INDEX model_interactions_by_date ON model_interactions(created_at);
             PRAGMA user_version = 2;
             COMMIT;",
            )
            .context("Could not migrate model interaction metadata")?;
        version = 2;
    }
    if version == 2 {
        connection
            .execute_batch(
                "BEGIN;
             CREATE TABLE permission_settings (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 profile TEXT NOT NULL CHECK (profile IN ('open', 'controlled', 'restricted')),
                 updated_at INTEGER NOT NULL
             );
             INSERT INTO permission_settings (id, profile, updated_at)
                 VALUES (1, 'open', unixepoch());
             CREATE TABLE permission_grants (
                 id INTEGER PRIMARY KEY,
                 capability_id TEXT NOT NULL,
                 capability_version TEXT NOT NULL,
                 operation TEXT NOT NULL,
                 permissions TEXT NOT NULL,
                 requires_approval INTEGER NOT NULL CHECK (requires_approval IN (0, 1)),
                 expires_at INTEGER,
                 created_at INTEGER NOT NULL,
                 revoked_at INTEGER
             );
             CREATE UNIQUE INDEX permission_grants_one_active_scope
                 ON permission_grants(capability_id, capability_version, operation)
                 WHERE revoked_at IS NULL;
             CREATE TABLE capability_approvals (
                 id INTEGER PRIMARY KEY,
                 request_id INTEGER NOT NULL,
                 capability_id TEXT NOT NULL,
                 capability_version TEXT NOT NULL,
                 operation TEXT NOT NULL,
                 request_fingerprint TEXT NOT NULL,
                 argument_summary TEXT NOT NULL,
                 required_permissions TEXT NOT NULL,
                 consequences TEXT NOT NULL,
                 grant_id INTEGER NOT NULL REFERENCES permission_grants(id),
                 permission_profile TEXT NOT NULL
                     CHECK (permission_profile IN ('open', 'controlled', 'restricted')),
                 scope TEXT NOT NULL CHECK (scope = 'single_request'),
                 expires_at INTEGER NOT NULL,
                 state TEXT NOT NULL
                     CHECK (state IN ('pending', 'approved', 'denied', 'revoked', 'expired', 'consumed')),
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX capability_approvals_by_request ON capability_approvals(request_id);
             CREATE TABLE capability_audit (
                 id INTEGER PRIMARY KEY,
                 request_id INTEGER NOT NULL,
                 capability_id TEXT NOT NULL,
                 capability_version TEXT NOT NULL,
                 operation TEXT NOT NULL,
                 argument_summary TEXT NOT NULL,
                 permission_profile TEXT NOT NULL
                     CHECK (permission_profile IN ('open', 'controlled', 'restricted')),
                 authorization_decision TEXT NOT NULL
                     CHECK (authorization_decision IN ('allowed', 'denied', 'approval_required', 'approved')),
                 decision_reason TEXT NOT NULL,
                 approval_id INTEGER REFERENCES capability_approvals(id),
                 status TEXT NOT NULL CHECK (
                     status IN ('proposed', 'denied', 'approved', 'started', 'completed', 'failed', 'cancelled')
                 ),
                 error_classification TEXT,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX capability_audit_by_request ON capability_audit(request_id, id);
             CREATE INDEX capability_audit_by_date ON capability_audit(created_at);
             PRAGMA user_version = 3;
             COMMIT;",
            )
            .context("Could not migrate capability permissions and audit schema")?;
    }
    Ok(())
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Workspace path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityDependency, CapabilityState, ExecutionRequirement};
    use serde_json::json;

    fn test_capability() -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: "test.secret-read".to_owned(),
            name: "Secret reader".to_owned(),
            description: "Test-only capability contract".to_owned(),
            version: "1.2.3".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"token": {"type": "string"}},
                "required": ["token"],
                "additionalProperties": false
            }),
            output_schema: json!({"type": "string"}),
            configuration_schema: None,
            effect: crate::capability::CapabilityEffect::ReadOnly,
            effects: vec!["Reads a bounded data source.".to_owned()],
            operations: vec!["read".to_owned()],
            state: CapabilityState::Available,
            dependencies: vec![CapabilityDependency {
                id: "core.base".to_owned(),
                version: "1.0.0".to_owned(),
            }],
            requirements: vec![ExecutionRequirement {
                permission: "data.read".to_owned(),
                description: "Read a user-selected data source".to_owned(),
                sensitive: true,
            }],
        }
    }

    fn test_request() -> CapabilityRequest {
        CapabilityRequest {
            capability_id: "test.secret-read".to_owned(),
            capability_version: "1.2.3".to_owned(),
            operation: "read".to_owned(),
            input: json!({"token": "sensitive-value-must-not-be-persisted"}),
        }
    }

    fn test_grant(requires_approval: bool, expires_at: Option<i64>) -> PermissionGrantDraft {
        PermissionGrantDraft {
            capability_id: "test.secret-read".to_owned(),
            capability_version: "1.2.3".to_owned(),
            operation: "read".to_owned(),
            permissions: vec!["data.read".to_owned()],
            requires_approval,
            expires_at,
        }
    }

    #[test]
    fn tasks_are_scoped_to_workspace_and_complete_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        let first = first.canonicalize()?;
        let second = second.canonicalize()?;
        database.register_workspace(&first)?;
        database.register_workspace(&second)?;

        let id = database.add_task(&first, "Run the focused test suite")?;
        assert_eq!(database.list_tasks(&first)?.len(), 1);
        assert!(database.list_tasks(&second)?.is_empty());
        assert!(database.complete_task(&first, id)?);
        assert!(!database.complete_task(&first, id)?);
        assert_eq!(database.list_tasks(&first)?[0].status, "completed");
        Ok(())
    }

    #[test]
    fn rejects_empty_task_descriptions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        let workspace = temp.path().canonicalize()?;
        database.register_workspace(&workspace)?;
        assert!(database.add_task(&workspace, "  ").is_err());
        Ok(())
    }

    #[test]
    fn rejects_a_database_schema_newer_than_the_application() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "user_version", 99)?;
        drop(connection);

        let error = Database::open(&path)
            .err()
            .expect("future schema must fail");
        assert!(error.to_string().contains("newer than this application"));
        Ok(())
    }

    #[test]
    fn retention_prunes_only_old_completed_tasks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        let workspace = temp.path().canonicalize()?;
        database.register_workspace(&workspace)?;

        let old_completed = database.add_task(&workspace, "Old completed task")?;
        let recent_completed = database.add_task(&workspace, "Recent completed task")?;
        let old_pending = database.add_task(&workspace, "Old pending task")?;
        database.complete_task(&workspace, old_completed)?;
        database.complete_task(&workspace, recent_completed)?;
        database.connection.execute(
            "UPDATE tasks SET updated_at = datetime('now', '-60 days') WHERE id IN (?1, ?2)",
            params![old_completed, old_pending],
        )?;

        assert_eq!(database.prune_completed_tasks(30)?, 1);
        let tasks = database.list_tasks(&workspace)?;
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|task| task.id == recent_completed));
        assert!(tasks.iter().any(|task| task.id == old_pending));
        Ok(())
    }

    #[test]
    fn persists_provider_metadata_without_prompt_or_response_fields() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        database.record_model_interaction("local", "qwen:7b", Some(12), Some(5))?;
        let (provider, model, input, output): (String, String, i64, i64) =
            database.connection.query_row(
                "SELECT provider, model, input_tokens, output_tokens FROM model_interactions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(
            (provider.as_str(), model.as_str(), input, output),
            ("local", "qwen:7b", 12, 5)
        );
        Ok(())
    }

    #[test]
    fn model_metadata_retention_removes_only_expired_records() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        database.record_model_interaction("local", "old-model", None, None)?;
        database.record_model_interaction("cloud", "recent-model", Some(1), Some(1))?;
        database.connection.execute(
            "UPDATE model_interactions SET created_at = datetime('now', '-60 days') WHERE model = 'old-model'",
            [],
        )?;
        drop(database);
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        assert_eq!(database.prune_model_interactions(30)?, 1);
        let count: i64 = database.connection.query_row(
            "SELECT COUNT(*) FROM model_interactions",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[test]
    fn migrates_existing_v1_database_without_losing_tasks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                root TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE tasks (
                id INTEGER PRIMARY KEY,
                workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                description TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX tasks_by_workspace ON tasks(workspace_id, id);
            PRAGMA user_version = 1;",
        )?;
        drop(connection);

        let database = Database::open(&path)?;
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 3);
        assert_eq!(database.permission_profile()?, PermissionProfile::Open);
        database.record_model_interaction("test", "model", None, None)?;
        Ok(())
    }

    #[test]
    fn migrates_v2_database_and_preserves_provider_metadata() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                root TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE tasks (
                id INTEGER PRIMARY KEY,
                workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                description TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE model_interactions (
                id INTEGER PRIMARY KEY,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO model_interactions (provider, model, input_tokens, output_tokens)
                VALUES ('local', 'model-v2', 4, 2);
            PRAGMA user_version = 2;",
        )?;
        drop(connection);

        let database = Database::open(&path)?;
        assert_eq!(database.permission_profile()?, PermissionProfile::Open);
        let (model, input_tokens): (String, i64) = database.connection.query_row(
            "SELECT model, input_tokens FROM model_interactions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(model, "model-v2");
        assert_eq!(input_tokens, 4);
        Ok(())
    }

    #[test]
    fn grants_profiles_and_revocation_persist_across_reopen() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("state.sqlite3");
        let database = Database::open(&path)?;
        assert_eq!(database.permission_profile()?, PermissionProfile::Open);
        database.set_permission_profile(PermissionProfile::Controlled, 100)?;
        let id = database.create_permission_grant(&test_grant(false, Some(500)), 101)?;
        assert!(
            database
                .create_permission_grant(&test_grant(true, None), 102)
                .is_err()
        );
        drop(database);

        let database = Database::open(&path)?;
        assert_eq!(
            database.permission_profile()?,
            PermissionProfile::Controlled
        );
        let grants = database.list_permission_grants()?;
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].id, id);
        assert!(grants[0].is_active_at(200));
        assert!(!grants[0].is_active_at(500));
        assert!(database.revoke_permission_grant(id, 201)?);
        assert!(!database.revoke_permission_grant(id, 202)?);
        assert_eq!(database.list_permission_grants()?[0].revoked_at, Some(201));
        Ok(())
    }

    #[test]
    fn scoped_approval_and_audit_persist_without_sensitive_arguments() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("state.sqlite3");
        let database = Database::open(&path)?;
        database.set_permission_profile(PermissionProfile::Controlled, 100)?;
        database.create_permission_grant(&test_grant(true, Some(10_000)), 101)?;
        let capability = test_capability();
        let request = test_request();
        let result = database.authorize_capability(&capability, &request, None, 102)?;
        assert!(matches!(
            result.decision,
            AuthorizationDecision::RequiresApproval { .. }
        ));
        let approval_id = result.approval_id.context("missing approval id")?;
        assert!(
            database
                .record_execution_event(
                    result.request_id,
                    CapabilityAuditStatus::Started,
                    None,
                    103
                )
                .is_err()
        );
        drop(database);

        let database = Database::open(&path)?;
        let approvals = database.list_approvals(104)?;
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].id, approval_id);
        assert_eq!(approvals[0].state, ApprovalState::Pending);
        assert_eq!(approvals[0].scope, "single_request");
        assert_eq!(
            approvals[0].permission_profile,
            PermissionProfile::Controlled
        );
        assert_eq!(
            approvals[0].required_permissions,
            vec!["data.read".to_owned()]
        );
        assert!(
            !approvals[0]
                .argument_summary
                .contains("sensitive-value-must-not-be-persisted")
        );

        let approved = database.resolve_approval(approval_id, true, 105)?;
        assert_eq!(approved.state, ApprovalState::Approved);
        let authorized =
            database.authorize_capability(&capability, &request, Some(approval_id), 106)?;
        assert!(matches!(
            authorized.decision,
            AuthorizationDecision::Allowed { .. }
        ));
        assert_eq!(authorized.request_id, result.request_id);
        assert_eq!(
            database.list_approvals(107)?[0].state,
            ApprovalState::Consumed
        );
        database.record_execution_event(
            result.request_id,
            CapabilityAuditStatus::Started,
            None,
            107,
        )?;
        assert!(
            matches!(
                database
                    .authorize_capability(&capability, &request, Some(approval_id), 108)?
                    .decision,
                AuthorizationDecision::Denied { .. }
            ),
            "single-request approval must not be reusable"
        );
        database.record_execution_event(
            result.request_id,
            CapabilityAuditStatus::Failed,
            Some("capability_failure"),
            109,
        )?;

        let open_request = test_request();
        database.set_permission_profile(PermissionProfile::Open, 110)?;
        let open_result =
            database.authorize_capability(&capability, &open_request, Some(approval_id), 111)?;
        assert!(matches!(
            open_result.decision,
            AuthorizationDecision::Allowed { .. }
        ));
        database.record_execution_event(
            open_result.request_id,
            CapabilityAuditStatus::Started,
            None,
            112,
        )?;
        database.record_execution_event(
            open_result.request_id,
            CapabilityAuditStatus::Failed,
            Some("capability_failure"),
            113,
        )?;

        let audit = database.list_capability_audit(50)?;
        assert!(audit.iter().all(|record| {
            !record
                .argument_summary
                .contains("sensitive-value-must-not-be-persisted")
        }));
        assert!(audit.iter().any(|record| {
            record.request_id == result.request_id
                && record.status == CapabilityAuditStatus::Approved
                && record.decision_reason.contains("User approved")
        }));
        assert!(audit.iter().any(|record| {
            record.request_id == open_result.request_id
                && record.status == CapabilityAuditStatus::Failed
                && record.error_classification.as_deref() == Some("capability_failure")
        }));
        Ok(())
    }

    #[test]
    fn execution_authorization_records_started_atomically() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        let result = database.authorize_capability_for_execution(
            &test_capability(),
            &test_request(),
            None,
            100,
        )?;
        assert!(matches!(
            result.decision,
            AuthorizationDecision::Allowed { .. }
        ));
        let audit = database.list_capability_audit(10)?;
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0].request_id, result.request_id);
        assert_eq!(audit[0].status, CapabilityAuditStatus::Started);
        assert_eq!(audit[1].status, CapabilityAuditStatus::Proposed);
        Ok(())
    }

    #[test]
    fn profile_change_invalidates_pending_approval() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        database.set_permission_profile(PermissionProfile::Controlled, 100)?;
        database.create_permission_grant(&test_grant(true, None), 101)?;
        let result =
            database.authorize_capability(&test_capability(), &test_request(), None, 102)?;
        let approval_id = result.approval_id.context("missing approval id")?;
        database.set_permission_profile(PermissionProfile::Restricted, 103)?;
        assert_eq!(
            database.list_approvals(104)?[0].state,
            ApprovalState::Expired
        );
        assert!(database.resolve_approval(approval_id, true, 105).is_err());
        assert!(database.list_capability_audit(50)?.iter().any(|event| {
            event.request_id == result.request_id
                && event.status == CapabilityAuditStatus::Denied
                && event.error_classification.as_deref() == Some("approval_expired")
        }));
        Ok(())
    }

    #[test]
    fn grant_revocation_and_expiry_invalidate_approvals_and_are_audited() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        database.set_permission_profile(PermissionProfile::Controlled, 100)?;
        let grant = database.create_permission_grant(&test_grant(true, Some(1_000)), 101)?;
        let pending =
            database.authorize_capability(&test_capability(), &test_request(), None, 102)?;
        let pending_id = pending.approval_id.context("missing pending approval")?;
        assert!(database.revoke_permission_grant(grant, 103)?);
        assert_eq!(
            database.list_approvals(104)?[0].state,
            ApprovalState::Revoked
        );
        assert!(database.list_capability_audit(50)?.iter().any(|event| {
            event.request_id == pending.request_id
                && event.status == CapabilityAuditStatus::Cancelled
                && event.error_classification.as_deref() == Some("approval_revoked")
        }));
        assert!(database.resolve_approval(pending_id, true, 105).is_err());

        let another_grant =
            database.create_permission_grant(&test_grant(true, Some(10_000)), 106)?;
        let expiring =
            database.authorize_capability(&test_capability(), &test_request(), None, 107)?;
        let expiring_id = expiring.approval_id.context("missing expiring approval")?;
        assert_eq!(
            database.list_approvals(107 + APPROVAL_TTL_SECONDS)?[1].state,
            ApprovalState::Expired
        );
        assert!(database.list_capability_audit(50)?.iter().any(|event| {
            event.request_id == expiring.request_id
                && event.status == CapabilityAuditStatus::Denied
                && event.error_classification.as_deref() == Some("approval_expired")
        }));
        assert!(database.revoke_permission_grant(another_grant, 500)?);
        assert!(database.resolve_approval(expiring_id, true, 501).is_err());

        database.create_permission_grant(&test_grant(true, Some(600)), 501)?;
        let grant_expiring =
            database.authorize_capability(&test_capability(), &test_request(), None, 502)?;
        let grant_expiring_id = grant_expiring
            .approval_id
            .context("missing grant-expiring approval")?;
        assert_eq!(
            database.list_approvals(601)?[2].state,
            ApprovalState::Revoked
        );
        assert!(database.list_capability_audit(50)?.iter().any(|event| {
            event.request_id == grant_expiring.request_id
                && event.status == CapabilityAuditStatus::Cancelled
                && event.error_classification.as_deref() == Some("approval_revoked")
        }));
        assert!(
            database
                .resolve_approval(grant_expiring_id, true, 602)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn invalid_untrusted_metadata_is_denied_and_terminal_controls_are_sanitized() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        let mut descriptor = test_capability();
        descriptor.id = "test.\u{1b}[31m".to_owned();
        let request = CapabilityRequest {
            capability_id: descriptor.id.clone(),
            ..test_request()
        };
        let result = database.authorize_capability(&descriptor, &request, None, 100)?;
        assert!(matches!(
            result.decision,
            AuthorizationDecision::Denied { .. }
        ));
        let audit = database.list_capability_audit(10)?;
        assert_eq!(audit.len(), 2);
        assert!(
            audit
                .iter()
                .all(|record| !record.capability_id.contains('\u{1b}'))
        );
        assert!(
            audit
                .iter()
                .any(|record| record.status == CapabilityAuditStatus::Denied)
        );
        Ok(())
    }
}
