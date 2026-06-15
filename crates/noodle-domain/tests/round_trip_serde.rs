//! Round-trip serde tests for every public type in `noodle-domain`.
//!
//! For each family the test constructs a canonical value, encodes
//! it to JSON, decodes it back, and asserts equality. This proves
//! that no field is silently dropped, no variant name diverges, and
//! that consumers re-reading the JSON get exactly what was written.
//!
//! Tests are deliberately exhaustive over enum variants (including
//! `VendorSpecific`) so the recurrence rule (ADR 029 §3) is
//! exercised on the wire.

use std::collections::BTreeMap;

use noodle_domain::{
    capability::{Capability, VendorCapability},
    citation_ref::{CitationRef, VendorCitationRef},
    classifier::{ClassificationContext, ClassificationResult},
    content_category::{ContentCategory, VendorContentCategory},
    envelope_metadata::{Direction, EndpointPath, ProviderId, RoundTripIndex},
    observation_context::{
        AgentApp, AgentAppName, AgentAppSource, Architecture, CollectorApp, Machine, OsFamily,
    },
    principal_identity::{AccountRole, DeviceId, PrincipalIdentity},
    reminder_subtype::{ReminderSubtype, VendorReminderSubtype},
    speech_act::{SpeechAct, VendorSpeechAct},
    subscription_context::{
        AccountType, ApiKeyFingerprint, ApiKeyKind, ApiKeySource, OrganizationContext,
        SubscriptionTier, SubscriptionTierSource, TierLabel,
    },
    task_plan::{Constraint, ConstraintKind, Goal, PlanStep, TodoItem, TodoStatus},
    trust_level::{TrustLevel, VendorTrustLevel},
    turn_end::{TurnEnd, VendorTurnEnd},
    usage::{Latency, RetryCount, TokenUsage},
    vendor::{VendorId, VendorTag},
};
use semver::Version;
use serde::{Deserialize, Serialize};
use time::macros::datetime;

/// Helper: encode then decode, assert equality.
fn round_trip<T>(value: &T)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let decoded: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(value, &decoded, "round-trip mismatch via JSON: {json}");
}

#[test]
fn speech_act_round_trip() {
    let canonicals = [
        SpeechAct::Instruction,
        SpeechAct::Claim,
        SpeechAct::HedgedClaim,
        SpeechAct::Question,
        SpeechAct::Suggestion,
        SpeechAct::Acknowledgement,
        SpeechAct::Refusal,
        SpeechAct::Clarification,
    ];
    for v in &canonicals {
        round_trip(v);
    }
    round_trip(&SpeechAct::VendorSpecific(VendorSpeechAct {
        vendor: VendorId::Anthropic,
        tag: "anthropic.cache_control.ephemeral".into(),
        closest_canonical: Some("acknowledgement".into()),
    }));
}

#[test]
fn content_category_round_trip() {
    let canonicals = [
        ContentCategory::Code,
        ContentCategory::Command,
        ContentCategory::Credential,
        ContentCategory::Pii,
        ContentCategory::Secret,
        ContentCategory::Prose,
        ContentCategory::StructuredData,
        ContentCategory::Path,
        ContentCategory::Url,
        ContentCategory::Reasoning,
        ContentCategory::Plan,
    ];
    for v in &canonicals {
        round_trip(v);
    }
    round_trip(&ContentCategory::VendorSpecific(VendorContentCategory {
        vendor: VendorId::Other("acme".into()),
        tag: "acme.confidential".into(),
        closest_canonical: None,
    }));
}

#[test]
fn capability_round_trip() {
    let canonicals = [
        Capability::ReadFile,
        Capability::WriteFile,
        Capability::Execute,
        Capability::NetworkRequest,
        Capability::NetworkListen,
        Capability::SpawnAgent,
        Capability::SystemQuery,
        Capability::EnvironmentRead,
    ];
    for v in &canonicals {
        round_trip(v);
    }
    round_trip(&Capability::VendorSpecific(VendorCapability {
        vendor: VendorId::Openai,
        tag: "openai.code_interpreter".into(),
        closest_canonical: Some("execute".into()),
    }));
}

#[test]
fn trust_level_round_trip() {
    for v in [
        TrustLevel::SystemTrusted,
        TrustLevel::UserTrusted,
        TrustLevel::ModelOutput,
        TrustLevel::ToolOutput,
        TrustLevel::InjectedReminder,
    ] {
        round_trip(&v);
    }
    round_trip(&TrustLevel::VendorSpecific(VendorTrustLevel {
        vendor: VendorId::Google,
        tag: "google.system_safety".into(),
        closest_canonical: None,
    }));
}

#[test]
fn citation_ref_round_trip() {
    round_trip(&CitationRef::FilePath {
        path: "src/lib.rs".into(),
    });
    round_trip(&CitationRef::UrlReference {
        url: "https://example.com".into(),
    });
    round_trip(&CitationRef::LineRange {
        path: "src/main.rs".into(),
        start: 10,
        end: 42,
    });
    round_trip(&CitationRef::CommitHash {
        hash: "abc123".into(),
    });
    round_trip(&CitationRef::IssueRef {
        repository: Some("owner/repo".into()),
        number: 7,
    });
    round_trip(&CitationRef::VendorSpecific(VendorCitationRef {
        vendor: VendorId::Anthropic,
        tag: "anthropic.tool.web_search.result".into(),
        closest_canonical: Some("url_reference".into()),
        payload: Some(serde_json::json!({"snippet": "..."})),
    }));
}

#[test]
fn reminder_subtype_round_trip() {
    for v in [
        ReminderSubtype::SkillCatalogue,
        ReminderSubtype::ToolAvailability,
        ReminderSubtype::ContextRefresh,
        ReminderSubtype::WorkingDirState,
        ReminderSubtype::SafetyClassifier,
        ReminderSubtype::LongConversation,
    ] {
        round_trip(&v);
    }
    round_trip(&ReminderSubtype::VendorSpecific(VendorReminderSubtype {
        vendor: VendorId::Anthropic,
        tag: "anthropic.reminder.working_dir".into(),
        closest_canonical: Some("working_dir_state".into()),
    }));
}

#[test]
fn task_plan_round_trip() {
    round_trip(&TodoItem {
        id: Some("todo-1".into()),
        text: "Wire up codec".into(),
        status: TodoStatus::InProgress,
    });
    for status in [
        TodoStatus::Pending,
        TodoStatus::InProgress,
        TodoStatus::Completed,
        TodoStatus::Cancelled,
        TodoStatus::Blocked,
    ] {
        round_trip(&status);
    }
    round_trip(&PlanStep {
        order: 2,
        description: "Run tests".into(),
        depends_on: vec![1],
    });
    round_trip(&Goal {
        statement: "Ship S1".into(),
        success_criteria: vec!["clippy green".into(), "tests green".into()],
    });
    round_trip(&Constraint {
        statement: "No new deps".into(),
        kind: ConstraintKind::MustNotDo,
    });
    for kind in [
        ConstraintKind::MustDo,
        ConstraintKind::MustNotDo,
        ConstraintKind::Preference,
        ConstraintKind::Unknown,
    ] {
        round_trip(&kind);
    }
}

#[test]
fn turn_end_round_trip() {
    for v in [
        TurnEnd::EndTurn,
        TurnEnd::MaxTokens,
        TurnEnd::ToolUsePending,
        TurnEnd::StopSequence,
        TurnEnd::ContentFiltered,
    ] {
        round_trip(&v);
    }
    round_trip(&TurnEnd::VendorSpecific(VendorTurnEnd {
        vendor: VendorId::Anthropic,
        tag: "anthropic.stop_reason.pause_turn".into(),
        closest_canonical: Some("tool_use_pending".into()),
    }));
}

#[test]
fn envelope_metadata_round_trip() {
    for p in [
        ProviderId::Anthropic,
        ProviderId::Openai,
        ProviderId::Google,
        ProviderId::AwsBedrock,
        ProviderId::AzureOpenai,
        ProviderId::Cohere,
        ProviderId::Mistral,
        ProviderId::Other("acme-llm".into()),
    ] {
        round_trip(&p);
    }
    for d in [Direction::Request, Direction::Response] {
        round_trip(&d);
    }
    round_trip(&RoundTripIndex::new(42));
    round_trip(&EndpointPath::new("/v1/messages"));
}

#[test]
fn observation_context_round_trip() {
    round_trip(&AgentApp {
        name: AgentAppName::ClaudeCode,
        version: Some(Version::new(0, 5, 2)),
        build_hash: Some("abc123".into()),
        build_date: Some(datetime!(2026-05-23 12:00:00 UTC)),
        source: AgentAppSource::UserAgentHeader,
    });
    for n in [
        AgentAppName::ClaudeCode,
        AgentAppName::OpenCode,
        AgentAppName::Cursor,
        AgentAppName::ChatGptDesktop,
        AgentAppName::ClaudeDesktop,
        AgentAppName::CodexCli,
        AgentAppName::Warp,
        AgentAppName::Zed,
        AgentAppName::Unknown,
        AgentAppName::VendorSpecific("acme-cli".into()),
    ] {
        round_trip(&n);
    }
    round_trip(&Machine {
        hostname: Some("workstation.local".into()),
        os_family: OsFamily::Macos,
        os_version: Some("15.0".into()),
        architecture: Architecture::Aarch64,
        locale: Some("en_US.UTF-8".into()),
        timezone: Some("America/New_York".into()),
    });
    for f in [
        OsFamily::Macos,
        OsFamily::Linux,
        OsFamily::Windows,
        OsFamily::Unknown,
    ] {
        round_trip(&f);
    }
    for a in [
        Architecture::X86_64,
        Architecture::Aarch64,
        Architecture::Unknown,
    ] {
        round_trip(&a);
    }
    round_trip(&CollectorApp {
        name: "noodle".into(),
        version: Version::new(0, 0, 1),
        build_hash: "deadbeef".into(),
        build_date: datetime!(2026-05-23 12:00:00 UTC),
        features: vec!["tap".into(), "viewer".into()],
    });
}

#[test]
fn principal_identity_round_trip() {
    round_trip(&PrincipalIdentity {
        device_id: Some(DeviceId::new("dev-001")),
        machine_tag: Some("eng-laptop-7".into()),
        account_role: Some(AccountRole::StandardUser),
    });
    for r in [
        AccountRole::Admin,
        AccountRole::StandardUser,
        AccountRole::ServiceAccount,
        AccountRole::Unknown,
        AccountRole::VendorSpecific("contractor".into()),
    ] {
        round_trip(&r);
    }
    round_trip(&DeviceId::new("dev-002"));
}

#[test]
fn usage_round_trip() {
    let mut extras = BTreeMap::new();
    extras.insert(
        "server_tool_use_input_tokens".into(),
        serde_json::json!(123),
    );
    round_trip(&TokenUsage {
        input: 1024,
        output: 256,
        cached_read: Some(512),
        cached_creation: Some(128),
        reasoning: Some(64),
        vendor_extras: extras,
    });
    round_trip(&TokenUsage::default());
    round_trip(&Latency {
        time_to_first_byte_ms: Some(120),
        total_ms: Some(3500),
    });
    round_trip(&Latency::default());
    round_trip(&RetryCount {
        attempts: 2,
        last_error_kind: Some("overloaded_error".into()),
    });
    round_trip(&RetryCount::default());
}

#[test]
fn subscription_context_round_trip() {
    round_trip(&ApiKeyFingerprint {
        prefix: "sk-ant-api03-wcq".into(),
        kind: ApiKeyKind::ApiKey,
        source: ApiKeySource::AuthorizationHeader,
    });
    for k in [
        ApiKeyKind::ApiKey,
        ApiKeyKind::Session,
        ApiKeyKind::Oauth,
        ApiKeyKind::Unknown,
    ] {
        round_trip(&k);
    }
    for s in [
        ApiKeySource::AuthorizationHeader,
        ApiKeySource::XApiKey,
        ApiKeySource::SessionCookie,
        ApiKeySource::UrlParam,
    ] {
        round_trip(&s);
    }
    round_trip(&OrganizationContext {
        organization_id: Some("org_abc".into()),
        parent_organization_id: Some("org_root".into()),
        account_type: AccountType::Enterprise,
    });
    for a in [
        AccountType::Enterprise,
        AccountType::Personal,
        AccountType::Api,
        AccountType::Team,
        AccountType::Free,
        AccountType::Pro,
        AccountType::Other("custom".into()),
        AccountType::Unknown,
        AccountType::VendorSpecific("anthropic.partner".into()),
    ] {
        round_trip(&a);
    }
    round_trip(&SubscriptionTier {
        tier: Some(TierLabel::Pro),
        source: SubscriptionTierSource::Header,
    });
    for t in [
        TierLabel::Free,
        TierLabel::Pro,
        TierLabel::Team,
        TierLabel::Enterprise,
        TierLabel::Custom("growth".into()),
        TierLabel::Unknown,
    ] {
        round_trip(&t);
    }
    for s in [
        SubscriptionTierSource::Header,
        SubscriptionTierSource::UrlPath,
        SubscriptionTierSource::ResponseMetadata,
        SubscriptionTierSource::EmbellishmentPlane,
        SubscriptionTierSource::Unknown,
    ] {
        round_trip(&s);
    }
}

#[test]
fn vendor_id_round_trip() {
    for v in [
        VendorId::Anthropic,
        VendorId::Openai,
        VendorId::Google,
        VendorId::AwsBedrock,
        VendorId::AzureOpenai,
        VendorId::Cohere,
        VendorId::Mistral,
        VendorId::Other("acme".into()),
    ] {
        round_trip(&v);
    }
    round_trip(&VendorTag::new("acme.tag.x"));
}

#[test]
fn classifier_types_round_trip() {
    round_trip(&ClassificationContext {
        block_index: Some(3),
        source_hint: Some("system".into()),
        upstream_text: Some("previous turn".into()),
    });
    round_trip(&ClassificationContext::default());
    round_trip(&ClassificationResult {
        speech_act: Some(SpeechAct::Instruction),
        category: Some(ContentCategory::Code),
        citations: vec![CitationRef::FilePath {
            path: "src/foo.rs".into(),
        }],
        plan_items: vec![TodoItem {
            id: None,
            text: "do thing".into(),
            status: TodoStatus::Pending,
        }],
    });
    round_trip(&ClassificationResult::default());
}
