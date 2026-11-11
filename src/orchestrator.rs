use crate::{
    capability::{
        CapabilityDescriptor, CapabilityEffect, CapabilityError, CapabilityErrorKind,
        CapabilityRegistry, CapabilityRequest, CapabilityState, ExecutionRequirement,
    },
    database::Database,
    permissions::{AuthorizationDecision, CapabilityAuditStatus},
    repository,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait CapabilityExecutor: Send + Sync {
    fn descriptor(&self) -> &CapabilityDescriptor;

    async fn execute(
        &self,
        request: &CapabilityRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError>;
}

#[derive(Debug, PartialEq)]
pub enum CapabilityRunResult {
    AwaitingApproval {
        request_id: i64,
        approval_id: i64,
        reason: String,
    },
    Denied {
        request_id: i64,
        reason: String,
    },
    Cancelled {
        request_id: i64,
    },
    Completed {
        request_id: i64,
        output: Value,
    },
}

#[derive(Default)]
pub struct CapabilityOrchestrator {
    registry: CapabilityRegistry,
    executors: BTreeMap<String, Box<dyn CapabilityExecutor>>,
}

impl CapabilityOrchestrator {
    pub fn register(&mut self, executor: Box<dyn CapabilityExecutor>) -> Result<()> {
        let descriptor = executor.descriptor().clone();
        descriptor.validate()?;
        if descriptor.effect == CapabilityEffect::Mutating
            && !descriptor
                .requirements
                .iter()
                .any(|requirement| requirement.permission == "workspace.write")
        {
            bail!(
                "Mutating capability '{}' must declare the workspace.write permission",
                descriptor.id
            );
        }
        let id = descriptor.id.clone();
        self.registry.register(descriptor)?;
        self.executors.insert(id, executor);
        Ok(())
    }

    pub fn discover(&self) -> impl Iterator<Item = &CapabilityDescriptor> {
        self.registry.discover()
    }

    pub async fn run(
        &self,
        database: &Database,
        request: &CapabilityRequest,
        approval_id: Option<i64>,
        now: i64,
        cancellation: CancellationToken,
    ) -> Result<CapabilityRunResult> {
        let descriptor = self
            .registry
            .get(&request.capability_id)
            .with_context(|| format!("Capability '{}' is not registered", request.capability_id))?;
        let executor = self
            .executors
            .get(&request.capability_id)
            .context("Registered capability has no executor")?;
        if cancellation.is_cancelled() {
            bail!("Capability request was cancelled before authorization");
        }

        let authorization =
            database.authorize_capability_for_execution(descriptor, request, approval_id, now)?;
        match authorization.decision {
            AuthorizationDecision::Denied { reason } => {
                return Ok(CapabilityRunResult::Denied {
                    request_id: authorization.request_id,
                    reason,
                });
            }
            AuthorizationDecision::RequiresApproval { reason } => {
                let approval_id = authorization
                    .approval_id
                    .context("Approval-required decision did not create an approval record")?;
                return Ok(CapabilityRunResult::AwaitingApproval {
                    request_id: authorization.request_id,
                    approval_id,
                    reason,
                });
            }
            AuthorizationDecision::Allowed { .. } => {}
        }

        let execution = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                database.record_execution_event(
                    authorization.request_id,
                    CapabilityAuditStatus::Cancelled,
                    Some("cancelled"),
                    now,
                )?;
                return Ok(CapabilityRunResult::Cancelled {
                    request_id: authorization.request_id,
                });
            }
            result = executor.execute(request, cancellation.clone()) => result,
        };
        let output = match execution {
            Ok(output) => output,
            Err(error) => {
                if error.kind == CapabilityErrorKind::Cancelled {
                    database.record_execution_event(
                        authorization.request_id,
                        CapabilityAuditStatus::Cancelled,
                        Some("cancelled"),
                        now,
                    )?;
                    return Ok(CapabilityRunResult::Cancelled {
                        request_id: authorization.request_id,
                    });
                }
                database.record_execution_event(
                    authorization.request_id,
                    CapabilityAuditStatus::Failed,
                    Some(capability_error_classification(error.kind)),
                    now,
                )?;
                return Err(anyhow::anyhow!(
                    "Capability '{}' failed: {}",
                    descriptor.id,
                    error.message
                ));
            }
        };
        if let Err(error) = descriptor.validate_output(&output) {
            database.record_execution_event(
                authorization.request_id,
                CapabilityAuditStatus::Failed,
                Some("invalid_output"),
                now,
            )?;
            bail!(
                "Capability '{}' returned invalid output: {error}",
                descriptor.id
            );
        }
        database.record_execution_event(
            authorization.request_id,
            CapabilityAuditStatus::Completed,
            None,
            now,
        )?;
        Ok(CapabilityRunResult::Completed {
            request_id: authorization.request_id,
            output,
        })
    }
}

pub fn register_workspace_capabilities(
    orchestrator: &mut CapabilityOrchestrator,
    root: PathBuf,
) -> Result<()> {
    orchestrator.register(Box::new(workspace_summary_capability(root.clone())))?;
    orchestrator.register(Box::new(workspace_file_read_capability(root.clone())))?;
    orchestrator.register(Box::new(workspace_search_capability(root.clone())))?;
    orchestrator.register(Box::new(workspace_file_replace_capability(root)))?;
    Ok(())
}

pub fn workspace_summary_capability(root: PathBuf) -> WorkspaceSummaryCapability {
    WorkspaceSummaryCapability {
        descriptor: CapabilityDescriptor {
            id: "workspace.summary".to_owned(),
            name: "Workspace summary".to_owned(),
            description:
                "Summarize file counts and top-level project manifests in the selected workspace."
                    .to_owned(),
            version: "1.0.0".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"workspace": {"type": "string"}},
                "required": ["workspace"],
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "workspace": {"type": "string"},
                    "file_count": {"type": "integer"},
                    "manifests": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["workspace", "file_count", "manifests"],
                "additionalProperties": false
            }),
            configuration_schema: None,
            effect: CapabilityEffect::ReadOnly,
            effects: vec![
                "Reads file and directory names and applicable ignore rules under the selected workspace."
                    .to_owned(),
                "Honors ignore rules and skips common generated directories.".to_owned(),
                "Does not return project file contents or modify the workspace.".to_owned(),
            ],
            operations: vec!["summarize".to_owned()],
            state: CapabilityState::Available,
            dependencies: Vec::new(),
            requirements: vec![ExecutionRequirement {
                permission: "workspace.read".to_owned(),
                description: "Read workspace file and directory names.".to_owned(),
                sensitive: false,
            }],
        },
        root,
    }
}

pub fn workspace_file_read_capability(root: PathBuf) -> WorkspaceFileReadCapability {
    WorkspaceFileReadCapability {
        descriptor: CapabilityDescriptor {
            id: "workspace.file-read".to_owned(),
            name: "Read workspace file".to_owned(),
            description: "Read one visible UTF-8 text file under the selected workspace."
                .to_owned(),
            version: "1.0.0".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "size_bytes": {"type": "integer"}
                },
                "required": ["path", "content", "size_bytes"],
                "additionalProperties": false
            }),
            configuration_schema: None,
            effect: CapabilityEffect::ReadOnly,
            effects: vec![
                "Reads UTF-8 file content from the selected workspace.".to_owned(),
                "Honors hidden-file, ignore, and generated-directory exclusions.".to_owned(),
                "Rejects symbolic links and paths outside the workspace; limits files to 256 KiB."
                    .to_owned(),
                "Limits the visibility scan to 100,000 files.".to_owned(),
            ],
            operations: vec!["read".to_owned()],
            state: CapabilityState::Available,
            dependencies: Vec::new(),
            requirements: vec![ExecutionRequirement {
                permission: "workspace.read".to_owned(),
                description: "Read a visible text file under the selected workspace.".to_owned(),
                sensitive: true,
            }],
        },
        root,
    }
}

pub fn workspace_search_capability(root: PathBuf) -> WorkspaceSearchCapability {
    WorkspaceSearchCapability {
        descriptor: CapabilityDescriptor {
            id: "workspace.search".to_owned(),
            name: "Search workspace text".to_owned(),
            description: "Search visible UTF-8 text files under the selected workspace.".to_owned(),
            version: "1.0.0".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "case_sensitive": {"type": "boolean"}
                },
                "required": ["query", "case_sensitive"],
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "files_scanned": {"type": "integer"},
                    "truncated": {"type": "boolean"},
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "line_number": {"type": "integer"},
                                "line": {"type": "string"}
                            },
                            "required": ["path", "line_number", "line"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["files_scanned", "truncated", "matches"],
                "additionalProperties": false
            }),
            configuration_schema: None,
            effect: CapabilityEffect::ReadOnly,
            effects: vec![
                "Searches visible UTF-8 file content under the selected workspace.".to_owned(),
                "Honors hidden-file, ignore, and generated-directory exclusions.".to_owned(),
                "Limits each file to 256 KiB, total scanned data to 16 MiB, and results to 50 matches."
                    .to_owned(),
            ],
            operations: vec!["search".to_owned()],
            state: CapabilityState::Available,
            dependencies: Vec::new(),
            requirements: vec![ExecutionRequirement {
                permission: "workspace.read".to_owned(),
                description: "Search visible text files under the selected workspace.".to_owned(),
                sensitive: true,
            }],
        },
        root,
    }
}

pub fn workspace_file_replace_capability(root: PathBuf) -> WorkspaceFileReplaceCapability {
    WorkspaceFileReplaceCapability {
        descriptor: CapabilityDescriptor {
            id: "workspace.file-replace".to_owned(),
            name: "Replace exact text in workspace file".to_owned(),
            description: "Replace one exact, unique single-line text occurrence in a visible workspace file."
                .to_owned(),
            version: "1.0.0".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "expected_sha256": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"}
                },
                "required": ["path", "expected_sha256", "old_text", "new_text"],
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "size_bytes": {"type": "integer"},
                    "sha256": {"type": "string"}
                },
                "required": ["path", "size_bytes", "sha256"],
                "additionalProperties": false
            }),
            configuration_schema: None,
            effect: CapabilityEffect::Mutating,
            effects: vec![
                "Replaces exactly one occurrence of the proposed single-line text in a visible UTF-8 workspace file."
                    .to_owned(),
                "Rejects stale file content, hidden/ignored/generated files, symbolic links, and paths outside the workspace."
                    .to_owned(),
                "Requires a workspace.write grant and a fresh exact-request approval in every permission profile."
                    .to_owned(),
            ],
            operations: vec!["replace".to_owned()],
            state: CapabilityState::Available,
            dependencies: Vec::new(),
            requirements: vec![ExecutionRequirement {
                permission: "workspace.write".to_owned(),
                description: "Replace a single exact text occurrence in a workspace file.".to_owned(),
                sensitive: true,
            }],
        },
        root,
    }
}

pub struct WorkspaceSummaryCapability {
    descriptor: CapabilityDescriptor,
    root: PathBuf,
}

impl WorkspaceSummaryCapability {
    pub fn request(&self) -> CapabilityRequest {
        CapabilityRequest {
            capability_id: self.descriptor.id.clone(),
            capability_version: self.descriptor.version.clone(),
            operation: "summarize".to_owned(),
            input: json!({"workspace": self.root.to_string_lossy()}),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl CapabilityExecutor for WorkspaceSummaryCapability {
    fn descriptor(&self) -> &CapabilityDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        request: &CapabilityRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        let requested_workspace = request
            .input
            .get("workspace")
            .and_then(Value::as_str)
            .context("Workspace path is missing from capability input")
            .map_err(|error| CapabilityError {
                kind: CapabilityErrorKind::InvalidInput,
                message: error.to_string(),
            })?;
        if Path::new(requested_workspace) != self.root {
            return Err(CapabilityError {
                kind: CapabilityErrorKind::InvalidInput,
                message: "Capability input does not match the selected workspace".to_owned(),
            });
        }
        if cancellation.is_cancelled() {
            return Err(CapabilityError {
                kind: CapabilityErrorKind::Cancelled,
                message: "Capability execution was cancelled".to_owned(),
            });
        }
        let summary =
            repository::workspace_summary(&self.root).map_err(|error| CapabilityError {
                kind: CapabilityErrorKind::OperationFailed,
                message: error.to_string(),
            })?;
        let workspace_name = self
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".to_owned());
        Ok(json!({
            "workspace": workspace_name,
            "file_count": summary.file_count,
            "manifests": summary.manifests,
        }))
    }
}

pub struct WorkspaceFileReadCapability {
    descriptor: CapabilityDescriptor,
    root: PathBuf,
}

impl WorkspaceFileReadCapability {
    pub fn request(&self, path: String) -> CapabilityRequest {
        CapabilityRequest {
            capability_id: self.descriptor.id.clone(),
            capability_version: self.descriptor.version.clone(),
            operation: "read".to_owned(),
            input: json!({"path": path}),
        }
    }
}

#[async_trait]
impl CapabilityExecutor for WorkspaceFileReadCapability {
    fn descriptor(&self) -> &CapabilityDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        request: &CapabilityRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        if cancellation.is_cancelled() {
            return Err(CapabilityError {
                kind: CapabilityErrorKind::Cancelled,
                message: "Capability execution was cancelled".to_owned(),
            });
        }
        let path = request.input["path"]
            .as_str()
            .context("Workspace file path must be a string")
            .map_err(operation_failed)?;
        let file = repository::read_workspace_file(&self.root, path).map_err(operation_failed)?;
        Ok(json!({
            "path": file.path,
            "content": file.content,
            "size_bytes": file.size_bytes
        }))
    }
}

pub struct WorkspaceSearchCapability {
    descriptor: CapabilityDescriptor,
    root: PathBuf,
}

impl WorkspaceSearchCapability {
    pub fn request(&self, query: String, case_sensitive: bool) -> CapabilityRequest {
        CapabilityRequest {
            capability_id: self.descriptor.id.clone(),
            capability_version: self.descriptor.version.clone(),
            operation: "search".to_owned(),
            input: json!({"query": query, "case_sensitive": case_sensitive}),
        }
    }
}

pub struct WorkspaceFileReplaceCapability {
    descriptor: CapabilityDescriptor,
    root: PathBuf,
}

impl WorkspaceFileReplaceCapability {
    pub fn request(
        &self,
        path: String,
        expected_sha256: String,
        old_text: String,
        new_text: String,
    ) -> CapabilityRequest {
        CapabilityRequest {
            capability_id: self.descriptor.id.clone(),
            capability_version: self.descriptor.version.clone(),
            operation: "replace".to_owned(),
            input: json!({
                "path": path,
                "expected_sha256": expected_sha256,
                "old_text": old_text,
                "new_text": new_text
            }),
        }
    }
}

#[async_trait]
impl CapabilityExecutor for WorkspaceFileReplaceCapability {
    fn descriptor(&self) -> &CapabilityDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        request: &CapabilityRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        if cancellation.is_cancelled() {
            return Err(CapabilityError {
                kind: CapabilityErrorKind::Cancelled,
                message: "Capability execution was cancelled".to_owned(),
            });
        }
        let path = request.input["path"]
            .as_str()
            .context("Workspace file path must be a string")
            .map_err(operation_failed)?;
        let expected_sha256 = request.input["expected_sha256"]
            .as_str()
            .context("Expected file digest must be a string")
            .map_err(operation_failed)?;
        let old_text = request.input["old_text"]
            .as_str()
            .context("Text to replace must be a string")
            .map_err(operation_failed)?;
        let new_text = request.input["new_text"]
            .as_str()
            .context("Replacement text must be a string")
            .map_err(operation_failed)?;
        let file = repository::replace_workspace_file(
            &self.root,
            path,
            expected_sha256,
            old_text,
            new_text,
        )
        .map_err(operation_failed)?;
        Ok(json!({
            "path": file.path,
            "size_bytes": file.size_bytes,
            "sha256": repository::content_sha256(file.content.as_bytes())
        }))
    }
}

#[async_trait]
impl CapabilityExecutor for WorkspaceSearchCapability {
    fn descriptor(&self) -> &CapabilityDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        request: &CapabilityRequest,
        _cancellation: CancellationToken,
    ) -> std::result::Result<Value, CapabilityError> {
        let query = request.input["query"]
            .as_str()
            .context("Workspace search query must be a string")
            .map_err(operation_failed)?;
        let case_sensitive = request.input["case_sensitive"]
            .as_bool()
            .context("Workspace search case_sensitive must be a boolean")
            .map_err(operation_failed)?;
        let result = repository::search_workspace(&self.root, query, case_sensitive)
            .map_err(operation_failed)?;
        let matches = result
            .matches
            .into_iter()
            .map(|matched| {
                json!({
                    "path": matched.path,
                    "line_number": matched.line_number,
                    "line": matched.line
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "files_scanned": result.files_scanned,
            "truncated": result.truncated,
            "matches": matches
        }))
    }
}

fn operation_failed(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError {
        kind: CapabilityErrorKind::OperationFailed,
        message: error.to_string(),
    }
}

fn capability_error_classification(kind: CapabilityErrorKind) -> &'static str {
    match kind {
        CapabilityErrorKind::InvalidInput => "invalid_input",
        CapabilityErrorKind::InvalidOutput => "invalid_output",
        CapabilityErrorKind::InvalidConfiguration => "invalid_configuration",
        CapabilityErrorKind::DependencyUnavailable => "dependency_unavailable",
        CapabilityErrorKind::Unavailable => "capability_unavailable",
        CapabilityErrorKind::OperationFailed => "capability_failure",
        CapabilityErrorKind::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capability::CapabilityEffect,
        permissions::{ApprovalState, PermissionGrantDraft, PermissionProfile},
    };
    use anyhow::Result;
    use serde_json::json;
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::tempdir;

    struct TestCapability {
        descriptor: CapabilityDescriptor,
        calls: AtomicUsize,
        output: Value,
        failure: Option<CapabilityError>,
    }

    #[async_trait]
    impl CapabilityExecutor for TestCapability {
        fn descriptor(&self) -> &CapabilityDescriptor {
            &self.descriptor
        }

        async fn execute(
            &self,
            _request: &CapabilityRequest,
            _cancellation: CancellationToken,
        ) -> std::result::Result<Value, CapabilityError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.failure {
                Some(error) => Err(error.clone()),
                None => Ok(self.output.clone()),
            }
        }
    }

    fn test_capability(effect: CapabilityEffect) -> TestCapability {
        TestCapability {
            descriptor: CapabilityDescriptor {
                id: "test.reader".to_owned(),
                name: "Test reader".to_owned(),
                description: "Reads test data.".to_owned(),
                version: "1.0.0".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                output_schema: json!({
                    "type": "object",
                    "properties": {"result": {"type": "string"}},
                    "required": ["result"],
                    "additionalProperties": false
                }),
                configuration_schema: None,
                effect,
                effects: vec!["Reads test data without changing it.".to_owned()],
                operations: vec!["read".to_owned()],
                state: CapabilityState::Available,
                dependencies: Vec::new(),
                requirements: vec![ExecutionRequirement {
                    permission: "data.read".to_owned(),
                    description: "Read test data.".to_owned(),
                    sensitive: false,
                }],
            },
            calls: AtomicUsize::new(0),
            output: json!({"result": "read-only"}),
            failure: None,
        }
    }

    fn test_request() -> CapabilityRequest {
        CapabilityRequest {
            capability_id: "test.reader".to_owned(),
            capability_version: "1.0.0".to_owned(),
            operation: "read".to_owned(),
            input: json!({}),
        }
    }

    fn database() -> Result<(tempfile::TempDir, Database)> {
        let temp = tempdir()?;
        let database = Database::open(&temp.path().join("state.sqlite3"))?;
        Ok((temp, database))
    }

    #[tokio::test]
    async fn open_profile_executes_read_only_capability_and_audits_completion() -> Result<()> {
        let (_temp, database) = database()?;
        let capability = test_capability(CapabilityEffect::ReadOnly);
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        let result = orchestrator
            .run(
                &database,
                &test_request(),
                None,
                100,
                CancellationToken::new(),
            )
            .await?;
        assert!(matches!(result, CapabilityRunResult::Completed { .. }));
        let events = database.list_capability_audit(10)?;
        assert_eq!(events[0].status, CapabilityAuditStatus::Completed);
        assert_eq!(events[1].status, CapabilityAuditStatus::Started);
        assert_eq!(events[2].status, CapabilityAuditStatus::Proposed);
        Ok(())
    }

    #[tokio::test]
    async fn executor_failure_is_reported_and_audited_without_error_details() -> Result<()> {
        let (_temp, database) = database()?;
        let mut capability = test_capability(CapabilityEffect::ReadOnly);
        capability.failure = Some(CapabilityError {
            kind: CapabilityErrorKind::OperationFailed,
            message: "simulated failure with private detail".to_owned(),
        });
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        let result = orchestrator
            .run(
                &database,
                &test_request(),
                None,
                100,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());
        let events = database.list_capability_audit(10)?;
        assert_eq!(events[0].status, CapabilityAuditStatus::Failed);
        assert_eq!(
            events[0].error_classification.as_deref(),
            Some("capability_failure")
        );
        assert!(
            events
                .iter()
                .all(|event| !event.decision_reason.contains("private detail"))
        );
        Ok(())
    }

    struct CancellingCapability {
        descriptor: CapabilityDescriptor,
    }

    #[async_trait]
    impl CapabilityExecutor for CancellingCapability {
        fn descriptor(&self) -> &CapabilityDescriptor {
            &self.descriptor
        }

        async fn execute(
            &self,
            _request: &CapabilityRequest,
            cancellation: CancellationToken,
        ) -> std::result::Result<Value, CapabilityError> {
            cancellation.cancel();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_during_execution_is_audited() -> Result<()> {
        let (_temp, database) = database()?;
        let capability = test_capability(CapabilityEffect::ReadOnly);
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(CancellingCapability {
            descriptor: capability.descriptor,
        }))?;
        let result = orchestrator
            .run(
                &database,
                &test_request(),
                None,
                100,
                CancellationToken::new(),
            )
            .await?;
        assert!(matches!(result, CapabilityRunResult::Cancelled { .. }));
        assert_eq!(
            database.list_capability_audit(10)?[0].status,
            CapabilityAuditStatus::Cancelled
        );
        Ok(())
    }

    #[test]
    fn registration_rejects_mutating_capabilities() {
        let capability = test_capability(CapabilityEffect::Mutating);
        let mut orchestrator = CapabilityOrchestrator::default();
        assert!(orchestrator.register(Box::new(capability)).is_err());
        assert_eq!(orchestrator.discover().count(), 0);
    }

    #[tokio::test]
    async fn invalid_output_is_not_reported_as_success_and_is_audited() -> Result<()> {
        let (_temp, database) = database()?;
        let mut capability = test_capability(CapabilityEffect::ReadOnly);
        capability.output = json!({"unexpected": "value"});
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        let result = orchestrator
            .run(
                &database,
                &test_request(),
                None,
                100,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());
        let events = database.list_capability_audit(10)?;
        assert_eq!(events[0].status, CapabilityAuditStatus::Failed);
        assert_eq!(
            events[0].error_classification.as_deref(),
            Some("invalid_output")
        );
        Ok(())
    }

    #[tokio::test]
    async fn undeclared_operation_is_denied_and_audited() -> Result<()> {
        let (_temp, database) = database()?;
        let capability = test_capability(CapabilityEffect::ReadOnly);
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        let mut request = test_request();
        request.operation = "write".to_owned();
        let result = orchestrator
            .run(&database, &request, None, 100, CancellationToken::new())
            .await?;
        assert!(matches!(result, CapabilityRunResult::Denied { .. }));
        assert_eq!(
            database.list_capability_audit(10)?[0].status,
            CapabilityAuditStatus::Denied
        );
        Ok(())
    }

    #[tokio::test]
    async fn approval_is_required_and_rechecked_before_execution() -> Result<()> {
        let (_temp, database) = database()?;
        database.set_permission_profile(PermissionProfile::Restricted, 100)?;
        let grant = PermissionGrantDraft {
            capability_id: "test.reader".to_owned(),
            capability_version: "1.0.0".to_owned(),
            operation: "read".to_owned(),
            permissions: vec!["data.read".to_owned()],
            requires_approval: false,
            expires_at: None,
        };
        database.create_permission_grant(&grant, 100)?;
        let capability = test_capability(CapabilityEffect::ReadOnly);
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        let request = test_request();
        let pending = orchestrator
            .run(&database, &request, None, 101, CancellationToken::new())
            .await?;
        let CapabilityRunResult::AwaitingApproval { approval_id, .. } = pending else {
            panic!("Restricted mode must require explicit approval");
        };
        assert_eq!(
            database.list_approvals(102)?[0].state,
            ApprovalState::Pending
        );
        database.resolve_approval(approval_id, true, 103)?;
        let completed = orchestrator
            .run(
                &database,
                &request,
                Some(approval_id),
                104,
                CancellationToken::new(),
            )
            .await?;
        assert!(
            matches!(completed, CapabilityRunResult::Completed { .. }),
            "expected approved replacement to complete, got {completed:?}"
        );
        assert_eq!(
            database.list_approvals(105)?[0].state,
            ApprovalState::Consumed
        );
        Ok(())
    }

    #[tokio::test]
    async fn workspace_approval_cannot_be_reused_for_another_path() -> Result<()> {
        let (_db_temp, database) = database()?;
        database.set_permission_profile(PermissionProfile::Restricted, 100)?;
        database.create_permission_grant(
            &PermissionGrantDraft {
                capability_id: "workspace.summary".to_owned(),
                capability_version: "1.0.0".to_owned(),
                operation: "summarize".to_owned(),
                permissions: vec!["workspace.read".to_owned()],
                requires_approval: false,
                expires_at: None,
            },
            100,
        )?;
        let first = tempdir()?;
        let second = tempdir()?;
        let first_root = crate::repository::normalize_workspace(first.path())?;
        let second_root = crate::repository::normalize_workspace(second.path())?;
        let capability = workspace_summary_capability(first_root);
        let request = capability.request();
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        let pending = orchestrator
            .run(&database, &request, None, 101, CancellationToken::new())
            .await?;
        let CapabilityRunResult::AwaitingApproval { approval_id, .. } = pending else {
            panic!("Restricted mode must require approval");
        };
        database.resolve_approval(approval_id, true, 102)?;
        let mut changed_request = request;
        changed_request.input["workspace"] = json!(second_root.to_string_lossy());
        let result = orchestrator
            .run(
                &database,
                &changed_request,
                Some(approval_id),
                103,
                CancellationToken::new(),
            )
            .await?;
        assert!(matches!(result, CapabilityRunResult::Denied { .. }));
        assert_eq!(
            database.list_capability_audit(10)?[0].status,
            CapabilityAuditStatus::Denied
        );
        Ok(())
    }

    #[tokio::test]
    async fn revoked_grant_denies_before_executor_is_called() -> Result<()> {
        let (_temp, database) = database()?;
        database.set_permission_profile(PermissionProfile::Controlled, 100)?;
        let grant_id = database.create_permission_grant(
            &PermissionGrantDraft {
                capability_id: "test.reader".to_owned(),
                capability_version: "1.0.0".to_owned(),
                operation: "read".to_owned(),
                permissions: vec!["data.read".to_owned()],
                requires_approval: false,
                expires_at: None,
            },
            100,
        )?;
        let calls = Arc::new(AtomicUsize::new(0));
        let capability = CountingCapability::new(calls.clone());
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        database.revoke_permission_grant(grant_id, 101)?;
        let result = orchestrator
            .run(
                &database,
                &test_request(),
                None,
                102,
                CancellationToken::new(),
            )
            .await?;
        assert!(matches!(result, CapabilityRunResult::Denied { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    struct CountingCapability {
        descriptor: CapabilityDescriptor,
        calls: Arc<AtomicUsize>,
    }

    impl CountingCapability {
        fn new(calls: Arc<AtomicUsize>) -> Self {
            let capability = test_capability(CapabilityEffect::ReadOnly);
            Self {
                descriptor: capability.descriptor,
                calls,
            }
        }
    }

    #[async_trait]
    impl CapabilityExecutor for CountingCapability {
        fn descriptor(&self) -> &CapabilityDescriptor {
            &self.descriptor
        }

        async fn execute(
            &self,
            _request: &CapabilityRequest,
            _cancellation: CancellationToken,
        ) -> std::result::Result<Value, CapabilityError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"result": "read-only"}))
        }
    }

    #[tokio::test]
    async fn workspace_capability_is_bounded_and_does_not_read_file_contents() -> Result<()> {
        let temp = tempdir()?;
        fs::write(temp.path().join("Cargo.toml"), "private content")?;
        let workspace = crate::repository::normalize_workspace(temp.path())?;
        let capability = workspace_summary_capability(workspace);
        let mut orchestrator = CapabilityOrchestrator::default();
        let request = capability.request();
        orchestrator.register(Box::new(capability))?;
        let (_db_temp, database) = database()?;
        let result = orchestrator
            .run(&database, &request, None, 100, CancellationToken::new())
            .await?;
        let CapabilityRunResult::Completed { output, .. } = result else {
            panic!("Open mode should execute the read-only workspace summary");
        };
        assert_eq!(output["file_count"], 1);
        assert_eq!(output["manifests"], json!(["Cargo.toml"]));
        assert!(!output.to_string().contains("private content"));
        Ok(())
    }

    #[tokio::test]
    async fn workspace_replacement_in_open_profile_still_requires_grant_and_exact_approval()
    -> Result<()> {
        let workspace_temp = tempdir()?;
        fs::write(workspace_temp.path().join("src.txt"), "alpha = 1\n")?;
        let workspace = crate::repository::normalize_workspace(workspace_temp.path())?;
        let proposal = repository::prepare_workspace_replacement(
            &workspace,
            "src.txt",
            "alpha = 1",
            "alpha = 2",
        )?;
        let capability = workspace_file_replace_capability(workspace);
        let request = capability.request(
            "src.txt".to_owned(),
            proposal.expected_sha256,
            "alpha = 1".to_owned(),
            "alpha = 2".to_owned(),
        );
        let mut orchestrator = CapabilityOrchestrator::default();
        orchestrator.register(Box::new(capability))?;
        let (_db_temp, database) = database()?;
        database.create_permission_grant(
            &PermissionGrantDraft {
                capability_id: "workspace.file-replace".to_owned(),
                capability_version: "1.0.0".to_owned(),
                operation: "replace".to_owned(),
                permissions: vec!["workspace.write".to_owned()],
                requires_approval: false,
                expires_at: None,
            },
            100,
        )?;
        let pending = orchestrator
            .run(&database, &request, None, 101, CancellationToken::new())
            .await?;
        let CapabilityRunResult::AwaitingApproval { approval_id, .. } = pending else {
            panic!("Mutating operations require approval even in Open profile");
        };
        assert_eq!(
            fs::read_to_string(workspace_temp.path().join("src.txt"))?,
            "alpha = 1\n"
        );
        database.resolve_approval(approval_id, true, 102)?;
        let completed = orchestrator
            .run(
                &database,
                &request,
                Some(approval_id),
                103,
                CancellationToken::new(),
            )
            .await?;
        assert!(
            matches!(completed, CapabilityRunResult::Completed { .. }),
            "expected approved replacement to complete, got {completed:?}"
        );
        assert_eq!(
            fs::read_to_string(workspace_temp.path().join("src.txt"))?,
            "alpha = 2\n"
        );
        Ok(())
    }
}
