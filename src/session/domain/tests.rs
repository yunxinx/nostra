use serde_json::json;

use super::*;

#[test]
fn ids_use_domain_prefix_and_uuid_v7() {
    let id = SessionId::new(SessionDomain::Chat);
    let encoded = id.to_string();
    assert!(encoded.starts_with("chat-"));
    assert_eq!(encoded.parse::<SessionId>().expect("valid id"), id);
    assert!("chat-not-a-uuid".parse::<SessionId>().is_err());
    assert!(
        "wrong-00000000-0000-7000-8000-000000000000"
            .parse::<SessionId>()
            .is_err()
    );
}

#[test]
fn header_validation_enforces_domain_project_pair() {
    let chat = SessionHeader::new(SessionDomain::Chat, None);
    assert!(chat.validate().is_ok());
    let agent = SessionHeader::new(
        SessionDomain::Agent,
        Some(ProjectIdentity::new("/tmp/project", "project")),
    );
    assert!(agent.validate().is_ok());
    assert!(
        SessionHeader::new(SessionDomain::Agent, None)
            .validate()
            .is_err()
    );
    assert!(
        SessionHeader::new(
            SessionDomain::Chat,
            Some(ProjectIdentity::new("/tmp/project", "project"))
        )
        .validate()
        .is_err()
    );
}

#[test]
fn tagged_entry_kinds_and_project_identity_round_trip() {
    let chat_id = SessionId::new(SessionDomain::Chat);
    let project = ProjectIdentity::new("/tmp/project", "project");
    let header = SessionHeader {
        format_version: CURRENT_FORMAT_VERSION,
        session_id: SessionId::new(SessionDomain::Agent),
        domain: SessionDomain::Agent,
        created_at: 1,
        project: Some(project.clone()),
        initial_model: Some(ModelSelection {
            profile_id: "provider".into(),
            model_id: "model".into(),
        }),
        initial_system_prompt: Some("system".into()),
    };
    let reference = ChatMessageRef::new(chat_id, EntryId::new()).expect("chat reference");
    let kinds = vec![
        SessionEntryKind::Header(header),
        SessionEntryKind::Message(MessageEntry {
            message: Message {
                role: crate::llm::Role::User,
                content: Vec::new(),
                provider_metadata: Default::default(),
            },
            turn_id: Some("turn".into()),
            model: None,
            usage: Usage::default(),
        }),
        SessionEntryKind::TurnResult(TurnResult {
            turn_id: Some("turn".into()),
            status: TurnStatus::Completed,
            finish_reason: Some(FinishReason::Stop),
            error: None,
            usage: Usage::default(),
        }),
        SessionEntryKind::ConfigChange(ConfigChange {
            model: ModelSelection {
                profile_id: "provider".into(),
                model_id: "model".into(),
            },
            system_prompt: Some("updated system".into()),
        }),
        SessionEntryKind::Compaction(Compaction {
            summary: "summary".into(),
            first_kept_entry_id: EntryId::new(),
            tokens_before: 10,
        }),
        SessionEntryKind::BranchSummary(BranchSummary {
            from_id: EntryId::new(),
            summary: "branch".into(),
        }),
        SessionEntryKind::TranscriptReplay(TranscriptReplay::ToolCall {
            call_id: "call-1".into(),
            tool_name: "read_file".into(),
            arguments: json!({"path": "src/lib.rs"}),
        }),
        SessionEntryKind::Reference(Reference {
            source: reference,
            label: Some("discussion".into()),
        }),
        SessionEntryKind::Leaf(Leaf {
            target_id: Some(EntryId::new()),
        }),
    ];

    for kind in kinds {
        let encoded = serde_json::to_value(&kind).expect("serialize");
        assert!(encoded["type"].is_string());
        assert!(encoded.get("payload").is_some());
        let decoded = serde_json::from_value::<SessionEntryKind>(encoded).expect("decode");
        assert_eq!(decoded, kind);
    }

    let project_json = serde_json::to_value(project).expect("serialize project");
    assert_eq!(project_json["display_name"], json!("project"));
    assert!(project_json["canonical_path"].is_string());
}

#[test]
fn project_identity_reuses_its_stable_id_when_location_changes() {
    let original = ProjectIdentity::new("/tmp/nostra-project", "Original");
    let moved = ProjectIdentity::from_parts(
        original.project_id.clone(),
        "/tmp/nostra-project-moved",
        "Renamed",
    )
    .expect("stable project identity");
    assert_eq!(moved.project_id, original.project_id);
    assert_ne!(moved.canonical_path, original.canonical_path);
    assert_eq!(moved.display_name, "Renamed");
}

#[test]
fn transcript_replay_rejects_missing_runtime_identifiers() {
    assert!(
        TranscriptReplay::ToolCall {
            call_id: "".into(),
            tool_name: "read_file".into(),
            arguments: json!({}),
        }
        .validate()
        .is_err()
    );
    assert!(
        TranscriptReplay::TerminalSnapshot {
            terminal_id: "".into(),
            title: None,
            content: String::new(),
        }
        .validate()
        .is_err()
    );
}

#[test]
fn unsafe_gateway_body_is_not_part_of_the_safe_error_projection() {
    let raw_body = r#"{"error":"sk-sensitive-value"}"#;
    let error = crate::llm::GatewayError::provider("Provider failed.", Some("safe-code".into()))
        .with_upstream_body(raw_body);
    let projected = SafeError::from_gateway(&error);
    let persisted = serde_json::to_string(&projected).expect("serialize");
    assert!(!persisted.contains("sk-sensitive-value"));
    assert_eq!(projected.message, "Provider failed.");
}

#[test]
fn unsupported_format_version_is_rejected() {
    let mut header = SessionHeader::new(SessionDomain::Chat, None);
    header.format_version = CURRENT_FORMAT_VERSION + 1;
    assert!(matches!(
        header.validate(),
        Err(SessionError::UnsupportedVersion(_))
    ));
}
